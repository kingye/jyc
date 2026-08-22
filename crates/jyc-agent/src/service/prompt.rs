//! Prompt building for the in-process agent (`JycAgentService`).
//!
//! Extracted from the monolithic `service.rs`.

use std::path::{Path, PathBuf};

use jyc_types::InboundMessage;

use super::JycAgentService;
use super::skills::{format_skills_section, persist_skill_names};

impl JycAgentService {
    pub(crate) async fn build_system_prompt(
        &self,
        topic_path: &Path,
        matched_pattern: Option<&str>,
    ) -> String {
        let mut prompt = String::new();

        // Security: directory boundaries
        prompt.push_str(&format!(
            "Your working directory is \"{}\". You MUST only read, write, and access files within this directory.\n\n",
            topic_path.display()
        ));

        // Read mode override early (plan mode injected at end for recency)
        let mode_override = jyc_core::session_state::read_mode_override(topic_path).await;
        tracing::info!(
            topic = %topic_path.display(),
            mode = ?mode_override,
            "Read mode override"
        );

        // Resolve skill filters: pattern > channel > none
        let pattern =
            matched_pattern.and_then(|name| self.patterns.iter().find(|p| p.name == name));

        // Mode resolution chain: .jyc/mode-override file > pattern.mode > default "build"
        let mode_override = mode_override.or_else(|| pattern.and_then(|p| p.mode.clone()));

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
        let skills = self.discover_skills(topic_path, include_list, exclude_slice);
        if !skills.is_empty() {
            prompt.push_str(&format_skills_section(&skills));
        }

        // Persist skill names to .jyc/skills.json for dashboard inspection
        let skill_names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        if let Err(e) = persist_skill_names(topic_path, &skill_names) {
            tracing::warn!(error = %e, "Failed to persist skill names to skills.json");
        }

        // Load AGENTS.md if present in the working directory
        let agents_md = topic_path.join("AGENTS.md");
        if agents_md.exists()
            && let Ok(content) = std::fs::read_to_string(&agents_md)
        {
            prompt.push_str("## Project Instructions (from AGENTS.md)\n\n");
            prompt.push_str(&content);
            prompt.push_str("\n\n");
        }

        // Load repo/AGENTS.md if present (for GitHub channel)
        let repo_agents_md = topic_path.join("repo").join("AGENTS.md");
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
             When you have your answer ready, use the jyc_reply_message tool:\n\
             - `message`: Your reply text\n\
             - `attachments`: Optional filenames to attach from the working directory\n\
             - `stop_after` (boolean, default true): Whether to stop working after this reply\n\
             CRITICAL: Always use the jyc_reply_message tool to send your reply.\n\n\
             **Final reply**: Set `stop_after: true` (or omit it). After a successful reply with\n\
             stop_after=true, STOP immediately. Do NOT call any other tools.\n\
             **Progress update**: For long-running tasks, send periodic progress replies with\n\
             `stop_after: false`. Each reply is a checkpoint — you will continue working\n\
             afterward. Use this when you have substantive progress to report.\n\n",
        );

        // History format guardrail: the sliding window renders past tool
        // calls as text annotations. Models have been observed MIMICKING
        // this format in their reply text ("(incl. followed tool calls:
        // jyc_reply_message(...) → Reply sent)") and believing they replied
        // — while no tool call actually happened and the narration was
        // delivered as the fallback reply. State the contract explicitly.
        prompt.push_str(
            "## History Format\n\
             Past turns in your context may show tool calls rendered as text: \
             `(incl. followed tool calls: name(args) → result)`. These are READ-ONLY \
             summaries of what already happened. Writing this format as your message \
             text does NOT invoke any tool — only real tool calls do.\n\n",
        );

        // Chat history access instructions
        prompt.push_str(
            "## Chat History\n\
             This topic maintains a chronological chat history in `.jyc/chat_history_YYYY-MM-DD.jsonl`.\n\
             Each line is a JSON object (one message or reply per line). You can read it with the\n\
             `read` tool if you need context from prior conversations, or use `grep` to search.\n\
             For earlier turns of THIS conversation that have fallen out of your context window,\n\
             use the `context_browse` tool to page the in-memory transcript.\n",
        );

        // Version control hygiene: `.jyc/` is JYC's private runtime state
        // (credentials, chat history, sessions) and must never be committed.
        // The bash tool already injects a global git excludes file that
        // ignores `.jyc/` everywhere; this rule is the backstop against
        // `git add -f`, which bypasses all ignore rules.
        prompt.push_str(
            "## Version Control\n\
             The `.jyc/` directory is JYC's private runtime state (credentials, chat history, sessions).\n\
             NEVER stage or commit it: do not run `git add .jyc`, do not run `git add -f` on it, and\n\
             before `git add .`, check that it will not include `.jyc/`.\n\n",
        );

        // Cross-Topic Communication section (when topic managers are available)
        let tm_map_opt = self
            .topic_managers
            .lock()
            .expect("topic_managers poisoned")
            .clone();
        let outbounds_configured = self.outbounds.lock().expect("outbounds poisoned").is_some();
        if let Some(ref tm_map) = tm_map_opt {
            prompt.push_str(
                "\n## Cross-Topic Communication\n\n\
                 You can send messages to topics in other channels using the `jyc_send_to_topic` tool.\n\
                 Set `require_reply=true` when you need the target agent to send results back to you.\n\n\
                 When you receive a message with a **Source:** header, it came from another topic. \
                 Process the content normally and use `jyc_reply_message` to display results in the \
                 current topic. If it includes \"⚠️ Reply requested\", you MUST ALSO use \
                 `jyc_send_to_topic` to send your results back to the source channel/topic.\n",
            );

            // Note about direct outbound messaging via jyc_send_message
            if outbounds_configured {
                prompt.push_str(
                    "For direct outbound messaging (bypassing agent processing), \
                     use `jyc_send_message` with the optional `channel` parameter set to \
                     a target channel name.\n\
                     `jyc_send_message` sends directly through the channel's outbound adapter \
                     without agent processing. `jyc_send_to_topic` injects into a topic queue \
                     for agent processing.\n\n",
                );
            }

            prompt.push_str("Available channels and their active topics:\n");
            let map = tm_map.lock().await;
            for (channel_name, tm) in map.iter() {
                let channel_type = tm.channel_type();
                prompt.push_str(&format!(
                    "- Channel \"{}\" ({})\n",
                    channel_name, channel_type
                ));
                // List active topics for this channel
                let topics = tm.list_topics().await;
                if topics.is_empty() {
                    prompt.push_str("    (no active topics)\n");
                } else {
                    for topic_info in &topics {
                        prompt.push_str(&format!("  - {}\n", topic_info.name));
                    }
                }
            }
            prompt.push('\n');
        }

        // Plan mode: inject at END for maximum recency before conversation
        if mode_override.as_deref() == Some("plan") {
            tracing::info!(
                topic = %topic_path.display(),
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
                 CRITICAL: Before planning or analyzing, review the Available Skills listed above.\n\
                 Read the full SKILL.md of any skill whose description matches the work you're about to do.\n\
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
                 CRITICAL: Before taking action, review the Available Skills listed above.\n\
                 Read the full SKILL.md of any skill whose description matches the work you're about to do.\n\
                 </system-reminder>\n\n",
            );
        }

        prompt
    }

    /// Build the user prompt text (header + body) from an inbound message.
    pub(crate) fn build_user_prompt_text(
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
        prompt.push_str(&format!("**Date:** {}\n", message.timestamp.to_rfc3339()));

        // Display cross-topic source info if present
        if let Some(src_ch) = message
            .metadata
            .get("source_channel")
            .and_then(|v| v.as_str())
            && let Some(src_th) = message
                .metadata
                .get("source_topic")
                .and_then(|v| v.as_str())
        {
            let require_reply = message
                .metadata
                .get("require_reply")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if require_reply {
                prompt.push_str(&format!(
                    "**Source:** channel \"{}\", topic \"{}\" \
                     (⚠️ Reply requested)\n\n\
                     ⚠️ ACTION REQUIRED — DO THIS FIRST:\n\
                     1. Call `jyc_send_to_topic` with channel=\"{}\", topic=\"{}\" \
                     to send your results back to the source topic.\n\
                     2. Then call `jyc_reply_message` with `stop_after=true` to \
                     display results in this topic.\n\
                     CRITICAL: Do NOT miss step 1. The source topic is waiting for \
                     your reply.\n",
                    src_ch, src_th, src_ch, src_th
                ));
            } else {
                prompt.push_str(&format!(
                    "**Source:** channel \"{}\", topic \"{}\"\n",
                    src_ch, src_th
                ));
            }
        }

        prompt.push('\n');

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
            prompt.push_str("Before starting, read the SKILL.md of any available skill whose description matches your task.\n");
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
            prompt.push_str("Before starting, read the SKILL.md of any available skill whose description matches your task.\n");
            prompt.push_str("</mode>\n");
        }

        prompt
    }

    /// Resolve the additional absolute read-roots for tools that enforce a
    /// path boundary. Returns at most one root: the resolved attachment
    /// save directory (per-pattern override beats global) when it points
    /// outside `topic_path`. Relative values resolve inside `topic_path`
    /// and need no widening.
    ///
    /// Reuses `jyc_core::attachment_storage::resolve_attachment_save_dir`
    /// so the agent's boundary rule never drifts from the channel adapters'
    /// save-location rule.
    pub(crate) fn resolve_additional_read_roots(
        &self,
        message: &InboundMessage,
        topic_path: &Path,
    ) -> Vec<PathBuf> {
        let mut roots = Vec::new();

        // 1. Attachment save directory (if outside topic_path)
        let pattern_cfg = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .and_then(|p| p.attachments.as_ref());
        let cfg = pattern_cfg.or(self.global_inbound_attachments.as_ref());
        let resolved = jyc_core::attachment_storage::resolve_attachment_save_dir(topic_path, cfg);
        if !resolved.starts_with(topic_path) {
            roots.push(resolved);
        }

        // 2. External skill directories (outside topic_path)
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
        if let Some(global_skills) = jyc_utils::paths::global_skills_dir().filter(|d| d.is_dir()) {
            roots.push(global_skills);
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
    pub(crate) fn resolve_additional_write_roots(&self, message: &InboundMessage) -> Vec<PathBuf> {
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
    pub(crate) fn build_user_blocks(
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
}

/// Read the thinking-display state from `.jyc/thinking-state` in the topic directory.
///
/// Returns `true` (default) if the file is missing or contains anything other
/// than `"hide"`. The `/thinking hide` command writes `"hide"` to this file;
/// `/thinking show` writes `"show"`.
pub(crate) fn read_thinking_enabled(topic_path: &Path) -> bool {
    match std::fs::read_to_string(topic_path.join(".jyc").join("thinking-state")) {
        Ok(content) => content.trim() != "hide",
        Err(_) => true,
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
