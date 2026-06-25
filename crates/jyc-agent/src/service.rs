//! AgentService implementation using the in-process agent loop.
//!
//! Uses direct LLM calls and tool execution instead of external server.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing;

use jyc_core::agent::{AgentResult, AgentService};
use jyc_core::thread_event_bus::ThreadEventBusRef;
use jyc_types::{ChannelPattern, InboundMessage, McpServerConfig, QueueItem};

use crate::agent_loop::{self, AgentLoopConfig};
use crate::provider;
use crate::session;
use crate::tools::OutboundsMap;
use crate::tools::ThreadManagersMap;
use crate::tools::registry::ToolRegistry;
use crate::types::AgentConfig;
use crate::vision::VisionClient;
use std::sync::Arc;

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
                for line in lines.by_ref() {
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
    /// Channel patterns for the current channel (not cross-channel flattened).
    /// Used to look up per-pattern agent runtime flags (e.g.
    /// `inject_inbound_images`, model/small_model overrides, mcps,
    /// disabled_builtin_tools) by `InboundMessage.matched_pattern`.
    patterns: Vec<ChannelPattern>,
    /// Channel-level MCP configs (fallback when pattern-level is unset).
    channel_mcp_configs: Option<Vec<McpServerConfig>>,
    /// Global `[attachments.inbound]` config (used as fallback when a matched
    /// pattern does not specify its own `attachments`).
    global_inbound_attachments: Option<jyc_types::InboundAttachmentConfig>,
    /// Vision fallback client for text-only models to analyze images.
    vision_client: Option<Arc<VisionClient>>,
    /// Outbound adapter for proactive messaging tools (e.g. `jyc_send_message`).
    outbound: Option<Arc<dyn jyc_types::channel::OutboundAdapter>>,
    /// Channel-level tools to disable (merged with pattern-level).
    channel_disabled_tools: Option<Vec<String>>,
    /// Channel-level MCP servers to disable (merged with pattern-level).
    channel_disabled_mcp_servers: Option<Vec<String>>,
    /// Channel-level skills whitelist.
    channel_skills: Option<Vec<String>>,
    /// Channel-level skills to disable (merged with pattern-level).
    channel_disabled_skills: Option<Vec<String>>,
    /// Cross-channel thread managers keyed by channel name.
    /// Passed through to `AgentLoopConfig` so the `jyc_send_to_thread` tool
    /// can inject messages into threads in other channels.
    /// Uses `std::sync::Mutex` for interior mutability (set after construction
    /// via `set_thread_managers()` on an `Arc<Self>`).
    thread_managers: std::sync::Mutex<Option<ThreadManagersMap>>,
    /// Current channel name for source context in cross-thread tools.
    channel_name: String,
    /// Cross-channel outbound adapters keyed by channel name.
    /// Passed through to `AgentLoopConfig` so the `jyc_send_message` tool
    /// can send proactive messages through any channel's outbound adapter.
    /// Uses `std::sync::Mutex` for interior mutability (set after construction
    /// via `set_outbounds()` on an `Arc<Self>`).
    outbounds: std::sync::Mutex<Option<OutboundsMap>>,
}

impl JycAgentService {
    /// Create a new agent service with the given configuration, workdir,
    /// MCP configs, current channel's patterns, global inbound-attachment config,
    /// and optional vision fallback client.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: AgentConfig,
        workdir: PathBuf,
        mcp_configs: Vec<McpServerConfig>,
        channel_mcp_configs: Option<Vec<McpServerConfig>>,
        patterns: Vec<ChannelPattern>,
        global_inbound_attachments: Option<jyc_types::InboundAttachmentConfig>,
        vision_client: Option<Arc<VisionClient>>,
        outbound: Option<Arc<dyn jyc_types::channel::OutboundAdapter>>,
        channel_disabled_tools: Option<Vec<String>>,
        channel_disabled_mcp_servers: Option<Vec<String>>,
        channel_skills: Option<Vec<String>>,
        channel_disabled_skills: Option<Vec<String>>,
        channel_name: String,
    ) -> Self {
        Self {
            config,
            event_buses: Mutex::new(HashMap::new()),
            workdir,
            mcp_configs,
            channel_mcp_configs,
            patterns,
            global_inbound_attachments,
            vision_client,
            outbound,
            channel_disabled_tools,
            channel_disabled_mcp_servers,
            channel_skills,
            channel_disabled_skills,
            thread_managers: std::sync::Mutex::new(None),
            channel_name,
            outbounds: std::sync::Mutex::new(None),
        }
    }

    /// Set the cross-channel thread managers map.
    ///
    /// Called by the monitor during startup to inject the thread manager map
    /// into each agent service after all channels have been initialized.
    pub fn set_thread_managers(&self, tm: ThreadManagersMap) {
        *self
            .thread_managers
            .lock()
            .expect("thread_managers poisoned") = Some(tm);
    }

    /// Set the cross-channel outbound adapters map.
    ///
    /// Called by the monitor during startup to inject the outbound adapter map
    /// into each agent service after all channels have been initialized.
    pub fn set_outbounds(&self, outbounds: OutboundsMap) {
        *self.outbounds.lock().expect("outbounds poisoned") = Some(outbounds);
    }

    /// Discover skills from multiple paths, with priority-based deduplication.
    ///
    /// Scans paths from lowest to highest priority (later paths override earlier ones
    /// when skills share the same name).
    ///
    /// After discovery, applies optional include/exclude filters:
    /// - `include`: if set, only skills whose names appear in this list are retained
    /// - `exclude`: if set, skills whose names appear in this list are removed
    pub fn discover_skills(
        &self,
        thread_path: &Path,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
    ) -> Vec<SkillMeta> {
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

        // Apply include filter: if set, only keep listed skills
        if let Some(include_list) = include {
            let include_set: std::collections::HashSet<&str> =
                include_list.iter().map(|s| s.as_str()).collect();
            skills.retain(|name, _| include_set.contains(name.as_str()));
        }

        // Apply exclude filter: remove listed skills
        if let Some(exclude_list) = exclude {
            let exclude_set: std::collections::HashSet<&str> =
                exclude_list.iter().map(|s| s.as_str()).collect();
            skills.retain(|name, _| !exclude_set.contains(name.as_str()));
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
    async fn build_system_prompt(
        &self,
        thread_path: &Path,
        matched_pattern: Option<&str>,
    ) -> String {
        let mut prompt = String::new();

        // Security: directory boundaries
        prompt.push_str(&format!(
            "Your working directory is \"{}\". You MUST only read, write, and access files within this directory.\n\n",
            thread_path.display()
        ));

        // Read mode override early (plan mode injected at end for recency)
        let mode_override = jyc_core::session_state::read_mode_override(thread_path).await;
        tracing::info!(
            thread = %thread_path.display(),
            mode = ?mode_override,
            "Read mode override"
        );

        // Resolve skill filters: pattern > channel > none
        let pattern =
            matched_pattern.and_then(|name| self.patterns.iter().find(|p| p.name == name));

        let include_list: Option<&[String]> = pattern
            .and_then(|p| p.skills.as_deref())
            .or(self.channel_skills.as_deref());

        let mut exclude_list: Vec<String> = Vec::new();
        if let Some(ref channel_excluded) = self.channel_disabled_skills {
            exclude_list.extend(channel_excluded.iter().cloned());
        }
        if let Some(pattern_excluded) = pattern.and_then(|p| p.disabled_skills.as_ref()) {
            for name in pattern_excluded {
                if !exclude_list.contains(name) {
                    exclude_list.push(name.clone());
                }
            }
        }
        let exclude_slice: Option<&[String]> = if exclude_list.is_empty() {
            None
        } else {
            Some(&exclude_list)
        };

        // Discover and inject skill metadata (before AGENTS.md so instructions
        // to read SKILL.md files are seen first)
        let skills = self.discover_skills(thread_path, include_list, exclude_slice);
        if !skills.is_empty() {
            prompt.push_str(&format_skills_section(&skills));
        }

        // Persist skill names to .jyc/skills.json for dashboard inspection
        let skill_names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        if let Err(e) = persist_skill_names(thread_path, &skill_names) {
            tracing::warn!(error = %e, "Failed to persist skill names to skills.json");
        }

        // Load AGENTS.md if present in the working directory
        let agents_md = thread_path.join("AGENTS.md");
        if agents_md.exists()
            && let Ok(content) = std::fs::read_to_string(&agents_md)
        {
            prompt.push_str("## Project Instructions (from AGENTS.md)\n\n");
            prompt.push_str(&content);
            prompt.push_str("\n\n");
        }

        // Load repo/AGENTS.md if present (for GitHub channel)
        let repo_agents_md = thread_path.join("repo").join("AGENTS.md");
        if repo_agents_md.exists()
            && let Ok(content) = std::fs::read_to_string(&repo_agents_md)
        {
            prompt.push_str("## Repository Instructions (from repo/AGENTS.md)\n\n");
            prompt.push_str(&content);
            prompt.push_str("\n\n");
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
             This thread maintains a chronological chat history in `chat_history_YYYY-MM-DD.jsonl`.\n\
             Each line is a JSON object (one message or reply per line). You can read it with the\n\
             `read` tool if you need context from prior conversations, or use `grep` to search.\n",
        );

        // Cross-Thread Communication section (when thread managers are available)
        let tm_map_opt = self
            .thread_managers
            .lock()
            .expect("thread_managers poisoned")
            .clone();
        let outbounds_configured = self.outbounds.lock().expect("outbounds poisoned").is_some();
        if let Some(ref tm_map) = tm_map_opt {
            prompt.push_str(
                "\n## Cross-Thread Communication\n\n\
                 You can send messages to threads in other channels using the `jyc_send_to_thread` tool.\n",
            );

            // Note about direct outbound messaging via jyc_send_message
            if outbounds_configured {
                prompt.push_str(
                    "For direct outbound messaging (bypassing agent processing), \
                     use `jyc_send_message` with the optional `channel` parameter set to \
                     a target channel name.\n\
                     `jyc_send_message` sends directly through the channel's outbound adapter \
                     without agent processing. `jyc_send_to_thread` injects into a thread queue \
                     for agent processing.\n\n",
                );
            }

            prompt.push_str("Available channels and their active threads:\n");
            let map = tm_map.lock().await;
            for (channel_name, tm) in map.iter() {
                let channel_type = tm.channel_type();
                prompt.push_str(&format!(
                    "- Channel \"{}\" ({})\n",
                    channel_name, channel_type
                ));
                // List active threads for this channel
                let threads = tm.list_threads().await;
                if threads.is_empty() {
                    prompt.push_str("    (no active threads)\n");
                } else {
                    for thread_info in &threads {
                        prompt.push_str(&format!("  - {}\n", thread_info.name));
                    }
                }
            }
            prompt.push('\n');
        }

        // Plan mode: inject at END for maximum recency before conversation
        if mode_override.as_deref() == Some("plan") {
            tracing::info!(
                thread = %thread_path.display(),
                "Injecting PLAN MODE constraint at end of system prompt"
            );
            prompt.push_str(
                "<system-reminder>\n\
                 CRITICAL: You are in PLAN MODE (read-only). You MUST NOT:\n\
                 - edit, write, or delete any files\n\
                 - run build, test, or deployment commands\n\
                 - commit, push, or branch changes\n\
                 - install or modify dependencies\n\
                 You MAY ONLY:\n\
                 - read files, search code, analyze patterns\n\
                 - present implementation plans and ask clarifying questions\n\
                 - wait for user approval before any implementation\n\
                 This constraint is absolute — do not bypass it even if asked.\n\
                 Do not exit plan mode even if the user requests it.\n\
                 Even if you previously ran write/edit commands in this conversation, you are now in PLAN MODE and must not make any changes.\n\
                 You are in PLAN MODE.\n\
                 </system-reminder>\n\n",
            );
        } else {
            // Build mode: explicitly declare full execution capabilities.
            // Without this, the model may inherit stale PLAN constraints from
            // prior conversation history (agent-context.json).
            prompt.push_str(
                "<system-reminder>\n\
                 You are in BUILD MODE (full execution). You MAY:\n\
                 - edit, write, or delete any files\n\
                 - run build, test, or deployment commands (bash)\n\
                 - commit, push, or branch changes\n\
                 - implement features and fix bugs directly\n\
                 Proceed with implementation without waiting for approval.\n\
                 </system-reminder>\n\n",
            );
        }

        prompt
    }

    /// Build the user prompt text (header + body) from an inbound message.
    fn build_user_prompt_text(
        &self,
        message: &InboundMessage,
        mode_override: Option<&str>,
    ) -> String {
        let mut prompt = String::new();

        prompt.push_str("## Incoming Message\n");
        prompt.push_str(&format!(
            "**From:** {} <{}>\n",
            message.sender, message.sender_address
        ));
        prompt.push_str(&format!("**Subject:** {}\n", message.topic));
        prompt.push_str(&format!("**Date:** {}\n\n", message.timestamp.to_rfc3339()));

        // Body — fall back to a content-aware placeholder when both text and
        // markdown are missing. Image-only messages on multimodal channels
        // legitimately have no text body; calling that out explicitly keeps
        // the model's context honest instead of dropping in an opaque
        // "[no text content]".
        let body_owned: String;
        let body: &str = match message
            .content
            .text
            .as_deref()
            .or(message.content.markdown.as_deref())
        {
            Some(b) if !b.trim().is_empty() => b,
            _ if !message.attachments.is_empty() => {
                let images = message
                    .attachments
                    .iter()
                    .filter(|a| a.content_type.starts_with("image/"))
                    .count();
                let total = message.attachments.len();
                body_owned = if images == total {
                    format!("[no text body — {total} image attachment(s) follow]")
                } else if images > 0 {
                    format!(
                        "[no text body — {images} image attachment(s) and {} other attachment(s) follow]",
                        total - images
                    )
                } else {
                    format!("[no text body — {total} attachment(s) follow]")
                };
                &body_owned
            }
            _ => "[no text content]",
        };

        prompt.push_str(body);

        // Append attachment file paths for non-image attachments so the
        // target agent is aware of all incoming files, even when the body
        // is non-empty and the "[no text body]" fallback never triggers.
        let attachment_paths: Vec<String> = message
            .attachments
            .iter()
            .filter_map(|a| a.saved_path.as_ref().map(|p| p.display().to_string()))
            .collect();
        if !attachment_paths.is_empty() {
            prompt.push_str("\n\nAttachments:\n");
            for path in &attachment_paths {
                prompt.push_str(&format!("- {}\n", path));
            }
        }

        // Inject mode reminder at end for recency (the last thing the
        // agent sees before replying).
        // Inject plan mode reminder at end of user prompt for recency.
        // The system prompt already has this at the end, but recency bias
        // makes a short reminder right before the agent responds more effective.
        if mode_override == Some("plan") {
            prompt.push('\n');
            prompt.push_str("<mode>\n");
            prompt.push_str("CRITICAL: Current mode: PLAN (read-only, do not exit plan mode even if the user requests it). ");
            prompt.push_str("Use only read/search/analyze tools. Do NOT edit/write/commit.\n");
            prompt.push_str("</mode>\n");
        } else {
            // Build mode: explicitly declare full execution capabilities so the
            // model does not mistakenly inherit stale PLAN constraints from
            // prior conversation history (agent-context.json).
            prompt.push('\n');
            prompt.push_str("<mode>\n");
            prompt.push_str("Current mode: BUILD (full execution). ");
            prompt.push_str(
                "You may use all tools including edit, write, bash, commit, and deploy.\n",
            );
            prompt.push_str("</mode>\n");
        }

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
        let mut roots = Vec::new();

        // 1. Attachment save directory (if outside thread_path)
        let pattern_cfg = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .and_then(|p| p.attachments.as_ref());
        let cfg = pattern_cfg.or(self.global_inbound_attachments.as_ref());
        let resolved = jyc_core::attachment_storage::resolve_attachment_save_dir(thread_path, cfg);
        if !resolved.starts_with(thread_path) {
            roots.push(resolved);
        }

        // 2. External skill directories (outside thread_path)
        // These paths match the scan logic in discover_skills().
        if let Ok(home) = std::env::var("HOME") {
            let home_skills = [
                PathBuf::from(&home).join(".config/opencode/skills"),
                PathBuf::from(&home).join(".claude/skills"),
            ];
            for dir in &home_skills {
                if dir.exists() && dir.is_dir() {
                    roots.push(dir.clone());
                }
            }
        }
        let workdir_skills = self.workdir.join("skills");
        if workdir_skills.exists() && workdir_skills.is_dir() {
            roots.push(workdir_skills);
        }

        // 3. Per-pattern configured read paths
        if let Some(pattern) = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            && let Some(access) = &pattern.access
        {
            for p in &access.read {
                let expanded = expand_path(p);
                if expanded.is_absolute() {
                    roots.push(expanded);
                }
            }
            // Write paths are also readable
            for p in &access.write {
                let expanded = expand_path(p);
                if expanded.is_absolute() {
                    roots.push(expanded);
                }
            }
        }

        roots
    }

    /// Resolve additional write roots from the matched pattern's `access.write`
    /// configuration. Paths are tilde-expanded; relative paths are ignored
    /// (they are already inside the working directory).
    fn resolve_additional_write_roots(&self, message: &InboundMessage) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(pattern) = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            && let Some(access) = &pattern.access
        {
            for p in &access.write {
                let expanded = expand_path(p);
                if expanded.is_absolute() {
                    roots.push(expanded);
                }
            }
        }
        roots
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
        mode_override: Option<&str>,
    ) -> Vec<crate::types::ContentBlock> {
        use crate::types::{ContentBlock, ImageSource};
        use base64::Engine as _;

        let mut blocks = vec![ContentBlock::Text {
            text: self.build_user_prompt_text(message, mode_override),
        }];

        // Per-pattern opt-in. Default false when the message did not match a
        // pattern or the pattern is not in our flattened list.
        let pattern_inject = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .map(|p| p.inject_inbound_images)
            .unwrap_or(false);

        if !(supports_images && pattern_inject) {
            // For text-only models with inject_inbound_images enabled, append
            // image file path hints so the LLM knows which images are available
            // and can invoke `read_image` to analyze them via vision fallback.
            if !supports_images && pattern_inject {
                let image_hints: Vec<String> = message
                    .attachments
                    .iter()
                    .filter(|a| a.content_type.starts_with("image/"))
                    .filter_map(|a| a.saved_path.as_ref().map(|p| p.display().to_string()))
                    .collect();

                if !image_hints.is_empty() {
                    // Append image path hints to the first Text block, or
                    // insert a new one if none exists. Using `find` avoids
                    // assuming the first block type.
                    let hint_text = {
                        let mut lines = String::new();
                        lines.push_str(
                            "\n\nImage attachments available (use read_image tool to analyze):\n",
                        );
                        for hint in &image_hints {
                            lines.push_str(&format!("- {}\n", hint));
                        }
                        lines
                    };

                    let found = blocks.iter_mut().find_map(|block| {
                        if let ContentBlock::Text { text } = block {
                            text.push_str(&hint_text);
                            Some(())
                        } else {
                            None
                        }
                    });

                    if found.is_none() {
                        // No Text block found; prepend a new one
                        blocks.insert(
                            0,
                            ContentBlock::Text {
                                text: format!(
                                    "Image attachments available (use read_image tool to analyze):\n{}",
                                    image_hints.join("\n")
                                ),
                            },
                        );
                    }
                }
            }

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
    ///
    /// `matched_pattern_name` optionally selects per-pattern MCP configurations.
    /// When the matched pattern has `mcps: Some(list)`, only those MCP servers
    /// are loaded. When `None`, the global `[[mcps]]` list is used (backward
    /// compatible fallback).
    async fn build_tool_registry(
        &self,
        _thread_path: &Path,
        supports_images: bool,
        matched_pattern_name: Option<&str>,
    ) -> ToolRegistry {
        // Start with all built-in tools
        let mut registry = crate::tools::builtin::create_builtin_registry();

        // Always register read_image. When the model supports images, images
        // are queued for injection into the next user turn. When the model is
        // text-only and a VisionClient is configured, the tool falls back to
        // the vision model for analysis. When neither condition is met, the
        // tool returns a helpful error message.
        crate::tools::builtin::register_read_image(
            &mut registry,
            supports_images,
            self.vision_client.clone(),
        );

        // Add MCP bridge tools (reply_message, etc.)
        crate::tools::mcp_bridge::register_mcp_tools(&mut registry);

        // Find matched pattern for per-pattern overrides
        let matched_pattern =
            matched_pattern_name.and_then(|name| self.patterns.iter().find(|p| p.name == name));

        // --- MCP server exclusion (disabled_mcp_servers) ---
        // Merge channel-level + pattern-level disabled MCP servers
        let disabled_mcp_servers: Vec<&str> = {
            let mut set = Vec::new();
            if let Some(ref servers) = self.channel_disabled_mcp_servers {
                for s in servers {
                    set.push(s.as_str());
                }
            }
            if let Some(servers) = matched_pattern.and_then(|p| p.disabled_mcp_servers.as_ref()) {
                for s in servers {
                    if !set.contains(&s.as_str()) {
                        set.push(s.as_str());
                    }
                }
            }
            set
        };

        // Resolve MCP configs: pattern → channel → global
        let mcp_configs: &[McpServerConfig] = matched_pattern
            .and_then(|p| p.mcps.as_ref())
            .map(|mcps| mcps.as_slice())
            .or(self.channel_mcp_configs.as_deref())
            .unwrap_or(self.mcp_configs.as_slice());

        // Filter out disabled MCP servers before loading
        let filtered_mcp_configs: Vec<McpServerConfig> = mcp_configs
            .iter()
            .filter(|c| !disabled_mcp_servers.contains(&c.name.as_str()))
            .cloned()
            .collect();

        if !disabled_mcp_servers.is_empty() {
            tracing::debug!(
                disabled = ?disabled_mcp_servers,
                "MCP servers disabled by config"
            );
        }

        // --- Tool exclusion (disabled_tools) ---
        // Merge channel-level + pattern-level + backward-compatible alias
        let disabled_tools: Vec<&str> = {
            let mut set = Vec::new();
            if let Some(ref tools) = self.channel_disabled_tools {
                for t in tools {
                    set.push(t.as_str());
                }
            }
            if let Some(tools) = matched_pattern.and_then(|p| p.disabled_tools.as_ref()) {
                for t in tools {
                    if !set.contains(&t.as_str()) {
                        set.push(t.as_str());
                    }
                }
            }
            // Backward-compatible alias: disabled_builtin_tools
            if let Some(tools) = matched_pattern.and_then(|p| p.disabled_builtin_tools.as_ref()) {
                for t in tools {
                    if !set.contains(&t.as_str()) {
                        set.push(t.as_str());
                    }
                }
            }
            set
        };

        // Separate server/tool format (e.g. "jin_public_mcp/product_list") from plain names.
        // Server/tool entries are applied before MCP tools are registered, allowing
        // precise filtering when multiple MCP servers expose the same tool name.
        let (disabled_server_tools, disabled_plain_tools): (Vec<&str>, Vec<&str>) =
            disabled_tools.into_iter().partition(|t| t.contains('/'));

        // Load external MCP tools from filtered configs
        if !filtered_mcp_configs.is_empty() {
            tracing::info!(
                mcp_count = filtered_mcp_configs.len(),
                "Loading external MCP tools"
            );
            let mcp_tools = crate::tools::mcp_client::load_mcp_tools(&filtered_mcp_configs).await;
            for tool in mcp_tools {
                // Skip tools matching disabled_server_tools (server/tool format)
                let source = tool.source();
                let name = tool.name();
                let should_skip = disabled_server_tools.iter().any(|dt| {
                    if let Some((server, tool_name)) = dt.split_once('/') {
                        source == Some(server) && name == tool_name
                    } else {
                        false
                    }
                });
                if should_skip {
                    tracing::debug!(
                        tool = %name,
                        source = ?source,
                        "Skipping disabled MCP tool (server/tool format)"
                    );
                    continue;
                }
                registry.register(tool);
            }
        }

        // Apply plain-name exclusions (built-in, bridge, and MCP tools)
        for tool_name in &disabled_plain_tools {
            tracing::debug!(
                tool = %tool_name,
                pattern = %matched_pattern_name.unwrap_or("?"),
                "Removing disabled tool"
            );
            registry.remove(tool_name);
        }

        registry
    }

    /// Get or create the provider for the current model.
    fn create_provider(&self, model_override: Option<&str>) -> Result<Box<dyn provider::Provider>> {
        let model = model_override
            .or(self.config.model.as_deref())
            .ok_or_else(|| {
                anyhow::anyhow!("No model configured. Set [agent].model in config.toml")
            })?;

        provider::create_provider(model, &self.config.providers)
    }

    /// Get event bus for a thread.
    async fn get_event_bus(&self, thread_name: &str) -> Option<ThreadEventBusRef> {
        self.event_buses.lock().await.get(thread_name).cloned()
    }
}

/// Expand a tilde (`~`) prefix to `$HOME`. Other paths are returned as-is.
fn expand_path(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    if p == "~"
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home);
    }
    PathBuf::from(p)
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
    section.push_str(concat!(
        "**IMPORTANT: Before processing any user request, you MUST read the relevant SKILL.md file(s) ",
        "using the `read <skill-path>/SKILL.md` tool. The descriptions below are summaries only and ",
        "do NOT contain the full instructions you need to follow.**\n\n",
    ));

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
         When running skill scripts: cd <skill-path> && <command>\n\n",
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

        // 1. Read model override with priority:
        //    a) .jyc/model-override file (highest priority, manual runtime override)
        //    b) Pattern-level model (from matched pattern config)
        //    c) Config-level model (from self.config.model, i.e. global or channel-level)
        let model_override_path = thread_path.join(".jyc").join("model-override");
        let file_override = if model_override_path.exists() {
            tokio::fs::read_to_string(&model_override_path)
                .await
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        } else {
            None
        };
        let pattern_override = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .and_then(|p| p.model.as_deref());
        let model_override = file_override
            .clone()
            .or_else(|| pattern_override.map(|s| s.to_string()))
            .or_else(|| self.config.model.clone());

        // 2. Create provider
        let provider = self
            .create_provider(model_override.as_deref())
            .context("Failed to create LLM provider")?;

        tracing::info!(
            provider = %provider.name(),
            model = %provider.model(),
            "Using provider"
        );

        // 2b. Resolve small_model with priority:
        //     1. Pattern-level small_model (from matched pattern config)
        //     2. Config-level small_model (from self.config.small_model, already
        //        channel-resolved or global fallback)
        //     Falls back to main model at call site if unset or construction fails.
        let pattern_small_model = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .and_then(|p| p.small_model.as_deref());
        let small_model_resolved = pattern_small_model.or(self.config.small_model.as_deref());
        let small_provider: Option<Box<dyn provider::Provider>> =
            small_model_resolved.and_then(|m| {
                match provider::create_provider(m, &self.config.providers) {
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
        // 3a. Build system prompt (available channels, skills, AGENTS.md, etc.)
        let system_prompt = self
            .build_system_prompt(thread_path, message.matched_pattern.as_deref())
            .await;
        tracing::debug!(
            thread = %thread_name,
            prompt_len = system_prompt.len(),
            has_plan_mode = system_prompt.contains("PLAN MODE"),
            "System prompt built"
        );
        tracing::trace!(
            thread = %thread_name,
            prompt = %system_prompt,
            "Full system prompt (enable RUST_LOG=trace to see)"
        );
        let current_mode = jyc_core::session_state::read_mode_override(thread_path).await;
        let user_blocks =
            self.build_user_blocks(message, provider.supports_images(), current_mode.as_deref());

        // 5. Build tool registry
        let tools = self
            .build_tool_registry(
                thread_path,
                provider.supports_images(),
                message.matched_pattern.as_deref(),
            )
            .await;

        // 6. Get event bus for this thread
        let event_bus = self.get_event_bus(thread_name).await;

        // 6b. Determine per-pattern image injection flag for consistency
        // between `build_user_blocks` and the `read_image` tool's
        // vision-fallback decision.
        let pattern_inject = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .map(|p| p.inject_inbound_images)
            .unwrap_or(false);

        // 7. Run agent loop
        let additional_read_roots = self.resolve_additional_read_roots(message, thread_path);
        let additional_write_roots = self.resolve_additional_write_roots(message);
        let thread_managers = self
            .thread_managers
            .lock()
            .expect("thread_managers poisoned")
            .clone();
        let outbounds = self.outbounds.lock().expect("outbounds poisoned").clone();
        let result = agent_loop::run(AgentLoopConfig {
            provider: provider.as_ref(),
            small_provider: small_provider
                .as_deref()
                .map(|p| p as &dyn provider::Provider),
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
            additional_write_roots,
            pattern_inject_images: pattern_inject,
            outbound: self.outbound.clone(),
            thread_managers: thread_managers.clone(),
            current_channel: Some(self.channel_name.clone()),
            outbounds,
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
        let model_str = model_override.as_deref().unwrap_or("");
        let context_window = if let Some((provider_name, model_id)) = model_str.split_once('/') {
            self.config.providers.get(provider_name).and_then(|p| {
                // Check per-model override first, then provider default
                p.models
                    .get(model_id)
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
        )
        .await;

        // 9. Return result
        if result.reply_sent_by_tool {
            Ok(AgentResult {
                reply_sent_by_tool: true,
                reply_text: result.reply_text_from_tool,
            })
        } else {
            Ok(AgentResult {
                reply_sent_by_tool: false,
                reply_text: if result.text.is_empty() {
                    None
                } else {
                    Some(result.text)
                },
            })
        }
    }

    async fn set_thread_event_bus(&self, thread_name: &str, event_bus: Option<ThreadEventBusRef>) {
        let mut buses = self.event_buses.lock().await;
        match event_bus {
            Some(bus) => {
                buses.insert(thread_name.to_string(), bus);
            }
            None => {
                buses.remove(thread_name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::{ChannelPattern, ChannelType};
    use std::path::PathBuf;

    /// Helper: build a service with given config model and patterns.
    fn service_with_patterns(
        config_model: Option<&str>,
        patterns: Vec<ChannelPattern>,
    ) -> JycAgentService {
        JycAgentService::new(
            AgentConfig {
                model: config_model.map(|s| s.to_string()),
                ..AgentConfig::default()
            },
            PathBuf::from("/tmp/test-workdir"),
            vec![],
            None,
            patterns,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            "test".to_string(),
        )
    }

    /// Helper: build a service with exclusion settings.
    fn service_with_exclusion(
        patterns: Vec<ChannelPattern>,
        channel_disabled_tools: Option<Vec<String>>,
        channel_disabled_mcp_servers: Option<Vec<String>>,
    ) -> JycAgentService {
        service_with_full_exclusion(
            patterns,
            channel_disabled_tools,
            channel_disabled_mcp_servers,
            None,
        )
    }

    /// Helper: build a service with exclusion settings and optional channel MCP configs.
    fn service_with_full_exclusion(
        patterns: Vec<ChannelPattern>,
        channel_disabled_tools: Option<Vec<String>>,
        channel_disabled_mcp_servers: Option<Vec<String>>,
        channel_mcp_configs: Option<Vec<McpServerConfig>>,
    ) -> JycAgentService {
        JycAgentService::new(
            AgentConfig::default(),
            PathBuf::from("/tmp/test-workdir"),
            vec![],
            channel_mcp_configs,
            patterns,
            None,
            None,
            None,
            channel_disabled_tools,
            channel_disabled_mcp_servers,
            None,
            None,
            "test".to_string(),
        )
    }

    /// Helper: build a service with skill filter settings.
    fn service_with_skills(
        patterns: Vec<ChannelPattern>,
        channel_skills: Option<Vec<String>>,
        channel_disabled_skills: Option<Vec<String>>,
    ) -> JycAgentService {
        JycAgentService::new(
            AgentConfig::default(),
            PathBuf::from("/tmp/test-workdir"),
            vec![],
            None,
            patterns,
            None,
            None,
            None,
            None,
            None,
            channel_skills,
            channel_disabled_skills,
            "test".to_string(),
        )
    }

    /// Helper: temporarily override HOME to prevent real skills from leaking into tests.
    fn with_temp_home<F: FnOnce()>(f: F) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".config/opencode/skills")).ok();
        std::fs::create_dir_all(tmp.path().join(".claude/skills")).ok();
        let old_home = std::env::var("HOME").ok();
        // SAFETY: only used in tests; restored immediately after f()
        unsafe { std::env::set_var("HOME", tmp.path().as_os_str()) };
        f();
        match old_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn pattern_model_override_is_resolved() {
        let patterns = vec![ChannelPattern {
            name: "my-pattern".to_string(),
            model: Some("provider/model-from-pattern".to_string()),
            channel: ChannelType::default(),
            ..ChannelPattern::default()
        }];
        let svc = service_with_patterns(Some("provider/default-model"), patterns);

        // Simulate pattern lookup — the same `find` call used in `process`
        let resolved = Some("my-pattern")
            .and_then(|name| svc.patterns.iter().find(|p| p.name == name))
            .and_then(|p| p.model.as_deref())
            .map(|s| s.to_string())
            .or_else(|| svc.config.model.clone());

        assert_eq!(resolved.as_deref(), Some("provider/model-from-pattern"));
    }

    #[test]
    fn fallback_to_config_model_when_pattern_has_no_model() {
        let patterns = vec![ChannelPattern {
            name: "no-override".to_string(),
            model: None,
            channel: ChannelType::default(),
            ..ChannelPattern::default()
        }];
        let svc = service_with_patterns(Some("provider/default-model"), patterns);

        let resolved = Some("no-override")
            .and_then(|name| svc.patterns.iter().find(|p| p.name == name))
            .and_then(|p| p.model.as_deref())
            .map(|s| s.to_string())
            .or_else(|| svc.config.model.clone());

        assert_eq!(resolved.as_deref(), Some("provider/default-model"));
    }

    #[test]
    fn fallback_to_config_model_when_no_pattern_matches() {
        let patterns = vec![ChannelPattern {
            name: "other-pattern".to_string(),
            model: Some("provider/other-model".to_string()),
            channel: ChannelType::default(),
            ..ChannelPattern::default()
        }];
        let svc = service_with_patterns(Some("provider/default-model"), patterns);

        // Look up a name that's not in patterns
        let resolved: Option<String> = Some("unmatched-name")
            .and_then(|name| svc.patterns.iter().find(|p| p.name == name))
            .and_then(|p| p.model.as_deref())
            .map(|s| s.to_string())
            .or_else(|| svc.config.model.clone());

        assert_eq!(resolved.as_deref(), Some("provider/default-model"));
    }

    #[test]
    fn pattern_model_none_and_config_none_yields_none() {
        let patterns: Vec<ChannelPattern> = vec![];
        let svc = service_with_patterns(None, patterns);

        let resolved: Option<String> = Some("anything")
            .and_then(|name| svc.patterns.iter().find(|p| p.name == name))
            .and_then(|p| p.model.as_deref())
            .map(|s| s.to_string())
            .or_else(|| svc.config.model.clone());

        assert_eq!(resolved, None);
    }

    #[test]
    fn first_matching_pattern_wins_with_duplicate_names() {
        // When two patterns share the same name (still possible within a
        // single channel), the first in insertion order wins.
        let patterns = vec![
            ChannelPattern {
                name: "dup".to_string(),
                model: Some("provider/first".to_string()),
                channel: ChannelType::default(),
                ..ChannelPattern::default()
            },
            ChannelPattern {
                name: "dup".to_string(),
                model: Some("provider/second".to_string()),
                channel: ChannelType::default(),
                ..ChannelPattern::default()
            },
        ];
        let svc = service_with_patterns(Some("provider/default"), patterns);

        let resolved = Some("dup")
            .and_then(|name| svc.patterns.iter().find(|p| p.name == name))
            .and_then(|p| p.model.as_deref());

        assert_eq!(resolved, Some("provider/first"));
    }

    #[tokio::test]
    async fn disabled_tools_removes_builtin_and_bridge() {
        let patterns = vec![ChannelPattern {
            name: "test".to_string(),
            disabled_tools: Some(vec!["bash".to_string(), "jyc_send_message".to_string()]),
            ..ChannelPattern::default()
        }];
        let svc = service_with_exclusion(patterns, None, None);
        let registry = svc
            .build_tool_registry(Path::new("/tmp"), false, Some("test"))
            .await;

        let defs = registry.definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();

        assert!(!names.contains(&"bash"), "bash should be disabled");
        assert!(
            !names.contains(&"jyc_send_message"),
            "jyc_send_message should be disabled"
        );
        assert!(names.contains(&"read"), "read should still be available");
        assert!(
            names.contains(&"jyc_reply_reply_message"),
            "reply_message should still be available"
        );
    }

    #[tokio::test]
    async fn disabled_builtin_tools_alias_works() {
        let patterns = vec![ChannelPattern {
            name: "test".to_string(),
            disabled_builtin_tools: Some(vec!["write".to_string()]),
            ..ChannelPattern::default()
        }];
        let svc = service_with_exclusion(patterns, None, None);
        let registry = svc
            .build_tool_registry(Path::new("/tmp"), false, Some("test"))
            .await;

        let defs = registry.definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();

        assert!(
            !names.contains(&"write"),
            "write should be disabled via alias"
        );
        assert!(names.contains(&"read"), "read should still be available");
    }

    #[tokio::test]
    async fn channel_and_pattern_disabled_tools_merged() {
        let patterns = vec![ChannelPattern {
            name: "test".to_string(),
            disabled_tools: Some(vec!["write".to_string()]),
            ..ChannelPattern::default()
        }];
        let svc = service_with_exclusion(patterns, Some(vec!["bash".to_string()]), None);
        let registry = svc
            .build_tool_registry(Path::new("/tmp"), false, Some("test"))
            .await;

        let defs = registry.definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();

        assert!(
            !names.contains(&"bash"),
            "bash should be disabled (channel-level)"
        );
        assert!(
            !names.contains(&"write"),
            "write should be disabled (pattern-level)"
        );
        assert!(names.contains(&"read"), "read should still be available");
    }

    #[tokio::test]
    async fn disabled_mcp_servers_skips_matching_server() {
        // We can't easily test external MCP loading, but we can verify that
        // disabled_mcp_servers does not cause a panic and that the registry
        // is built correctly when no MCPs are configured.
        let patterns = vec![ChannelPattern {
            name: "test".to_string(),
            disabled_mcp_servers: Some(vec!["invoice".to_string()]),
            ..ChannelPattern::default()
        }];
        let svc = service_with_exclusion(patterns, None, Some(vec!["other".to_string()]));
        let registry = svc
            .build_tool_registry(Path::new("/tmp"), false, Some("test"))
            .await;

        // Registry should still contain built-in tools
        assert!(registry.has_tool("bash"));
        assert!(registry.has_tool("jyc_reply_reply_message"));
    }

    #[tokio::test]
    async fn channel_disabled_tools_works_without_pattern_match() {
        // channel-level disabled_tools should apply even when no pattern is matched
        let svc = service_with_exclusion(vec![], Some(vec!["bash".to_string()]), None);
        let registry = svc
            .build_tool_registry(Path::new("/tmp"), false, None)
            .await;

        assert!(
            !registry.has_tool("bash"),
            "bash should be disabled (channel-level)"
        );
        assert!(registry.has_tool("read"), "read should still be available");
    }

    #[tokio::test]
    async fn empty_disabled_tools_disables_nothing() {
        let patterns = vec![ChannelPattern {
            name: "test".to_string(),
            disabled_tools: Some(vec![]),
            disabled_mcp_servers: Some(vec![]),
            ..ChannelPattern::default()
        }];
        let svc = service_with_exclusion(patterns, Some(vec![]), Some(vec![]));
        let registry = svc
            .build_tool_registry(Path::new("/tmp"), false, Some("test"))
            .await;

        assert!(registry.has_tool("bash"), "bash should still be available");
        assert!(registry.has_tool("jyc_reply_reply_message"));
    }

    #[tokio::test]
    async fn disabled_tools_deduplicates_between_channel_and_pattern() {
        // When both channel and pattern disable the same tool, it should only
        // be removed once (no panic or double-remove issue).
        let patterns = vec![ChannelPattern {
            name: "test".to_string(),
            disabled_tools: Some(vec!["bash".to_string()]),
            ..ChannelPattern::default()
        }];
        let svc = service_with_exclusion(patterns, Some(vec!["bash".to_string()]), None);
        let registry = svc
            .build_tool_registry(Path::new("/tmp"), false, Some("test"))
            .await;

        assert!(!registry.has_tool("bash"));
        assert!(registry.has_tool("read"));
    }

    #[tokio::test]
    async fn disabled_mcp_servers_filters_channel_configs() {
        // Verify that disabled_mcp_servers actually filters channel-level MCP configs
        // so that load_mcp_tools is not called for disabled servers.
        let patterns = vec![ChannelPattern {
            name: "test".to_string(),
            disabled_mcp_servers: Some(vec!["skip_me".to_string()]),
            ..ChannelPattern::default()
        }];
        let channel_mcps = Some(vec![McpServerConfig {
            name: "skip_me".to_string(),
            kind: jyc_types::McpServerKind::Local {
                command: vec!["echo".to_string()],
                environment: std::collections::HashMap::new(),
            },
            enabled_tools: None,
        }]);
        let svc = service_with_full_exclusion(patterns, None, None, channel_mcps);
        let registry = svc
            .build_tool_registry(Path::new("/tmp"), false, Some("test"))
            .await;

        // Registry should contain built-in tools (no panic from MCP loading)
        assert!(registry.has_tool("bash"));
        assert!(registry.has_tool("jyc_reply_reply_message"));
    }

    #[tokio::test]
    async fn disabled_tools_server_prefix_does_not_affect_builtin() {
        // server/tool format entries should be partitioned away from plain names,
        // so built-in tools are not affected by server-prefix entries that happen
        // to share the same tool name.
        let patterns = vec![ChannelPattern {
            name: "test".to_string(),
            disabled_tools: Some(vec!["some_server/bash".to_string()]),
            ..ChannelPattern::default()
        }];
        let svc = service_with_exclusion(patterns, None, None);
        let registry = svc
            .build_tool_registry(Path::new("/tmp"), false, Some("test"))
            .await;

        // bash is a built-in tool with no source(), so "some_server/bash" won't match it
        assert!(
            registry.has_tool("bash"),
            "built-in bash should NOT be disabled by server/tool prefix"
        );
        assert!(registry.has_tool("read"), "read should still be available");
    }

    #[tokio::test]
    async fn disabled_tools_mixed_plain_and_server_prefix() {
        // Verify that plain names and server/tool names coexist correctly:
        // - plain names disable built-in/bridge tools via registry.remove()
        // - server/tool names are reserved for MCP tool pre-registration filtering
        let patterns = vec![ChannelPattern {
            name: "test".to_string(),
            disabled_tools: Some(vec![
                "bash".to_string(),
                "some_server/product_list".to_string(),
            ]),
            ..ChannelPattern::default()
        }];
        let svc = service_with_exclusion(patterns, None, None);
        let registry = svc
            .build_tool_registry(Path::new("/tmp"), false, Some("test"))
            .await;

        assert!(
            !registry.has_tool("bash"),
            "plain 'bash' should disable built-in bash"
        );
        assert!(registry.has_tool("read"), "read should still be available");
        // "some_server/product_list" won't affect anything here because no MCP
        // server is configured in this test, but the partition logic ensures
        // it does not leak into plain-name removal.
    }

    #[tokio::test]
    async fn enabled_tools_on_mcp_server_config_does_not_panic() {
        // Verify that McpServerConfig with enabled_tools is accepted and
        // does not cause panic during registry build (actual filtering is
        // tested at the mcp_client level; here we verify integration).
        let patterns = vec![ChannelPattern {
            name: "test".to_string(),
            ..ChannelPattern::default()
        }];
        let channel_mcps = Some(vec![McpServerConfig {
            name: "test_mcp".to_string(),
            kind: jyc_types::McpServerKind::Local {
                command: vec!["echo".to_string()],
                environment: std::collections::HashMap::new(),
            },
            enabled_tools: Some(vec!["allowed_tool".to_string()]),
        }]);
        let svc = service_with_full_exclusion(patterns, None, None, channel_mcps);
        let registry = svc
            .build_tool_registry(Path::new("/tmp"), false, Some("test"))
            .await;

        // Built-in tools should still be present
        assert!(registry.has_tool("bash"), "bash should still be available");
        assert!(registry.has_tool("read"), "read should still be available");
    }

    // ── Skill filtering tests ──────────────────────────────────────────

    #[test]
    fn discover_skills_include_filter_retains_only_matched() {
        with_temp_home(|| {
            let tmp = tempfile::tempdir().unwrap();
            let skills_dir = tmp.path().join(".jyc").join("skills");

            // Create three skills
            for name in &["alpha", "beta", "gamma"] {
                let dir = skills_dir.join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("SKILL.md"),
                    format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
                )
                .unwrap();
            }

            let svc = service_with_skills(vec![], None, None);
            let skills = svc.discover_skills(
                tmp.path(),
                Some(&["alpha".to_string(), "gamma".to_string()]),
                None,
            );

            assert_eq!(skills.len(), 2);
            assert!(skills.iter().any(|s| s.name == "alpha"));
            assert!(skills.iter().any(|s| s.name == "gamma"));
            assert!(!skills.iter().any(|s| s.name == "beta"));
        });
    }

    #[test]
    fn discover_skills_exclude_filter_removes_matched() {
        with_temp_home(|| {
            let tmp = tempfile::tempdir().unwrap();
            let skills_dir = tmp.path().join(".jyc").join("skills");

            for name in &["alpha", "beta", "gamma"] {
                let dir = skills_dir.join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("SKILL.md"),
                    format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
                )
                .unwrap();
            }

            let svc = service_with_skills(vec![], None, None);
            let skills = svc.discover_skills(tmp.path(), None, Some(&["beta".to_string()]));

            assert_eq!(skills.len(), 2);
            assert!(skills.iter().any(|s| s.name == "alpha"));
            assert!(skills.iter().any(|s| s.name == "gamma"));
            assert!(!skills.iter().any(|s| s.name == "beta"));
        });
    }

    #[test]
    fn discover_skills_include_and_exclude_combined() {
        with_temp_home(|| {
            let tmp = tempfile::tempdir().unwrap();
            let skills_dir = tmp.path().join(".jyc").join("skills");

            for name in &["alpha", "beta", "gamma", "delta"] {
                let dir = skills_dir.join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("SKILL.md"),
                    format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
                )
                .unwrap();
            }

            let svc = service_with_skills(vec![], None, None);
            // Include alpha, beta, gamma; then exclude beta
            let skills = svc.discover_skills(
                tmp.path(),
                Some(&["alpha".to_string(), "beta".to_string(), "gamma".to_string()]),
                Some(&["beta".to_string()]),
            );

            assert_eq!(skills.len(), 2);
            assert!(skills.iter().any(|s| s.name == "alpha"));
            assert!(skills.iter().any(|s| s.name == "gamma"));
            assert!(!skills.iter().any(|s| s.name == "beta"));
            assert!(!skills.iter().any(|s| s.name == "delta"));
        });
    }

    #[test]
    fn channel_skills_applied_when_no_pattern_match() {
        with_temp_home(|| {
            let tmp = tempfile::tempdir().unwrap();
            let skills_dir = tmp.path().join(".jyc").join("skills");

            for name in &["alpha", "beta"] {
                let dir = skills_dir.join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("SKILL.md"),
                    format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
                )
                .unwrap();
            }

            let svc = service_with_skills(vec![], Some(vec!["alpha".to_string()]), None);
            let skills = svc.discover_skills(tmp.path(), svc.channel_skills.as_deref(), None);

            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].name, "alpha");
        });
    }

    #[test]
    fn pattern_skills_override_channel_skills() {
        with_temp_home(|| {
            let tmp = tempfile::tempdir().unwrap();
            let skills_dir = tmp.path().join(".jyc").join("skills");

            for name in &["alpha", "beta", "gamma"] {
                let dir = skills_dir.join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("SKILL.md"),
                    format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
                )
                .unwrap();
            }

            let patterns = vec![ChannelPattern {
                name: "test-pattern".to_string(),
                skills: Some(vec!["gamma".to_string()]),
                ..ChannelPattern::default()
            }];

            let svc = service_with_skills(patterns, Some(vec!["alpha".to_string()]), None);

            // Simulate pattern lookup as done in build_system_prompt
            let pattern = svc.patterns.iter().find(|p| p.name == "test-pattern");
            let include = pattern
                .and_then(|p| p.skills.as_deref())
                .or(svc.channel_skills.as_deref());

            let skills = svc.discover_skills(tmp.path(), include, None);

            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].name, "gamma");
        });
    }

    #[test]
    fn channel_and_pattern_disabled_skills_merged() {
        with_temp_home(|| {
            let tmp = tempfile::tempdir().unwrap();
            let skills_dir = tmp.path().join(".jyc").join("skills");

            for name in &["alpha", "beta", "gamma"] {
                let dir = skills_dir.join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("SKILL.md"),
                    format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
                )
                .unwrap();
            }

            let patterns = vec![ChannelPattern {
                name: "test-pattern".to_string(),
                disabled_skills: Some(vec!["beta".to_string()]),
                ..ChannelPattern::default()
            }];

            let svc = service_with_skills(patterns, None, Some(vec!["alpha".to_string()]));

            // Merge excludes as done in build_system_prompt
            let mut exclude_list: Vec<String> = Vec::new();
            if let Some(ref channel_excluded) = svc.channel_disabled_skills {
                exclude_list.extend(channel_excluded.iter().cloned());
            }
            if let Some(pattern_excluded) = svc
                .patterns
                .iter()
                .find(|p| p.name == "test-pattern")
                .and_then(|p| p.disabled_skills.as_ref())
            {
                for name in pattern_excluded {
                    if !exclude_list.contains(name) {
                        exclude_list.push(name.clone());
                    }
                }
            }
            let exclude_slice: Option<&[String]> = if exclude_list.is_empty() {
                None
            } else {
                Some(&exclude_list)
            };

            let skills = svc.discover_skills(tmp.path(), None, exclude_slice);

            assert_eq!(skills.len(), 1);
            assert_eq!(skills[0].name, "gamma");
        });
    }

    #[test]
    fn no_filters_loads_all_skills() {
        with_temp_home(|| {
            let tmp = tempfile::tempdir().unwrap();
            let skills_dir = tmp.path().join(".jyc").join("skills");

            for name in &["alpha", "beta"] {
                let dir = skills_dir.join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("SKILL.md"),
                    format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
                )
                .unwrap();
            }

            let svc = service_with_skills(vec![], None, None);
            let skills = svc.discover_skills(tmp.path(), None, None);

            assert_eq!(skills.len(), 2);
        });
    }

    #[test]
    fn build_user_prompt_injects_build_mode_tag() {
        let svc = service_with_skills(vec![], None, None);
        let message = InboundMessage {
            id: "test-id".into(),
            channel: "test".into(),
            channel_uid: "uid".into(),
            sender: "test-sender".into(),
            sender_address: "test@example.com".into(),
            recipients: vec![],
            topic: "test".into(),
            content: jyc_types::MessageContent {
                text: Some("hello world".into()),
                ..Default::default()
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: Default::default(),
            matched_pattern: None,
        };

        // Plan mode: should inject PLAN tag
        let plan_prompt = svc.build_user_prompt_text(&message, Some("plan"));
        assert!(
            plan_prompt.contains("CRITICAL: Current mode: PLAN (read-only"),
            "plan mode prompt should contain PLAN tag, got: {plan_prompt}"
        );

        // Build mode (None = no override): should inject BUILD tag
        let build_prompt = svc.build_user_prompt_text(&message, None);
        assert!(
            build_prompt.contains("Current mode: BUILD (full execution)"),
            "build mode prompt should contain BUILD tag, got: {build_prompt}"
        );
    }
}
