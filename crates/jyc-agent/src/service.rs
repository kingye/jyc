//! AgentService implementation using the in-process agent loop.
//!
//! Uses direct LLM calls and tool execution instead of external server.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing;

use jyc_core::agent::{AgentResult, AgentService};
use jyc_core::thread_event_bus::ThreadEventBusRef;
use jyc_types::{InboundMessage, QueueItem, McpServerConfig, ChannelPattern};

use crate::agent_loop::{self, AgentLoopConfig};
use crate::provider;
use crate::session;
use crate::tools::registry::ToolRegistry;
use crate::types::AgentConfig;

/// Metadata for a discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    /// Skill name (e.g., "coding-principles")
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Path to the skill's directory (contains SKILL.md)
    pub source_path: PathBuf,
}

/// Parse frontmatter from a SKILL.md file.
///
/// Frontmatter is delimited by `---` lines. Supports `name:` and `description:` fields.
/// Returns `None` if the file has no valid frontmatter or missing required fields.
pub fn parse_skill_frontmatter(content: &str) -> Option<SkillMeta> {
    let mut lines = content.lines();

    // First line must be "---"
    if lines.next()?.trim() != "---" {
        return None;
    }

    let mut name = None;
    let mut description = None;

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        // End of frontmatter
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("name:") {
            name = Some(value.trim().to_string());
        } else if let Some(value) = trimmed.strip_prefix("description:") {
            let val = value.trim();
            if val == "|" || val == "|-" || val == ">" {
                // YAML block scalar: collect indented lines until --- or non-indented line
                let mut desc = String::new();
                while let Some(line) = lines.next() {
                    let trimmed = line.trim();
                    if trimmed == "---" {
                        // Put back the --- terminator so the outer loop can handle it
                        // Actually we've already consumed it; just break
                        break;
                    }
                    if !trimmed.is_empty() {
                        if !desc.is_empty() {
                            desc.push(' ');
                        }
                        desc.push_str(trimmed);
                    }
                }
                description = Some(desc);
            } else if !val.is_empty() {
                description = Some(val.to_string());
            }
        }
    }

    let name = name?;
    let description = description?;
    if name.is_empty() || description.is_empty() {
        return None;
    }

    Some(SkillMeta {
        name,
        description,
        source_path: PathBuf::new(), // caller fills this in
    })
}

/// In-process AI agent service.
///
/// Implements `AgentService` by running LLM inference and tool execution
/// directly in-process.
pub struct JycAgentService {
    config: AgentConfig,
    /// Per-thread event bus map.
    event_buses: Mutex<HashMap<String, ThreadEventBusRef>>,
    /// JYC workdir (for discovering global skills).
    workdir: PathBuf,
    /// MCP server configurations for dynamic tool loading.
    mcp_configs: Vec<McpServerConfig>,
    /// Channel patterns flattened from `[[channels.<name>.patterns]]`. Used
    /// to look up per-pattern agent runtime flags (e.g.
    /// `inject_inbound_images`) and per-pattern attachment configuration
    /// by `InboundMessage.matched_pattern`.
    patterns: Vec<ChannelPattern>,
    /// Global `[attachments.inbound]` config (used as fallback when a matched
    /// pattern does not specify its own `attachments`).
    global_inbound_attachments: Option<jyc_types::InboundAttachmentConfig>,
}

impl JycAgentService {
    /// Create a new agent service with the given configuration, workdir,
    /// MCP configs, channel patterns, and global inbound-attachment config.
    pub fn new(
        config: AgentConfig,
        workdir: PathBuf,
        mcp_configs: Vec<McpServerConfig>,
        patterns: Vec<ChannelPattern>,
        global_inbound_attachments: Option<jyc_types::InboundAttachmentConfig>,
    ) -> Self {
        Self {
            config,
            event_buses: Mutex::new(HashMap::new()),
            workdir,
            mcp_configs,
            patterns,
            global_inbound_attachments,
        }
    }

    /// Discover skills from multiple paths, with priority-based deduplication.
    ///
    /// Scans paths from lowest to highest priority (later paths override earlier ones
    /// when skills share the same name).
    pub fn discover_skills(&self, thread_path: &Path) -> Vec<SkillMeta> {
        let mut skills: HashMap<String, SkillMeta> = HashMap::new();

        // Build scan paths from low to high priority
        let scan_paths: Vec<PathBuf> = {
            let mut paths = Vec::new();

            // $HOME/.config/opencode/skills/
            if let Ok(home) = std::env::var("HOME") {
                paths.push(PathBuf::from(&home).join(".config/opencode/skills"));
                // $HOME/.claude/skills/
                paths.push(PathBuf::from(&home).join(".claude/skills"));
            }

            // {jyc-data}/skills/ (via workdir)
            paths.push(self.workdir.join("skills"));

            // {thread_path}/repo/.claude/skills/
            paths.push(thread_path.join("repo/.claude/skills"));
            // {thread_path}/repo/.opencode/skills/
            paths.push(thread_path.join("repo/.opencode/skills"));
            // {thread_path}/repo/.jyc/skills/
            paths.push(thread_path.join("repo/.jyc/skills"));

            // {thread_path}/.claude/skills/
            paths.push(thread_path.join(".claude/skills"));
            // {thread_path}/.opencode/skills/
            paths.push(thread_path.join(".opencode/skills"));
            // {thread_path}/.jyc/skills/
            paths.push(thread_path.join(".jyc/skills"));

            paths
        };

        for scan_dir in &scan_paths {
            if !scan_dir.exists() || !scan_dir.is_dir() {
                continue;
            }

            let entries = match std::fs::read_dir(scan_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for entry in entries.flatten() {
                let skill_dir = entry.path();
                if !skill_dir.is_dir() {
                    continue;
                }

                let skill_md = skill_dir.join("SKILL.md");
                if !skill_md.exists() {
                    continue;
                }

                let content = match std::fs::read_to_string(&skill_md) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                if let Some(mut meta) = parse_skill_frontmatter(&content) {
                    meta.source_path = skill_dir;
                    // HashMap insert: later (higher-priority) paths overwrite earlier ones
                    skills.insert(meta.name.clone(), meta);
                }
            }
        }

        let mut result: Vec<SkillMeta> = skills.into_values().collect();
        // Sort by name for deterministic output
        result.sort_by(|a, b| a.name.cmp(&b.name));

        tracing::info!(
            thread_path = %thread_path.display(),
            skills = ?result.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            "Discovered {} skill(s)", result.len()
        );

        result
    }

    /// Build the system prompt for a thread.
    fn build_system_prompt(&self, thread_path: &Path) -> String {
        let mut prompt = String::new();

        // Security: directory boundaries
        prompt.push_str(&format!(
            "Your working directory is \"{}\". You MUST only read, write, and access files within this directory.\n\n",
            thread_path.display()
        ));

        // Load AGENTS.md if present in the working directory
        let agents_md = thread_path.join("AGENTS.md");
        if agents_md.exists() {
            if let Ok(content) = std::fs::read_to_string(&agents_md) {
                prompt.push_str("## Project Instructions (from AGENTS.md)\n\n");
                prompt.push_str(&content);
                prompt.push_str("\n\n");
            }
        }

        // Also check repo/AGENTS.md (common for GitHub threads)
        let repo_agents_md = thread_path.join("repo").join("AGENTS.md");
        if repo_agents_md.exists() {
            if let Ok(content) = std::fs::read_to_string(&repo_agents_md) {
                prompt.push_str("## Repository Instructions (from repo/AGENTS.md)\n\n");
                prompt.push_str(&content);
                prompt.push_str("\n\n");
            }
        }

        // Discover and inject skill metadata
        let skills = self.discover_skills(thread_path);
        if !skills.is_empty() {
            prompt.push_str(&format_skills_section(&skills));
        }

        // Persist skill names to .jyc/skills.json for dashboard inspection
        let skill_names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        if let Err(e) = persist_skill_names(thread_path, &skill_names) {
            tracing::warn!(error = %e, "Failed to persist skill names to skills.json");
        }

        // Reply instructions
        prompt.push_str(
            "## Reply Instructions\n\
             When you have your answer ready, use the jyc_reply_reply_message tool:\n\
             - `message`: Your reply text\n\
             - `attachments`: Optional filenames to attach from the working directory\n\
             After a successful reply, STOP immediately. Do NOT call any other tools.\n\
             CRITICAL: Always use the jyc_reply_reply_message tool to send your reply.\n\n\
             For long-running tasks, send periodic progress replies so the user knows you're\n\
             still working. Each reply marks a checkpoint; you can continue working after sending one.\n\n"
        );

        // Chat history access instructions
        prompt.push_str(
            "## Chat History\n\
             This thread maintains a chronological chat history in `chat_history_YYYY-MM-DD.md`.\n\
             You can read it with the `read` tool if you need context from prior conversations.\n"
        );

        prompt
    }

    /// Build the user prompt text (header + body) from an inbound message.
    fn build_user_prompt_text(&self, message: &InboundMessage) -> String {
        let mut prompt = String::new();

        prompt.push_str("## Incoming Message\n");
        prompt.push_str(&format!("**From:** {} <{}>\n", message.sender, message.sender_address));
        prompt.push_str(&format!("**Subject:** {}\n", message.topic));
        prompt.push_str(&format!("**Date:** {}\n\n", message.timestamp.to_rfc3339()));

        // Body
        let body = message
            .content
            .text
            .as_deref()
            .or(message.content.markdown.as_deref())
            .unwrap_or("[no text content]");

        prompt.push_str(body);
        prompt
    }

    /// Resolve the additional absolute read-roots for tools that enforce a
    /// path boundary. Returns at most one root: the resolved attachment
    /// save directory (per-pattern override beats global) when it points
    /// outside `thread_path`. Relative values resolve inside `thread_path`
    /// and need no widening.
    ///
    /// Reuses `jyc_core::attachment_storage::resolve_attachment_save_dir`
    /// so the agent's boundary rule never drifts from the channel adapters'
    /// save-location rule.
    fn resolve_additional_read_roots(
        &self,
        message: &InboundMessage,
        thread_path: &Path,
    ) -> Vec<PathBuf> {
        let pattern_cfg = message.matched_pattern.as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .and_then(|p| p.attachments.as_ref());
        let cfg = pattern_cfg.or(self.global_inbound_attachments.as_ref());

        let resolved = jyc_core::attachment_storage::resolve_attachment_save_dir(thread_path, cfg);

        if resolved.starts_with(thread_path) {
            Vec::new()
        } else {
            vec![resolved]
        }
    }

    /// Build the user-turn content blocks from an inbound message.
    ///
    /// Always emits a leading text block (header + body). When the active
    /// model has `supports_images = true` AND the matched pattern has
    /// `inject_inbound_images = true`, also appends one `ContentBlock::Image`
    /// per `image/*` attachment, base64-encoded inline from
    /// `MessageAttachment.saved_path`.
    ///
    /// Skips attachments without a `saved_path` (download failed) and logs
    /// (but does not fail) on read errors so transient I/O issues degrade
    /// gracefully into the text-only path.
    fn build_user_blocks(
        &self,
        message: &InboundMessage,
        supports_images: bool,
    ) -> Vec<crate::types::ContentBlock> {
        use crate::types::{ContentBlock, ImageSource};
        use base64::Engine as _;

        let mut blocks = vec![ContentBlock::Text {
            text: self.build_user_prompt_text(message),
        }];

        // Per-pattern opt-in. Default false when the message did not match a
        // pattern or the pattern is not in our flattened list.
        let pattern_inject = message.matched_pattern.as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .map(|p| p.inject_inbound_images)
            .unwrap_or(false);

        if !(supports_images && pattern_inject) {
            return blocks;
        }

        let mut injected = 0usize;
        for att in &message.attachments {
            if !att.content_type.starts_with("image/") {
                continue;
            }
            let Some(saved) = att.saved_path.as_ref() else {
                tracing::debug!(
                    filename = %att.filename,
                    "Image attachment has no saved_path; skipping injection"
                );
                continue;
            };
            match std::fs::read(saved) {
                Ok(bytes) => {
                    blocks.push(ContentBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: att.content_type.clone(),
                            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                        },
                    });
                    injected += 1;
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    path = %saved.display(),
                    "Failed to read image attachment for injection; skipping"
                ),
            }
        }

        if injected > 0 {
            tracing::info!(
                count = injected,
                pattern = ?message.matched_pattern,
                "Injected inbound image attachments into user turn"
            );
        }
        blocks
    }

    /// Create the tool registry for a thread.
    ///
    /// `supports_images` gates the `read_image` built-in: when the active
    /// model can accept image content blocks, the agent gets a way to load
    /// local files or URLs into subsequent user turns. When the model is
    /// text-only, the tool is omitted to keep the schema honest (no point
    /// advertising a capability the model can't act on).
    async fn build_tool_registry(&self, _thread_path: &Path, supports_images: bool) -> ToolRegistry {
        // Start with all built-in tools
        let mut registry = crate::tools::builtin::create_builtin_registry();

        // Image-loading built-in (only when the model accepts images).
        if supports_images {
            crate::tools::builtin::register_read_image(&mut registry);
        }

        // Add MCP bridge tools (reply_message, etc.)
        crate::tools::mcp_bridge::register_mcp_tools(&mut registry);

        // Load external MCP tools from config.toml [[mcps]]
        if !self.mcp_configs.is_empty() {
            tracing::info!(
                mcp_count = self.mcp_configs.len(),
                "Loading external MCP tools"
            );
            let mcp_tools = crate::tools::mcp_client::load_mcp_tools(&self.mcp_configs).await;
            for tool in mcp_tools {
                registry.register(tool);
            }
        }

        registry
    }

    /// Get or create the provider for the current model.
    fn create_provider(&self, model_override: Option<&str>) -> Result<Box<dyn provider::Provider>> {
        let model = model_override
            .or(self.config.model.as_deref())
            .ok_or_else(|| anyhow::anyhow!("No model configured. Set [agent].model in config.toml"))?;

        provider::create_provider(model, &self.config.providers)
    }

    /// Get event bus for a thread.
    async fn get_event_bus(&self, thread_name: &str) -> Option<ThreadEventBusRef> {
        self.event_buses.lock().await.get(thread_name).cloned()
    }
}

/// Persist skill names to the thread's .jyc/skills.json file.
///
/// This allows the dashboard to read the skills list without re-scanning directories.
fn persist_skill_names(thread_path: &Path, skill_names: &[&str]) -> Result<()> {
    let jyc_dir = thread_path.join(".jyc");
    std::fs::create_dir_all(&jyc_dir)
        .with_context(|| format!("Failed to create .jyc dir: {}", jyc_dir.display()))?;
    let skills_path = jyc_dir.join("skills.json");
    let json = serde_json::to_string_pretty(skill_names)?;
    std::fs::write(&skills_path, json)
        .with_context(|| format!("Failed to write skills.json: {}", skills_path.display()))?;
    Ok(())
}

/// Format the skills section for inclusion in the system prompt.
///
/// Produces a markdown-formatted list of available skills with their paths.
/// Returns an empty string if the skills list is empty.
pub fn format_skills_section(skills: &[SkillMeta]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut section = String::new();
    section.push_str("## Available Skills\n\n");

    for skill in skills {
        section.push_str(&format!(
            "- **{}** (at {})\n  {}\n\n",
            skill.name,
            skill.source_path.display(),
            skill.description
        ));
    }

    section.push_str(
        "To load a skill's full instructions, use `read <skill-path>/SKILL.md`.\n\
         All file paths within a SKILL.md are relative to that skill's directory.\n\
         When running skill scripts: cd <skill-path> && <command>\n\n"
    );

    section
}

#[async_trait]
impl AgentService for JycAgentService {
    async fn base_url(&self) -> Result<String> {
        // Not applicable for in-process agent
        Ok("in-process".to_string())
    }

    async fn process(
        &self,
        message: &InboundMessage,
        thread_name: &str,
        thread_path: &Path,
        message_dir: &str,
        _pending_rx: &mut mpsc::Receiver<QueueItem>,
        thread_cancel: CancellationToken,
    ) -> Result<AgentResult> {
        tracing::info!(
            thread = %thread_name,
            message_dir = %message_dir,
            "Processing message with in-process agent"
        );

        // 1. Read model override if present
        let model_override_path = thread_path.join(".jyc").join("model-override");
        let model_override = if model_override_path.exists() {
            tokio::fs::read_to_string(&model_override_path)
                .await
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        } else {
            None
        };

        // 2. Create provider
        let provider = self.create_provider(model_override.as_deref())
            .context("Failed to create LLM provider")?;

        tracing::info!(
            provider = %provider.name(),
            model = %provider.model(),
            "Using provider"
        );

        // 2b. Optionally create the small provider for ancillary calls
        //     (cycle-boundary progress summary, between-message context
        //     reset). Construction failures are non-fatal — log a warning
        //     and fall back to the main provider for those calls.
        let small_provider: Option<Box<dyn provider::Provider>> = self.config.small_model
            .as_deref()
            .and_then(|m| match provider::create_provider(m, &self.config.providers) {
                Ok(p) => {
                    tracing::info!(
                        small_provider = %p.name(),
                        small_model = %p.model(),
                        "Using small model for ancillary LLM calls"
                    );
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        small_model = m,
                        "Failed to construct small_model provider; falling back to main model"
                    );
                    None
                }
            });

        // 3. Load session and prior raw context
        let (prior_history, prior_raw_context) = session::load_context(thread_path).await;

        tracing::debug!(
            prior_messages = prior_history.len(),
            prior_raw_context = prior_raw_context.len(),
            "Loaded prior context"
        );

        // 4. Build prompts (image-injection gated by per-pattern flag and
        //    per-model `supports_images`)
        let system_prompt = self.build_system_prompt(thread_path);
        let user_blocks = self.build_user_blocks(message, provider.supports_images());

        // 5. Build tool registry
        let tools = self.build_tool_registry(thread_path, provider.supports_images()).await;

        // 6. Get event bus for this thread
        let event_bus = self.get_event_bus(thread_name).await;

        // 7. Run agent loop
        let additional_read_roots = self.resolve_additional_read_roots(message, thread_path);
        let result = agent_loop::run(AgentLoopConfig {
            provider: provider.as_ref(),
            small_provider: small_provider.as_deref().map(|p| p as &dyn provider::Provider),
            tools: &tools,
            system_prompt: &system_prompt,
            user_blocks,
            working_dir: thread_path,
            cancel: thread_cancel,
            thread_name,
            event_bus: event_bus.as_ref(),
            prior_history,
            prior_raw_context,
            max_iterations: Some(self.config.max_iterations),
            additional_read_roots,
        })
        .await?;

        tracing::info!(
            reply_sent_by_tool = result.reply_sent_by_tool,
            text_len = result.text.len(),
            input_tokens = result.input_tokens,
            output_tokens = result.output_tokens,
            "Agent loop completed"
        );

        // 8. Save raw context (preserves provider-specific fields for round-tripping)
        session::save_raw_context(thread_path, &result.raw_context).await;

        // 9. Update session token tracking
        // Resolve context_window: per-model override > provider default
        let model_str = model_override.as_deref()
            .or(self.config.model.as_deref())
            .unwrap_or("");
        let context_window = if let Some((provider_name, model_id)) = model_str.split_once('/') {
            self.config.providers.get(provider_name).and_then(|p| {
                // Check per-model override first, then provider default
                p.models.get(model_id)
                    .and_then(|m| m.context_window)
                    .or(p.context_window)
            })
        } else {
            None
        };
        // Provider used for the between-message context-reset summary (when
        // input_tokens crosses the 95 % auto-reset threshold). Same fallback
        // rule as the cycle-boundary summary: small_model if configured,
        // else the main model.
        let summary_provider: &dyn provider::Provider = small_provider
            .as_deref()
            .map(|p| p as &dyn provider::Provider)
            .unwrap_or(provider.as_ref());
        session::update_tokens(
            thread_path,
            result.input_tokens,
            result.output_tokens,
            context_window,
            summary_provider,
        ).await;

        // 9. Return result
        if result.reply_sent_by_tool {
            Ok(AgentResult {
                reply_sent_by_tool: true,
                reply_text: result.reply_text_from_tool,
            })
        } else {
            Ok(AgentResult {
                reply_sent_by_tool: false,
                reply_text: if result.text.is_empty() { None } else { Some(result.text) },
            })
        }
    }

    async fn set_thread_event_bus(&self, thread_name: &str, event_bus: Option<ThreadEventBusRef>) {
        let mut buses = self.event_buses.lock().await;
        match event_bus {
            Some(bus) => { buses.insert(thread_name.to_string(), bus); }
            None => { buses.remove(thread_name); }
        }
    }
}
