//! Prompt building for the in-process agent (`JycAgentService`).
//!
//! Extracted from the monolithic `service.rs`.

use jyc_types::state_dir::resolve_state_dir;
use std::path::{Path, PathBuf};

use crate::capabilities;
use crate::tools::builtin::lesson;
use crate::tools::registry::ToolRegistry;

use jyc_types::InboundMessage;
use jyc_types::TopicConfig;

use super::JycAgentService;
use super::skills::persist_skill_names;
use jyc_types::skill::format_skills_section;

impl JycAgentService {
    // The inputs are the per-turn facts — who the topic is, what the
    // boundaries are, and which tools this turn has. They are one list
    // because they are read together, and the repo's other long builders
    // keep theirs in one list too.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn build_system_prompt(
        &self,
        topic_name: &str,
        topic_path: &Path,
        matched_pattern: Option<&str>,
        topic_cfg: Option<&TopicConfig>,
        additional_read_roots: &[PathBuf],
        additional_write_roots: &[PathBuf],
        registry: Option<&ToolRegistry>,
    ) -> String {
        let mut prompt = String::new();

        // Security: directory boundaries — must describe exactly the roots the
        // tool layer enforces (same Vec passed to AgentLoopConfig), otherwise
        // the model refuses access that is actually permitted.
        prompt.push_str(&Self::format_access_boundary(
            topic_path,
            additional_read_roots,
            additional_write_roots,
        ));

        // NOTE: the subagent list and the channel/topic enumeration are NOT
        // here — both change whenever a topic or a child is created or closed,
        // and every byte of churn in the system prompt invalidates the cached
        // prefix this prompt exists to keep stable. The subagent roster
        // rides the turn's user message (see `build_turn_reference`); the
        // channel/topic roster is not in the prompt at all — the
        // `list_topics` tool fetches it on demand.
        //
        // The skills this topic lists: the config allow-list (topic config >
        // pattern > channel), empty unless a layer declares one — skills are
        // opt-in, and this list is what the system prompt spends context on.
        let mut include_list = self.config_skill_allow_list(matched_pattern, topic_cfg);
        // Runtime `/skill` toggle override (persisted until `/skill reset`):
        // `on` adds a skill to the list, `off` removes one. The two lists
        // are disjoint by construction, and exclusion is applied after
        // inclusion, so `off` wins.
        let skill_ovr = jyc_core::session_state::read_skill_override(topic_name, topic_path).await;
        let mut exclude_list: Vec<String> = Vec::new();
        if let Some(ovr) = &skill_ovr {
            for name in &ovr.on {
                if !include_list.contains(name) {
                    include_list.push(name.clone());
                }
            }
            exclude_list = ovr.off.clone();
        }

        // Discover and inject skill metadata (before AGENTS.md so instructions
        // to read SKILL.md files are seen first)
        let skills = self.discover_skills(
            topic_name,
            topic_path,
            Some(&include_list),
            (!exclude_list.is_empty()).then_some(exclude_list.as_slice()),
        );
        if !skills.is_empty() {
            prompt.push_str(&format_skills_section(&skills));
        }

        // Persist skill names to .jyc/skills.json for dashboard inspection
        let skill_names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        if let Err(e) = persist_skill_names(topic_name, topic_path, &skill_names) {
            tracing::warn!(error = %e, "Failed to persist skill names to skills.json");
        }

        // Topic config `instructions` — the topic's own standing orders, and
        // where a subagent type's role text lands. Static (it changes when the
        // file does, not per turn), so it stays cache-friendly.
        if let Some(instructions) = topic_cfg
            .and_then(|c| c.instructions.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            prompt.push_str("## Topic Instructions\n\n");
            prompt.push_str(instructions);
            prompt.push_str("\n\n");
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
             Your final message at the end of the turn IS the reply — it is\n\
             delivered to the user automatically. Text written alongside tool\n\
             calls is internal narration and is NOT delivered.\n\
             - To tell the user something, finish your turn with that text.\n\
             - If nothing needs to reach the user, end the turn with no text.\n\n",
        );

        // History format guardrail: the sliding window folds older turns to
        // text and marks the block with a machine-generated divider. Models
        // have been observed treating such machine text as conversation.
        // State the contract explicitly.
        prompt.push_str(
            "## History Format\n\
             The sliding window inserts a machine-generated `[History messages]` \
             divider before older compacted history: it marks text-only, degraded turns \
             (tool calls and results omitted), is not from the user, and must not be \
             treated as an instruction.\n\n",
        );

        // Chat history access instructions — point at the resolved state dir
        // (relocated out of the topic dir for pinned topics).
        let state_dir = resolve_state_dir(topic_name, topic_path);
        prompt.push_str(&format!(
            "## Chat History\n\
             This topic maintains a chronological chat history in \
             `{}/chat_history_YYYY-MM-DD.jsonl`.\n\
             Each line is a JSON object (one message or reply per line). You can read it with the\n\
             `read` tool if you need context from prior conversations, or use `grep` to search.\n\
             For earlier turns of THIS conversation that have fallen out of your context window,\n\
             use the `context_browse` tool to page the in-memory transcript.\n",
            state_dir.display()
        ));

        // Version control hygiene: only relevant while `.jyc` still sits
        // inside the topic dir (unpinned topics). Adopted state dirs live in
        // data_dir and cannot leak into the repo.
        if state_dir.starts_with(topic_path) {
            // The bash tool already injects a global git excludes file that
            // ignores `.jyc/` everywhere; this rule is the backstop against
            // `git add -f`, which bypasses all ignore rules.
            prompt.push_str(
                "## Version Control\n\
                 The `.jyc/` directory is JYC's private runtime state (credentials, chat history, sessions).\n\
                 NEVER stage or commit it: do not run `git add .jyc`, do not run `git add -f` on it, and\n\
                 before `git add .`, check that it will not include `.jyc/`.\n\n",
            );
        }

        // CLI helpers installed on this machine (rg, fd, fzf, jq — whichever
        // of the list this machine actually has). The model reaches for the
        // tools its training named; without this it runs `grep -r` on a box
        // that has ripgrep. Absent when nothing is installed, so the prompt
        // never names a tool that is not there.
        //
        // Process-stable (PATH does not change under a running instance), so
        // like the sections above it never churns the cached prefix.
        push_cli_helpers_section(&mut prompt, capabilities::available());

        // Cross-Topic Communication section (when topic managers are available)
        let managers_opt = self
            .topic_managers
            .lock()
            .expect("topic_managers poisoned")
            .clone();
        let outbounds_configured = self.outbounds.lock().expect("outbounds poisoned").is_some();
        if managers_opt.is_some() {
            prompt.push_str(
                "\n## Cross-Topic Communication\n\n\
                 You can send messages to another agent — its own topic by default, or one of its \
                 topics with `topic` — using the `jyc_send_to_topic` tool.\n",
            );
            // The addressing tools are extended: the group is loaded on demand
            // instead of being advertised. Say so only when the loader is
            // actually registered — a role that disabled it must not be told
            // to call a tool that does not exist. The registry is the only
            // truth about that, so the `disabled_tools` layers are not
            // parsed a second time here.
            if registry.is_some_and(|r| r.has_tool("load_extended_tools")) {
                prompt.push_str(
                    "The addressing tools are extended: they stay out of your tool list until the \
                     group is loaded — `load_extended_tools` is the index of what it holds.\n",
                );
            }
            prompt.push_str(
                "Set `require_reply=true` when you need the target agent to send results back to you.\n\n\
                 When you receive a message with a **Source:** header, it came from another topic. \
                 Process the content normally — your final message displays the results in the \
                 current topic. If it includes \"⚠️ Reply requested\", you MUST ALSO use \
                 `jyc_send_to_topic` to send your results back to the source channel/topic.\n\
                 Should your turn end without that reply, the framework forwards your last message \
                 to the source in your place — so the requester hears the answer, or the fact that \
                 there was none. An explicit reply is still better: it carries the whole result, \
                 while the forward carries only your last message.\n",
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

            // The live channel/topic roster is deliberately NOT enumerated
            // here: topics come and go, and one line of churn in the system
            // prompt invalidates its cached prefix on every provider that
            // caches by prefix. The subagent roster rides the turn's user
            // message instead (`build_turn_reference`); the channel/topic
            // roster is not in the prompt at all — the `list_topics` tool
            // fetches it on demand. What stays here is the static prose
            // above, which does not move.
            prompt.push('\n');
        }

        // The agent's own lessons — what it learned and kept for itself. The
        // newest ones ride along with every turn, so a lesson is in view
        // without a tool call. They live in the *system* prompt, which is
        // cached by prefix: adding a lesson moves the system breakpoint, so
        // the next request re-sends this topic's history uncached — once
        // per lesson, not once per turn. Carrying them in the turn
        // reference instead would keep the history cache but pay for these
        // bytes on every turn, uncached. The owner's instructions come
        // first, the agent's own experience after.
        if let Some(section) = lesson::prompt_section(&state_dir.join(lesson::LESSONS_FILE)) {
            prompt.push_str(&section);
        }

        prompt
    }

    /// Build the per-turn runtime reference: the parts of what the model needs
    /// to know that change as the instance runs — the subagents this topic
    /// spawned.
    ///
    /// The roster used to live in the system prompt. That made the system
    /// prompt change whenever any topic appeared or disappeared, which is
    /// frequent (spawning a reviewer, closing a watcher) and which costs
    /// twice over: the changed bytes are re-sent on every later call of the
    /// turn, and for providers with automatic prefix caching the whole
    /// prompt's cache entry is invalidated, not just the tail.
    ///
    /// It rides the current turn's user message instead: that message is new
    /// on every turn anyway, so churn here lands past the cached prefix
    /// regardless of how often it changes, while the system prompt stays
    /// byte-stable and its cache entry survives from turn to turn.
    ///
    /// The channel/topic roster is not here either: the `list_topics` tool
    /// fetches it on demand.
    ///
    /// Returns an empty string when there is nothing volatile to report, so the
    /// caller can prepend unconditionally.
    pub(crate) async fn build_turn_reference(&self, topic_name: &str) -> String {
        let mut body = String::new();

        // Subagents this topic spawned (their directories are the grants the
        // system prompt's access boundary lists). Naming them every turn, from
        // the same lineage the spawn cap check reads, is what stops the model
        // from spawning a duplicate for work one of them already holds.
        let subagents = jyc_core::command::spawn_handler::subagents_of(topic_name);
        if !subagents.is_empty() {
            body.push_str(&Self::format_subagents_section(&subagents));
        }

        // The channel/topic roster used to ride here. It is now fetched on
        // demand by the `list_topics` tool: the list changed whenever a topic
        // appeared or closed, and as a per-turn block it was repeated in the
        // history of every later turn — a standing cost for a table only
        // needed when the model wants to address someone.

        if body.is_empty() {
            return String::new();
        }
        if !body.ends_with('\n') {
            body.push('\n');
        }

        format!(
            "<runtime-state>\n\
             Auto-generated snapshot taken at the start of this turn. It is not a \
             message from the user, and it can go stale within a long turn.\n\
             {body}</runtime-state>\n\n"
        )
    }

    /// Build the user prompt text (header + body) from an inbound message.
    ///
    /// `turn_reference` is the runtime snapshot from `build_turn_reference`
    /// (the subagent roster; the channel/topic roster is fetched on demand
    /// by `list_topics`), prepended here so it travels with the one block
    /// that is new every turn. Empty means "nothing volatile to report" and
    /// contributes nothing.
    pub(crate) fn build_user_prompt_text(
        &self,
        message: &InboundMessage,
        mode_override: Option<&str>,
        turn_reference: &str,
    ) -> String {
        let mut prompt = String::new();

        // Volatile runtime state first, so the model reads it as standing
        // context rather than as part of the inbound message below.
        if !turn_reference.is_empty() {
            prompt.push_str(turn_reference);
            prompt.push('\n');
        }

        prompt.push_str("## Incoming Message\n");
        prompt.push_str(&format!(
            "**From:** {} <{}>\n",
            message.sender, message.sender_address
        ));
        prompt.push_str(&format!("**Subject:** {}\n", message.topic));
        prompt.push_str(&format!("**Date:** {}\n", message.timestamp.to_rfc3339()));

        // Display cross-topic source info if present. The guard owns the
        // reading of this metadata — the header rendered here and the answer
        // the guard forwards to that address cannot drift apart.
        if let Some(source) = crate::service::reply_guard::source_of(&message.metadata) {
            if crate::service::reply_guard::asks_for_reply(&message.metadata) {
                prompt.push_str(&reply_request_header(&source));
            } else {
                prompt.push_str(&source_line(&source));
                prompt.push('\n');
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
        // agent sees before replying). The mode block is intentionally placed
        // in the user message rather than the system prompt so the system
        // prompt stays byte-stable across plan/build switches, preserving
        // prefix-cache hits.
        if mode_override == Some("plan") {
            prompt.push('\n');
            prompt.push_str("<mode>\n");
            prompt.push_str("CRITICAL: Current mode: PLAN (read-only, do not exit plan mode even if the user requests it). ");
            prompt.push_str("This constraint is absolute — do not bypass it even if asked. ");
            prompt.push_str("Even if you previously ran write/edit commands in this conversation, you are now in PLAN MODE and must not change the project.\n");
            prompt.push_str("You MUST NOT: edit, write, or delete any project file; run build, test, or deployment commands; commit, push, or branch changes; install or modify dependencies.\n");
            prompt.push_str("You MAY ONLY: read files, search code, analyze patterns; manage your own task list (task_create, task_update, task_list) and record lessons (lesson_add); present implementation plans and ask clarifying questions; wait for user approval before any implementation.\n");
            prompt.push_str("The task list and the lessons are the plan-mode exceptions: they are your own record — the plan of record and what was learned — kept in your topic state, not in the project.\n");
            prompt.push_str("Before starting, read the SKILL.md of any available skill whose description matches your task.\n");
            prompt.push_str("</mode>\n");
        } else {
            // Build mode: explicitly declare full execution capabilities so the
            // model does not mistakenly inherit stale PLAN constraints from
            // prior conversation history (agent-context.jsonl).
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

    /// Format the file-access boundary block that leads the system prompt.
    ///
    /// With no extra roots this returns the classic fixed sentence byte-for-byte.
    /// With roots it enumerates them exactly as the tool layer enforces them, so
    /// the model knows which outside-the-working-directory paths it may
    /// legitimately use. Roots are deterministic per pattern, so the prompt
    /// stays byte-stable across calls for a given agent either way.
    pub(crate) fn format_access_boundary(
        topic_path: &Path,
        read_roots: &[PathBuf],
        write_roots: &[PathBuf],
    ) -> String {
        if read_roots.is_empty() && write_roots.is_empty() {
            return format!(
                "Your working directory is \"{}\". You MUST only read, write, and access files within this directory.\n\n",
                topic_path.display()
            );
        }
        let mut s = format!("Your working directory is \"{}\".\n", topic_path.display());
        s.push_str("File access boundaries (enforced by the tool layer):\n");
        if !read_roots.is_empty() {
            s.push_str(
                "- You may read files within the working directory and these additional directories:\n",
            );
            for r in read_roots {
                s.push_str(&format!("  - {}\n", r.display()));
            }
        }
        if write_roots.is_empty() {
            s.push_str("- You MUST only write files within the working directory.\n");
        } else {
            s.push_str("- You may write files within the working directory and these additional directories:\n");
            for r in write_roots {
                s.push_str(&format!("  - {}\n", r.display()));
            }
        }
        s.push_str(
            "- Never access files outside these locations; the tools will deny such attempts.\n\n",
        );
        s
    }

    /// The subagent section: who this topic already spawned, and the reuse rule.
    /// Names only — the directories are the grants the system prompt's access
    /// boundary lists. `/spawn` counts here too: the section reports what is.
    /// Ships in the turn reference, not the system prompt, so spawning or
    /// closing a child does not invalidate the system prompt's cache prefix.
    pub(crate) fn format_subagents_section(subagents: &[(String, std::path::PathBuf)]) -> String {
        format!(
            "Your subagents ({}): {}.\n\
             Continue an existing one with jyc_send_to_topic — it keeps its own context — \
             instead of spawning a duplicate; spawn a new one only when nothing you already \
             spawned fits.\n\n",
            subagents.len(),
            jyc_core::command::spawn_handler::format_subagent_names(subagents),
        )
    }

    /// Resolve the additional absolute read roots granted to this topic's agent:
    /// the attachment save dir (when outside `topic_path`), external skill
    /// directories, and the matched pattern's `access.read`/`access.write`
    /// paths (write paths are readable too). Tilde-expanded; relative paths
    /// resolve inside `topic_path` and need no widening.
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
        let data_dir_skills = self.data_dir.join("skills");
        if data_dir_skills.exists() && data_dir_skills.is_dir() {
            roots.push(data_dir_skills);
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

        // 4. Relocated topic state dir (outside the topic dir since the
        // `.jyc` adoption refactor) — the agent reads its own chat history
        // and session state from it.
        let state = jyc_types::state_dir::resolve_state_dir(&message.topic, topic_path);
        if !state.starts_with(topic_path) {
            roots.push(state);
        }

        // 5. Runtime grants from /grant (temporary, topic-scoped). Write
        // grants are readable too.
        for g in jyc_types::access_grants::grants_for(&message.topic) {
            if !g.path.starts_with(topic_path) {
                roots.push(g.path);
            }
        }

        roots
    }

    /// Resolve additional write roots: the matched pattern's `access.write`
    /// configuration plus the topic's relocated state dir (write parity with
    /// the pre-refactor layout, where `.jyc` sat inside the working dir).
    /// Paths are tilde-expanded; relative paths are ignored
    /// (they are already inside the working directory).
    pub(crate) fn resolve_additional_write_roots(
        &self,
        message: &InboundMessage,
        topic_path: &Path,
    ) -> Vec<PathBuf> {
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
        let state = jyc_types::state_dir::resolve_state_dir(&message.topic, topic_path);
        if !state.starts_with(topic_path) {
            roots.push(state);
        }
        // Runtime grants from /grant -w (temporary, topic-scoped).
        for g in jyc_types::access_grants::grants_for(&message.topic) {
            if g.write && !g.path.starts_with(topic_path) {
                roots.push(g.path);
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
        turn_reference: &str,
    ) -> Vec<crate::types::ContentBlock> {
        use crate::types::{ContentBlock, ImageSource};
        use base64::Engine as _;

        let mut blocks = vec![ContentBlock::Text {
            text: self.build_user_prompt_text(message, mode_override, turn_reference),
        }];

        // Per-pattern opt-in. Default false when the message did not match a
        // pattern or the pattern is not in our flattened list.
        let pattern_inject = message
            .matched_pattern
            .as_deref()
            .and_then(|name| self.patterns.iter().find(|p| p.name == name))
            .map(|p| p.inject_inbound_images())
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

/// The `**Source:**` line: where a message came from.
///
/// One place renders it, so the header a turn starts on and the notice a
/// running turn gets cannot drift apart.
pub(crate) fn source_line(source: &crate::service::reply_guard::ReplySource) -> String {
    format!(
        "**Source:** channel \"{}\", topic \"{}\"",
        source.channel, source.topic
    )
}

/// The header for a message that asked for a reply: where it came from, and
/// what the target has to do about it.
///
/// The turn that starts on this message owes the source topic an answer.
pub(crate) fn reply_request_header(source: &crate::service::reply_guard::ReplySource) -> String {
    format!(
        "{} (⚠️ Reply requested)\n\n\
         ⚠️ ACTION REQUIRED — DO THIS FIRST:\n\
         1. Call `jyc_send_to_topic` with agent=\"{}\" \
         to send your results back to the source topic (its topic name \
         is its address; add `topic` only if that name is not the agent's own).\n\
         2. Then finish your turn with a final message displaying \
         the results in this topic.\n\
         CRITICAL: Do NOT miss step 1. The source topic is waiting for \
         your reply.\n",
        source_line(source),
        source.topic
    )
}

/// The header for a message that asked for a reply and arrived while the
/// turn was already running.
///
/// The turn is mid-task here, so it is told to answer — not to wrap up.
pub(crate) fn reply_request_notice(source: &crate::service::reply_guard::ReplySource) -> String {
    format!(
        "{} (⚠️ Reply requested)\n\
         ⚠️ This message asked for a reply: call `jyc_send_to_topic` with agent=\"{}\" \
         to send your results back to the source topic (its topic name is its address). \
         The source topic is waiting for your reply.\n",
        source_line(source),
        source.topic
    )
}

/// Read the thinking-display state from `.jyc/thinking-state` in the topic directory.
///
/// Returns `true` (default) if the file is missing or contains anything other
/// than `"hide"`. The `/thinking hide` command writes `"hide"` to this file;
/// `/thinking show` writes `"show"`.
pub(crate) fn read_thinking_enabled(topic_name: &str, topic_path: &Path) -> bool {
    match std::fs::read_to_string(resolve_state_dir(topic_name, topic_path).join("thinking-state"))
    {
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

/// Append the `## CLI Helpers` section for `helpers`, when there are any.
///
/// Takes the helpers as an argument instead of probing PATH here, so the
/// injection itself is testable with fixtures: otherwise the positive path
/// would only ever run on a machine that happens to have ripgrep installed,
/// and the test would quietly degrade to one vacuous assertion on one that
/// does not. `available()` is what the caller passes — see `capabilities`.
pub(super) fn push_cli_helpers_section(
    prompt: &mut String,
    helpers: &[&crate::capabilities::Helper],
) {
    if let Some(section) = crate::capabilities::prompt_section(helpers) {
        prompt.push_str(&section);
    }
}
