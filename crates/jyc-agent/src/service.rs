//! AgentService implementation using the in-process agent loop.
//!
//! Replaces the OpenCode HTTP/SSE client with direct LLM calls and tool execution.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing;

use jyc_core::agent::{AgentResult, AgentService};
use jyc_core::thread_event_bus::ThreadEventBusRef;
use jyc_types::{InboundMessage, QueueItem};

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
/// directly in-process (no external OpenCode server needed).
pub struct JycAgentService {
    config: AgentConfig,
    /// Per-thread event bus map.
    event_buses: Mutex<HashMap<String, ThreadEventBusRef>>,
    /// JYC workdir (for discovering global skills).
    workdir: PathBuf,
}

impl JycAgentService {
    /// Create a new agent service with the given configuration and workdir.
    pub fn new(config: AgentConfig, workdir: PathBuf) -> Self {
        Self {
            config,
            event_buses: Mutex::new(HashMap::new()),
            workdir,
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

        // Reply instructions
        prompt.push_str(
            "## Reply Instructions\n\
             When you have your answer ready, use the jyc_reply_reply_message tool:\n\
             - `message`: Your reply text\n\
             - `attachments`: Optional filenames to attach from the working directory\n\
             After a successful reply, STOP immediately. Do NOT call any other tools.\n\
             CRITICAL: Always use the jyc_reply_reply_message tool to send your reply.\n\n"
        );

        // Chat history access instructions
        prompt.push_str(
            "## Chat History\n\
             This thread maintains a chronological chat history in `chat_history_YYYY-MM-DD.md`.\n\
             You can read it with the `read` tool if you need context from prior conversations.\n"
        );

        prompt
    }

    /// Build the user prompt from an inbound message.
    fn build_user_prompt(&self, message: &InboundMessage) -> String {
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

    /// Create the tool registry for a thread.
    fn build_tool_registry(&self, _thread_path: &Path) -> ToolRegistry {
        // Start with all built-in tools
        let mut registry = crate::tools::builtin::create_builtin_registry();

        // Add MCP bridge tools (reply_message, etc.)
        crate::tools::mcp_bridge::register_mcp_tools(&mut registry);

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

        // 3. Load session and prior raw context
        let (prior_history, prior_raw_context) = session::load_context(thread_path).await;

        tracing::debug!(
            prior_messages = prior_history.len(),
            prior_raw_context = prior_raw_context.len(),
            "Loaded prior context"
        );

        // 4. Build prompts
        let system_prompt = self.build_system_prompt(thread_path);
        let user_prompt = self.build_user_prompt(message);

        // 5. Build tool registry
        let tools = self.build_tool_registry(thread_path);

        // 6. Get event bus for this thread
        let event_bus = self.get_event_bus(thread_name).await;

        // 7. Run agent loop
        let result = agent_loop::run(AgentLoopConfig {
            provider: provider.as_ref(),
            tools: &tools,
            system_prompt: &system_prompt,
            user_message: &user_prompt,
            working_dir: thread_path,
            cancel: thread_cancel,
            thread_name,
            event_bus: event_bus.as_ref(),
            prior_history,
            prior_raw_context,
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
        session::update_tokens(thread_path, result.input_tokens, result.output_tokens, context_window).await;

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
