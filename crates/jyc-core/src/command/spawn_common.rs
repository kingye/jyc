//! What every spawn shares: the state a child inherits, the name it may hold,
//! and the shape a refusal is reported in.
//!
//! `/spawn` (the command) and `jyc_spawn_subagent` (the tool) are one
//! implementation with two triggers — [`super::spawn_handler`] holds that core
//! — and this module holds the pieces both triggers read. `/fork` used to be a
//! second implementation here; it is gone, folded into `/spawn -s`, which is
//! the same shared-directory shape with the same lineage.

use std::path::Path;

use anyhow::Result;

use super::handler::CommandResult;
use crate::session_state;
use crate::topic_manager::TopicManager;

/// State files a spawned topic inherits from its parent.
///
/// An allow-list, not a directory walk: whatever is absent here stays in the
/// parent. The allow-list is the *settings* a child needs to be itself — the
/// conversation and the transcript are a separate question, decided by `fresh`
/// (see [`seed_state`]). What is deliberately missing, and why, is worth knowing
/// before adding to it:
///
/// - `agent-session.json` — the token/cost meter. The usage it counts was
///   billed to the parent, and a child must not inherit a bill it did not run
///   up (`record_token_usage` adds to whatever is on disk). Leaving it out
///   starts the child un-metered; the first response re-seeds `context_window`.
/// - `activity.jsonl`, `wire-payload.jsonl` — per-topic logs.
/// - `topic-name`, `topic-path`, `topic-meta.json`, `thread-meta.json` — topic
///   identity and channel routing. `set_topic_path` writes the first two for
///   the new topic; the routing meta must not point the child at the parent's
///   channel or thread.
/// - `jobs/` — a scheduled job fires by injecting into *its* topic, so a copy
///   would make the child run the parent's jobs too. Directories are skipped.
/// - Anything else in the state dir belongs to the user, not to the framework.
const SEED_FILES: &[&str] = &[
    CONVERSATION_FILE,     // the running context: the conversation
    "backlog.jsonl",       // the topic's own backlog
    "pattern",             // which agent pattern runs the topic
    "mode-override",       // plan/build mode
    "plan-model-override", // per-mode model choice
    "build-model-override",
    "model-override", // the mode-agnostic fallback the chain reads next
    session_state::CONTEXT_STRATEGY_FILE, // `/context`
    session_state::MCP_OVERRIDE_FILE, // `/mcp`
    session_state::SKILL_OVERRIDE_FILE, // `/skill`
    "skills.json",
];

/// The running conversation, as opposed to the settings around it. Aliased to
/// the shared name so seeding/`fresh` logic follows the journal format.
const CONVERSATION_FILE: &str = session_state::AGENT_CONTEXT_FILE;

/// Chat history is one file per day, so it is matched by prefix instead of
/// listed. Together with [`CONVERSATION_FILE`] this is what `fresh` leaves
/// behind: a new topic can run in the parent's directory and inherit every
/// setting without inheriting the parent's train of thought.
const CHAT_HISTORY_PREFIX: &str = "chat_history_";

/// `dir/<topic>-2`, `dir/<topic>-3`, … — the first candidate that is neither a
/// registered topic nor a directory that already exists under `dir`. If all of
/// them are taken, return `<topic>-2` so the caller reports the duplicate
/// rather than inventing a name nobody asked for. `/spawn` passes its agents
/// root as `dir` (default spawn dirs and shared children's state live there).
///
/// ponytail: the probe and the `create_dir_all` inside `set_topic_path` are
/// not one transaction, so two topics spawning to the same automatic name at
/// the same instant would share a directory. Unreachable at typing speed; a
/// lock would cost more than the case it covers.
pub(super) async fn next_free_name(tm: &TopicManager, topic: &str, dir: &Path) -> String {
    for n in 2..100u32 {
        let candidate = format!("{topic}-{n}");
        if tm.topic_path(&candidate).await.is_none() && !dir.join(&candidate).exists() {
            return candidate;
        }
    }
    format!("{topic}-2")
}

/// Why `name` cannot be a topic name, if it cannot.
///
/// Keeps the shape the router itself produces (`plan-197`, `上海天气`,
/// `英国旅行2026`): unicode is fine, path syntax is not — the name becomes a
/// directory name and a routing key, and `/` or `..` would escape the agent
/// subtree. Stricter than `post_topic`'s check in `jyc-inspect` (path syntax
/// only) on purpose, because here the name also picks a parent directory; if
/// anything outside `command` ever needs the same rule, lift it to
/// `jyc-types` rather than copying it. `/spawn` shares it via `pub(super)`.
pub(super) fn invalid_name(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("the name is empty");
    }
    if name.len() > 64 {
        return Some("the name is too long (64 bytes max)");
    }
    if name.starts_with('.') {
        return Some("the name must not start with '.'");
    }
    // A leading `-` is never a name: it is a flag the caller mistyped
    // (`--inherit=false` is not one, and would otherwise create a topic by that
    // name), so it is refused rather than silently taken as one. Derived names
    // are held to this too: a parent whose own name starts with `-` needs an
    // explicit name for its child.
    if name.starts_with('-') {
        return Some("the name must not start with '-'");
    }
    // `~` is a destination shorthand, not a name: left allowed, the spawn tool
    // would take `name = "~"` as a path and run the child in `$HOME`.
    if name.starts_with('~') {
        return Some("the name must not start with '~'");
    }
    if name.contains('/') || name.contains('\\') {
        return Some("the name must not contain a path separator");
    }
    if name.contains("..") {
        return Some("the name must not contain '..'");
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ' | ':'))
    {
        return Some("the name may only use letters, digits, '-', '_', '.', ':' or spaces");
    }
    None
}

/// Copy the inheritable state of `parent` into `child` and record where the
/// new topic came from under `marker` (`spawned-from`). Parent files that do
/// not exist are simply not copied — a young topic has few of them.
///
/// `fresh` skips the conversation ([`CONVERSATION_FILE`]) and the transcript
/// ([`CHAT_HISTORY_PREFIX`]): the child gets every *setting* the parent
/// configured — pattern, mode, model/context/MCP/skill overrides — and a head
/// of its own. This is the default for both triggers (`/spawn`, the
/// `jyc_spawn_subagent` tool): inheriting a long development conversation is
/// expensive and anchors the child to a line of reasoning nobody asked it to
/// continue, which is exactly wrong for the common case — a reviewer or a
/// second worker starting on a task of its own. `--inherit` / `inherit_context`
/// opts back into the copy. The task list is never among them, `--inherit`
/// included: a child plans its own work. #810 seeded it so a child could
/// continue the parent's plan, but the same seed ran for the spawn tool, where
/// the parent's plan is somebody else's.
pub(super) async fn seed_state(
    parent: &Path,
    child: &Path,
    from: &str,
    marker: &str,
    fresh: bool,
) -> Result<usize> {
    let mut copied = 0;
    let mut entries = tokio::fs::read_dir(parent).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        let transcript =
            file_name.starts_with(CHAT_HISTORY_PREFIX) && file_name.ends_with(".jsonl");
        let conversation = transcript || file_name == CONVERSATION_FILE;
        if fresh && conversation {
            continue;
        }
        let inherit = SEED_FILES.contains(&file_name) || transcript;
        if inherit && tokio::fs::copy(&path, child.join(file_name)).await.is_ok() {
            copied += 1;
        }
    }
    tokio::fs::write(child.join(marker), from).await?;
    Ok(copied)
}

/// Give a freshly seeded child the model its parent is running.
///
/// [`seed_state`] copies the parent's override *files*, which covers the case
/// where the parent's model is one of them. It does not cover the case where the
/// model comes from a config or pattern layer — there is no file to copy, so the
/// child falls through to whatever *its own* chain resolves to, which is not the
/// parent's model when the two run different modes (a typed subagent's
/// `mode = "plan"` lands in `.jyc/config.toml`, so its plan-mode lookup is the
/// topic-config layer rather than the file its build-mode parent read).
///
/// Written to `model-override`, the mode-agnostic fallback: the chain reads it
/// whenever the mode-specific file for the child's own mode is missing, so
/// nothing about the child's mode naming can hide it. A mode-specific file the
/// child did inherit still wins over it — that file is the parent's own explicit
/// choice for that same mode.
///
/// `model` is [`crate::session_state::resolve_effective_model`]'s answer for the
/// *parent*; `None` (nothing to resolve for it) writes nothing, which is the
/// behavior a child had before this existed.
pub(super) async fn seed_start_model(state_dir: &Path, model: Option<&str>) -> bool {
    let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else {
        return false;
    };
    write_model_override(state_dir, "model-override", model).await
}

/// Write a resolved model into one of the runtime override files — the ones the
/// resolution chain reads above a topic config (`model-override`,
/// `plan-model-override`, `build-model-override`).
pub(super) async fn write_model_override(state_dir: &Path, file: &str, model: &str) -> bool {
    tokio::fs::write(state_dir.join(file), model.trim())
        .await
        .is_ok()
}

/// A refused command: reported to the user, never an `error` (nothing broke).
/// `/spawn` refuses the same way.
pub(super) fn fail(message: String) -> CommandResult {
    CommandResult {
        success: false,
        message,
        error: None,
        append_body: None,
    }
}
