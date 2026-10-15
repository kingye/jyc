//! Central topic path resolution.
//!
//! A topic has a **home** under the agents dir — `<agents dir>/<agent>/<topic>`
//! for a pipe-routed dynamic topic, `<agents dir>/<key>` for an agent-keyed
//! one (a 1:1 topic, a `/spawn` child). The home owns the topic's identity and
//! keeps the framework's directories for it:
//!
//! - `<home>/.jyc`        state dir ([`state_dir_for`], [`activate_workspace_state`])
//! - `<home>/.data`       default working directory = `topic_path` ([`default_topic_dir`])
//!
//! A pinned topic's `topic_path` is wherever the pin points and stays there:
//! the pin *is* the working directory, and the state dir is registered
//! separately under the agents dir. So a topic's working directory never
//! contains its state, and every framework-owned directory is dot-prefixed
//! (`.jyc`, `.data`, `.attachments`, `.inbound`) — a name a user can own.

use std::path::{Path, PathBuf};

/// Resolve a channel's legacy workspace directory: `<data dir>/<channel>/workspace/`.
///
/// Not a topic root — topics live under the agents dir and never under a
/// channel. Kept for the non-pipe websocket channel shape and the GitHub
/// PR-topic scan; both are removal candidates in the channel-workspace
/// migration.
pub fn resolve_workspace(data_dir: &Path, channel: &str) -> PathBuf {
    data_dir.join(channel).join("workspace")
}

/// Name of the hub-level directory holding the bytes of an inbound attachment
/// between their upload and the message that announces them.
///
/// It lives directly under `data_dir()` — see [`inbound_dir`] —
/// deliberately outside every topic root and every channel workspace: staged
/// bytes must not be reachable through the topic files endpoint, and the topic
/// a message belongs to is only known once it has been routed.
pub const INBOUND_DIR_NAME: &str = ".inbound";

/// Name of a topic's default working directory, inside its home: `<home>/.data`.
///
/// Dot-prefixed like every other framework-owned directory (`.jyc` state,
/// `.inbound` staging, `.attachments` inbound files), which keeps it out of
/// the user's namespace: a topic name may not start with `.` (`invalid_name`
/// in `spawn_common`), so `<home>/.data` can never be a sibling topic's home.
pub const TOPIC_DIR_NAME: &str = ".data";

/// Resolve the agents dir: the root holding every agent's subtree.
///
/// Each agent's topics live one level deeper, at
/// `<agents dir>/<agent>/<topic>/` — the 1:1 dashboard topic and every
/// pipe-routed dynamic topic (`plan-42`, `review-7`, …) get their own
/// directory. Each such home keeps that topic's state dir (`<home>/.jyc`)
/// and its working directory (`<home>/.data`, [`default_topic_dir`]). An
/// agent that configures `topic_path` pins only its 1:1 topic there.
///
/// Falls back to `<data dir>/agents` when `data_dir()` is unavailable —
/// using the data dir (the instance root) instead of cwd so multi-instance
/// setups don't accidentally share state.
pub fn resolve_agents_dir(data_dir: &Path) -> PathBuf {
    if let Some(home) = jyc_utils::paths::data_dir() {
        home.join("agents")
    } else {
        tracing::warn!("data_dir() returned None; falling back to <data dir>/agents");
        data_dir.join("agents")
    }
}

/// Directory where the hub stages the bytes of inbound attachments:
/// `<data dir>/.inbound/`.
///
/// A sibling of `agents/` and `channels/`, so no topic directory can ever be
/// this path — which is the point: the inbound endpoint used to stage in the
/// uploading channel's workspace, and for the synthesized "agents" channel that
/// is `<data dir>/agents/workspace/`, i.e. exactly where a topic *named*
/// `workspace` lives. Staged bytes were then reachable through that topic's
/// files endpoint.
///
/// It has to be on the data volume rather than in the OS temp dir: the hub and
/// the agent workers can run in different containers sharing only this volume,
/// and the worker moves staged files out with a same-filesystem rename.
///
/// Falls back to a temp dir when `data_dir()` is unavailable (never to the
/// cwd, which multi-instance setups would share). Callers that need an isolated
/// root — tests — pass their own directory to the staging helpers instead.
pub fn inbound_dir() -> PathBuf {
    match jyc_utils::paths::data_dir() {
        Some(home) => home.join(INBOUND_DIR_NAME),
        None => {
            tracing::warn!("data_dir() returned None; staging inbound attachments in the temp dir");
            std::env::temp_dir().join("jyc-inbound")
        }
    }
}

/// Root directory holding adopted topic state dirs: `<agents dir>`.
///
/// Deliberately data-dir-based (unlike [`resolve_agents_dir`], which
/// prefers the platform data home for topic *dirs*): adopted state belongs
/// to the instance that pinned it, so multi-instance setups and tests with
/// an explicit `--data-dir` stay isolated. With the default data dir
/// (== `data_dir()`) this coincides with `~/.local/share/jyc/agents/`.
pub fn agents_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("agents")
}

/// Resolve a custom topic path from a pattern's `topic_path` config.
///
/// - `~` is expanded to `$HOME`
/// - Absolute paths are used as-is
/// - Relative paths are resolved against `base` — the caller's anchor dir: the
///   data root for a config pin, a topic's own dir for `/spawn`
pub fn resolve_topic_path(path: &str, base: &Path) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(rest)
        } else {
            PathBuf::from(path)
        }
    } else if path == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path))
    } else {
        let p = PathBuf::from(path);
        if p.is_absolute() { p } else { base.join(p) }
    }
}

/// The default working directory of a topic whose home is `home`.
///
/// This is the `topic_path` a fresh topic runs in — the directory its agent
/// works in, and the one a user opens to find its files. A topic keeps the
/// directory it was created with (see [`topic_work_dir`]), so this is the
/// *default*, not a rule applied to existing topics.
pub fn default_topic_dir(home: &Path) -> PathBuf {
    home.join(TOPIC_DIR_NAME)
}

/// The home a topic's working directory belongs to: `<home>/.data` → `home`.
///
/// Every other directory is its own home — a pinned `topic_path` (a user
/// dir), and both pre-`.data` layouts, where the working directory *was* the
/// home. So the state dir of a topic is `topic_home(dir)/.jyc`, never
/// `dir/.jyc`.
pub fn topic_home(topic_dir: &Path) -> &Path {
    match topic_dir.file_name() {
        Some(name) if name == TOPIC_DIR_NAME => topic_dir.parent().unwrap_or(topic_dir),
        _ => topic_dir,
    }
}

/// The working directory of a topic whose home is `home`.
///
/// The choice is made once and then read back, never guessed twice: a topic
/// records the directory it works in as `topic-path` inside its state dir
/// (the same breadcrumb a pinned topic has always carried), and that record
/// wins here. Only a topic with no record falls back to what the filesystem
/// says:
///
/// - `<home>/.data` — the new layout, chosen but not yet recorded;
/// - `<home>/.jyc` — the home itself was the working directory;
/// - `<home>/<topic_name>/.jyc` — a self-named agents topic nested one level
///   deeper (`<agents dir>/<agent>/<agent>`), as it used to be.
///
/// A topic with none of those is fresh and gets [`default_topic_dir`]. Note
/// that `<home>/.jyc` alone no longer means the old layout: it is where the
/// *new* layout keeps the state dir, which is why `.data` is probed first and
/// why the record is what an existing topic is really read from.
pub fn topic_work_dir_at(home: &Path, topic_name: &str) -> PathBuf {
    let recorded = std::fs::read_to_string(home.join(".jyc").join("topic-path"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    if let Some(dir) = recorded {
        return dir;
    }
    let default = default_topic_dir(home);
    if default.is_dir() {
        default
    } else if home.join(".jyc").is_dir() {
        home.to_path_buf()
    } else if home.join(topic_name).join(".jyc").is_dir() {
        home.join(topic_name)
    } else {
        default
    }
}

/// The working directory of `agent`'s topic named `topic_name`, under the
/// agents dir.
///
/// The topic's **home** is the agent's own root for the self-named topic
/// (agent `jyc` ↔ topic `jyc`) and one directory deeper for a pipe-routed one:
/// `<agents dir>/<agent>/<topic>`. What directory inside that home the topic
/// works in is [`topic_work_dir_at`]'s answer.
pub fn topic_work_dir(agents_dir: &Path, agent: &str, topic_name: &str) -> PathBuf {
    let root = agents_dir.join(agent);
    let home = if topic_name == agent {
        root
    } else {
        root.join(topic_name)
    };
    topic_work_dir_at(&home, topic_name)
}

/// Record the directory a topic works in, so its choice survives a restart.
///
/// Written once, next to the topic's state: every later resolution reads it
/// back instead of inferring the layout from the filesystem. Refuses to
/// overwrite an existing record — the choice is made once (`/close --force
/// --purge` and re-spawn being the deliberate way to remake a topic).
pub fn record_topic_work_dir(state_dir: &Path, topic_dir: &Path) -> std::io::Result<()> {
    let file = state_dir.join("topic-path");
    if file.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(file, format!("{}\n", topic_dir.display()))
}

/// Resolve the canonical state dir for a pinned/ad-hoc topic dir.
///
/// - Agent-keyed topic (config `[agents.<key>]` or the 1:1 "agents" channel):
///   `<agents dir>/<key>/.jyc` — config keys are unique by construction.
/// - Ad-hoc topic (any other pinned dir): `<agents dir>/<derived>/.jyc` with
///   the path-derived `_`-prefixed name from
///   [`jyc_types::state_dir::derive_state_name`] (the prefix keeps derived
///   names out of the config-key namespace).
pub fn state_dir_for(agents_dir: &Path, topic_dir: &Path, agent_key: Option<&str>) -> PathBuf {
    let name = match agent_key {
        Some(key) => key.to_string(),
        None => jyc_types::state_dir::derive_state_name(topic_dir),
    };
    agents_dir.join(name).join(".jyc")
}

/// Register the state dir of a jyc-owned topic whose working directory is
/// `topic_dir`, when that directory lives under `workspace`.
///
/// The state dir hangs off the topic's *home*, not off its working
/// directory: `<home>/.jyc`, with [`topic_home`] recovering `home` from a
/// `<home>/.data` work dir. No-op when the topic already has a registration
/// (an adopted pin, or an earlier activation) — a state dir is chosen once
/// and never moved (#825). A dir outside the workspace without a
/// registration is a lost adoption: left unregistered so `state_dir` fails
/// loudly instead of silently writing state into a user-owned dir (#825).
///
/// Known corner: stripping the `.data` leaf is lexical, so two topics whose
/// dirs differ only by that leaf (`<home>` and `<home>/.data`, which needs a
/// `-d <home>/.data` spawn inside another topic's home) would claim one
/// `<home>/.jyc`. The registry is keyed by topic name and has no reverse
/// lookup to catch it, so a close of one would take the other's state. Left
/// as-is deliberately: the trigger is contrived, and a guard would mean a new
/// jyc-types API (`registered_state` has no "who else holds this dir").
pub fn activate_workspace_state(topic_name: &str, topic_dir: &Path, workspace: &Path) {
    if topic_dir.starts_with(workspace) {
        let state_dir = topic_home(topic_dir).join(".jyc");
        jyc_types::state_dir::register_if_absent(topic_name, &state_dir);
        // Record which directory this topic works in, so a restart reads the
        // choice back instead of inferring it from the filesystem (see
        // `topic_work_dir_at`).
        if let Err(e) = record_topic_work_dir(&state_dir, topic_dir) {
            tracing::debug!(
                topic = %topic_name,
                error = %e,
                "Could not record the topic's working directory"
            );
        }
    }
}

/// Adopt `state_dir` as the `.jyc` location of topic `topic_name`, whose
/// topic dir is `topic_dir`.
///
/// First adoption wins: a topic's state dir is chosen once and never moved.
/// When the topic already has a registration (a runtime pin adopted before
/// the config restore ran, or the reverse), the existing state dir is kept
/// and `state_dir` is ignored — the old re-pin carry moved state between
/// dirs and could leave it split across two (#825).
///
/// On adoption, registers the name→state mapping, creates `state_dir`, and
/// writes the `topic-path` breadcrumb; `topic-name` is restamped only where
/// one already exists — restore discovery keys on that file to tell a used
/// topic from a fresh empty pin, so it is never created here.
/// [`restore_state_registry`] rebuilds the name-keyed mapping after a
/// restart even in processes without config access (subprocesses), where
/// several agents may share one `topic_dir`: registrations depend on the
/// name, never on the dir.
pub fn adopt_state_dir(
    topic_name: &str,
    topic_dir: &Path,
    state_dir: &Path,
) -> std::io::Result<()> {
    let legacy = topic_dir.join(".jyc");
    if legacy.exists() && state_dir != legacy {
        // In-dir state is no longer migrated (all deployments moved long ago),
        // so an in-dir `.jyc` here is either a pre-adoption leftover that this
        // topic's adopted state displaced, or *another* topic's live state — a
        // co-pinned dir, which is what a shared spawn creates. Both deserve a log;
        // only the first deserves a "delete it manually", which previously sent
        // people hunting through the topic that owns the directory.
        tracing::warn!(
            path = %legacy.display(),
            topic = %topic_name,
            "Legacy in-topic .jyc found; it is not migrated and not this topic's \
             state — it belongs to whoever pinned this dir first"
        );
    }
    if jyc_types::state_dir::registered_state(topic_name).is_some() {
        return Ok(());
    }
    jyc_types::state_dir::register(topic_name, state_dir);
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(
        state_dir.join("topic-path"),
        topic_dir.to_string_lossy().as_bytes(),
    )?;
    // Restamp the identity breadcrumb only where one already exists (a
    // previously initialized topic). Never CREATE it: restore discovery
    // keys on this file to tell "used topic" from a fresh empty pin, and
    // `set_topic_path` writes it on first real open.
    let name_file = state_dir.join("topic-name");
    if name_file.exists() {
        std::fs::write(&name_file, topic_name.as_bytes())?;
    }
    Ok(())
}

/// Canonicalize when the path exists, else use it as given.
///
/// The guards that compare topic dirs run on paths that may not exist yet (a
/// state root nothing has adopted into, a destination about to be created), and
/// a literal fallback is the right answer there.
pub(crate) fn resolved(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Re-register state dirs of previously adopted topics at startup.
///
/// Scans every `<agents dir>/*/.jyc` carrying a `topic-path` breadcrumb
/// and registers **topic name → state dir** from its `topic-name`
/// breadcrumb (falling back to the folder name for config-key state dirs
/// written before the breadcrumb existed). Pure `state_dir` bootstrap for
/// processes without config access (e.g. per-topic hook commands);
/// several agents may share one topic dir, so the mapping is keyed by
/// name, never by dir. Derived (`_`-prefixed) state
/// dirs with no readable `topic-name` are skipped — they are
/// deterministically re-adopted under their topic name whenever the
/// dashboard or `/pin` re-opens the dir. Config-pin processes additionally
/// adopt from config; re-registering an identical mapping is idempotent.
/// Returns the number of registrations restored.
pub fn restore_state_registry(agents_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return 0;
    };
    let mut restored = 0;
    for entry in entries.flatten() {
        let state = entry.path().join(".jyc");
        let Ok(topic_path) = std::fs::read_to_string(state.join("topic-path")) else {
            continue;
        };
        let topic_path = PathBuf::from(topic_path.trim());
        let name = std::fs::read_to_string(state.join("topic-name"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let name = if name.is_empty() {
            let folder = entry.file_name().to_string_lossy().into_owned();
            if folder.starts_with('_') {
                continue; // derived, no name: re-adopted on topic open
            }
            folder // config-key dir: folder name is the topic name
        } else {
            name
        };
        if !topic_path.is_dir() {
            tracing::warn!(
                state_dir = %state.display(),
                topic_dir = %topic_path.display(),
                "Adopted topic dir no longer exists; state kept for history"
            );
        }
        jyc_types::state_dir::register(&name, &state);
        restored += 1;
    }
    if restored > 0 {
        tracing::info!(count = restored, "Restored topic state-dir registry");
    }
    restored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_storage::MessageStorage;
    use jyc_types::{ChannelPattern, InboundMessage, MessageContent};
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn make_message(channel: &str, topic: &str) -> InboundMessage {
        InboundMessage {
            id: "1".to_string(),
            channel: channel.to_string(),
            channel_uid: "1".to_string(),
            sender: "user".to_string(),
            sender_address: "user@test".to_string(),
            recipients: vec![],
            topic: topic.to_string(),
            content: MessageContent::default(),
            timestamp: chrono::Utc::now(),
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        }
    }

    fn make_feishu_message(chat_name: &str, chat_type: &str) -> InboundMessage {
        let mut msg = make_message("feishu_bot", "");
        msg.metadata
            .insert("chat_name".to_string(), serde_json::json!(chat_name));
        msg.metadata
            .insert("chat_type".to_string(), serde_json::json!(chat_type));
        msg
    }

    // === state dir adoption ===

    #[test]
    fn state_dir_for_agent_key_vs_derived() {
        let root = Path::new("/data/agents");
        assert_eq!(
            state_dir_for(root, Path::new("/home/u/proj"), Some("jyc")),
            PathBuf::from("/data/agents/jyc/.jyc")
        );
        assert_eq!(
            state_dir_for(root, Path::new("/home/u/proj"), None),
            PathBuf::from("/data/agents/_home_u_proj/.jyc")
        );
    }

    #[test]
    fn adopt_state_dir_registers_and_breadcrumbs() {
        let tmp = tempdir().unwrap();
        let topic = tmp.path().join("adopt-topic");
        std::fs::create_dir_all(&topic).unwrap();
        let state = tmp.path().join("agents").join("_adopt");

        adopt_state_dir("adopt-name", &topic, &state).unwrap();
        assert_eq!(
            std::fs::read_to_string(state.join("topic-path")).unwrap(),
            topic.to_string_lossy()
        );
        assert!(
            !state.join("topic-name").exists(),
            "topic-name is never created by adopt"
        );
        assert_eq!(
            jyc_types::state_dir::resolve_state_dir("adopt-name", &topic),
            state
        );

        // second adopt is a no-op (already registered)
        adopt_state_dir("adopt-name", &topic, &state).unwrap();
        assert_eq!(
            jyc_types::state_dir::resolve_state_dir("adopt-name", &topic),
            state
        );
    }

    #[test]
    fn co_pinned_dirs_keep_isolated_state() {
        // Two topics pinning ONE dir: each name keeps its own state dir.
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("copin");
        std::fs::create_dir_all(&dir).unwrap();
        let root = tmp.path().join("agents");
        let state_a = state_dir_for(&root, &dir, Some("alpha"));
        let state_b = state_dir_for(&root, &dir, Some("beta"));

        adopt_state_dir("alpha", &dir, &state_a).unwrap();
        adopt_state_dir("beta", &dir, &state_b).unwrap();

        assert_ne!(state_a, state_b, "each topic gets its own state dir");
        let jyc = |n: &str| jyc_types::state_dir::resolve_state_dir(n, &dir);
        assert_eq!(jyc("alpha"), state_a);
        assert_eq!(jyc("beta"), state_b);

        // Removing one topic's registration must not disturb the other's.
        jyc_types::state_dir::unregister("alpha");
        assert!(jyc_types::state_dir::registered_state("alpha").is_none());
        assert_eq!(jyc("beta"), state_b);

        // The breadcrumb scan recovers BOTH via the folder-name fallback:
        // never-initialized dirs carry no topic-name file by design.
        jyc_types::state_dir::unregister("alpha");
        jyc_types::state_dir::unregister("beta");
        assert!(super::restore_state_registry(&root) >= 2);
        assert_eq!(
            jyc_types::state_dir::registered_state("alpha"),
            Some(jyc_types::state_dir::resolve_state_dir("alpha", &dir))
        );
        assert_eq!(
            jyc_types::state_dir::registered_state("beta"),
            Some(state_b.clone())
        );
    }

    #[test]
    fn restore_state_registry_reregisters_from_breadcrumbs() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("agents");
        let topic = tmp.path().join("scanned-topic");
        std::fs::create_dir_all(&topic).unwrap();
        let state = root.join("_scanned").join(".jyc");
        adopt_state_dir("scanned-name", &topic, &state).unwrap();
        // An initialized topic carries the name breadcrumb (written by
        // `set_topic_path`); seed it to exercise the scan's primary path.
        std::fs::write(state.join("topic-name"), "scanned-name").unwrap();

        // Prove the scan (not the adopt) rebuilds the name-keyed mapping.
        jyc_types::state_dir::unregister("scanned-name");
        assert!(restore_state_registry(&root) >= 1);
        assert_eq!(
            jyc_types::state_dir::resolve_state_dir("scanned-name", &topic),
            state
        );

        // Config-key state dirs whose topic-name breadcrumb is missing
        // fall back to the folder name (pre-breadcrumb layout).
        let key_state = root.join("agentkey").join(".jyc");
        std::fs::create_dir_all(&key_state).unwrap();
        std::fs::write(
            key_state.join("topic-path"),
            topic.to_string_lossy().as_bytes(),
        )
        .unwrap();
        assert!(restore_state_registry(&root) >= 2);
        assert_eq!(
            jyc_types::state_dir::registered_state("agentkey"),
            Some(key_state.clone())
        );

        // A derived dir with no topic-name is skipped (no config-key to
        // fall back to; it re-adopts on reopen).
        let lone = root.join("_lone").join(".jyc");
        std::fs::create_dir_all(&lone).unwrap();
        std::fs::write(lone.join("topic-path"), "/nonexistent/dir").unwrap();
        let before = restore_state_registry(&root);
        assert_eq!(before, restore_state_registry(&root), "_lone not counted");

        // missing agents root -> 0, no panic
        assert_eq!(restore_state_registry(&tmp.path().join("nowhere")), 0);
    }

    #[test]
    fn adopt_repin_keeps_first_adoption() {
        // A topic's state dir is chosen once and never moved: a second
        // adopt under a different candidate dir is a no-op (#825).
        let tmp = tempdir().unwrap();
        let topic = tmp.path().join("repin-topic");
        std::fs::create_dir_all(&topic).unwrap();
        let first = tmp.path().join("agents").join("_first").join(".jyc");
        adopt_state_dir("repin-name", &topic, &first).unwrap();
        std::fs::write(first.join("marker"), b"payload").unwrap();

        let second = tmp.path().join("agents").join("key").join(".jyc");
        adopt_state_dir("repin-name", &topic, &second).unwrap();
        assert!(first.join("marker").exists(), "first state dir untouched");
        assert!(
            !second.exists(),
            "candidate dir not provisioned when a registration exists"
        );
        assert_eq!(
            jyc_types::state_dir::resolve_state_dir("repin-name", &topic),
            first
        );

        // A DIFFERENT name pinning the same dir gets its own clean state.
        let other = tmp.path().join("agents").join("other").join(".jyc");
        adopt_state_dir("other-name", &topic, &other).unwrap();
        assert!(first.join("marker").exists(), "repin state untouched");
        assert!(!other.join("marker").exists(), "clean state for other");
    }

    #[test]
    fn adopt_ignores_legacy_in_dir_state() {
        // A pre-adoption .jyc still sitting in the topic dir is left
        // alone (warned about), never moved or merged.
        let tmp = tempdir().unwrap();
        let topic = tmp.path().join("conflict-topic");
        std::fs::create_dir_all(topic.join(".jyc")).unwrap();
        let state = tmp.path().join("conflict-state");
        adopt_state_dir("conflict-name", &topic, &state).unwrap();
        assert!(topic.join(".jyc").exists(), "legacy dir left in place");
        assert!(
            !state.join("topic-name").exists(),
            "nothing carried out of the legacy dir"
        );
    }

    // === resolve_workspace (used by cli/serve.rs) ===

    #[test]
    fn test_resolve_topic_path_absolute() {
        let p = resolve_topic_path("/home/jiny/my-project", Path::new("/data"));
        assert_eq!(p, PathBuf::from("/home/jiny/my-project"));
    }

    #[test]
    fn test_resolve_topic_path_tilde() {
        let p = resolve_topic_path("~/my-project", Path::new("/data"));
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(p, PathBuf::from(home).join("my-project"));
        } else {
            // No HOME set — falls back to literal
            assert_eq!(p, PathBuf::from("~/my-project"));
        }
    }

    #[test]
    fn test_resolve_workspace_email() {
        let ws = resolve_workspace(Path::new("/data"), "jiny283a");
        assert_eq!(ws, PathBuf::from("/data/jiny283a/workspace"));
    }

    #[test]
    fn test_resolve_workspace_feishu() {
        let ws = resolve_workspace(Path::new("/data"), "feishu_bot");
        assert_eq!(ws, PathBuf::from("/data/feishu_bot/workspace"));
    }

    // === MessageStorage.store_with_match (real production path) ===

    #[tokio::test]
    async fn test_storage_topic_path_from_email_subject() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);
        let msg = make_message("jiny283a", "Test Subject");

        // The email adapter strips `Re:`/`Fw:` before handing the topic over;
        // storage must accept the name as given.
        let topic_name = "Test Subject".to_string();

        let result = storage
            .store_with_match(&msg, &topic_name, true, None)
            .await
            .unwrap();

        // Verify: <data dir>/jiny283a/workspace/Test Subject/
        assert_eq!(result.topic_path, ws.join("Test Subject"));
        assert!(result.topic_path.exists());
        // No double nesting
        assert!(
            !result
                .topic_path
                .to_string_lossy()
                .contains("workspace/jiny283a")
        );
    }

    #[tokio::test]
    async fn test_storage_topic_path_from_chinese_subject() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);
        let topic_name = "您收到来自上海栋菁餐饮管理有限公司的电子发票".to_string();
        let msg = make_message("jiny283a", &topic_name);

        let result = storage
            .store_with_match(&msg, &topic_name, true, None)
            .await
            .unwrap();
        assert!(result.topic_path.exists());
        assert!(result.topic_path.to_string_lossy().contains("上海栋菁餐饮"));
    }

    #[tokio::test]
    async fn test_storage_topic_path_from_config_override() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);

        // Pattern has topic_name override
        let pattern = ChannelPattern {
            name: "invoices".to_string(),
            topic_name: Some("invoice-processing".to_string()),
            ..Default::default()
        };

        // Different subjects all go to same topic
        for subject in &["Invoice food", "发票 office", "Receipt hotel"] {
            let topic_name = pattern.topic_name.as_deref().unwrap_or(*subject);
            assert_eq!(topic_name, "invoice-processing");
        }

        let msg = make_message("jiny283a", "Invoice food");
        let result = storage
            .store_with_match(&msg, "invoice-processing", true, None)
            .await
            .unwrap();

        assert_eq!(result.topic_path, ws.join("invoice-processing"));
        assert!(result.topic_path.exists());
    }

    #[tokio::test]
    async fn test_storage_topic_path_from_feishu_with_config_override() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "feishu_bot");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);

        let pattern = ChannelPattern {
            name: "invoices".to_string(),
            topic_name: Some("invoice-processing-feishu".to_string()),
            ..Default::default()
        };

        // Feishu chat_name would be "发票群" but config overrides
        let topic_name = pattern.topic_name.as_deref().unwrap_or("发票群");
        assert_eq!(topic_name, "invoice-processing-feishu");

        let msg = make_feishu_message("发票群", "group");
        let result = storage
            .store_with_match(&msg, topic_name, true, None)
            .await
            .unwrap();
        assert_eq!(result.topic_path, ws.join("invoice-processing-feishu"));
    }

    // === Attachment path (real production path) ===

    #[tokio::test]
    async fn test_attachment_saves_to_correct_topic_dir() {
        use jyc_types::MessageAttachment;

        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        let topic_path = ws.join("invoice-processing");
        tokio::fs::create_dir_all(&topic_path).await.unwrap();

        let mut msg = make_message("jiny283a", "Invoice");
        msg.attachments.push(MessageAttachment {
            filename: "test.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 5,
            content: Some(b"hello".to_vec()),
            saved_path: None,
            staged_path: None,
        });

        crate::attachment_storage::save_attachments_to_dir(&mut msg, &topic_path, None)
            .await
            .unwrap();

        // Verify attachment saved under topic_path/.attachments/
        let att_dir = topic_path.join(crate::attachment_storage::ATTACHMENT_DIR_NAME);
        assert!(att_dir.exists());

        // No double nesting
        let att_path_str = att_dir.to_string_lossy();
        assert_eq!(att_path_str.matches("workspace").count(), 1);
        assert!(!att_path_str.contains("jiny283a/workspace/jiny283a"));

        // File exists
        assert!(msg.attachments[0].saved_path.is_some());
        assert!(msg.attachments[0].saved_path.as_ref().unwrap().exists());
    }

    // === store_at_path (custom topic_path override) ===

    #[tokio::test]
    async fn test_store_at_path_writes_to_registered_state_dir() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);

        // Custom topic path OUTSIDE the workspace — a pinned topic. State
        // was adopted at pin time (#825: never in-dir for user-owned dirs).
        let custom_path = tmp.path().join("custom-projects").join("my-project");
        tokio::fs::create_dir_all(&custom_path).await.unwrap();
        let state = tmp
            .path()
            .join("agents")
            .join("at-path-writes")
            .join(".jyc");
        jyc_types::state_dir::register("at-path-writes", &state);

        let msg = make_message("jiny283a", "Test Subject");
        let result = storage
            .store_at_path(&msg, "at-path-writes", &custom_path, true)
            .await
            .unwrap();

        // Topic path should be the custom path, not workspace-joined
        assert_eq!(result.topic_path, custom_path);
        assert!(result.topic_path.exists());

        // Chat log goes to the registered state dir, not the topic dir
        let entries: Vec<_> = std::fs::read_dir(&state).unwrap().collect();
        let has_chat_log = entries.iter().any(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("chat_history_")
        });
        assert!(has_chat_log, "chat log file should exist in the state dir");
        assert!(
            !custom_path.join(".jyc").exists(),
            "no state written into the user-owned topic dir"
        );

        // Should NOT be under workspace
        assert!(
            !result.topic_path.starts_with(&ws),
            "custom topic path should not be under workspace"
        );
    }

    #[tokio::test]
    #[should_panic(expected = "no registered state dir")]
    async fn test_store_at_path_unregistered_outside_workspace_panics() {
        // A user-owned dir without a registration is a lost adoption: state
        // access must fail loudly, never write into the topic dir (#825).
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);
        let custom_path = tmp.path().join("unregistered-project");
        tokio::fs::create_dir_all(&custom_path).await.unwrap();

        let msg = make_message("jiny283a", "Test Subject");
        storage
            .store_at_path(&msg, "at-path-unregistered", &custom_path, true)
            .await
            .unwrap();
    }

    // === resolve_topic_path edge cases ===

    #[test]
    fn test_resolve_topic_path_home_only() {
        let p = resolve_topic_path("~", Path::new("/data"));
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(p, PathBuf::from(home));
        } else {
            assert_eq!(p, PathBuf::from("~"));
        }
    }

    #[test]
    fn test_resolve_topic_path_relative() {
        // Relative paths are resolved against the data root (data_dir)
        let p = resolve_topic_path("my-project", Path::new("/data"));
        assert_eq!(p, PathBuf::from("/data/my-project"));
    }

    #[tokio::test]
    async fn test_topic_path_override_not_under_workspace() {
        // Verify that store_at_path produces a path completely outside
        // the standard workspace hierarchy.
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "feishu_bot");

        let custom = tmp.path().join("elsewhere");
        tokio::fs::create_dir_all(&custom).await.unwrap();

        let storage = MessageStorage::new(&ws);
        let state = tmp
            .path()
            .join("agents")
            .join("at-path-elsewhere")
            .join(".jyc");
        jyc_types::state_dir::register("at-path-elsewhere", &state);
        let msg = make_feishu_message("发票群", "group");
        let result = storage
            .store_at_path(&msg, "at-path-elsewhere", &custom, true)
            .await
            .unwrap();

        assert_eq!(result.topic_path, custom);
        // Ensure path doesn't contain "workspace" segment at all
        assert!(
            !result.topic_path.to_string_lossy().contains("workspace"),
            "custom path should not contain 'workspace'"
        );
    }

    /// Agents workspace root is `<data dir>/agents`, distinct from a
    /// regular channel's `<data dir>/<channel>/workspace/`.
    #[test]
    fn test_resolve_agents_dir_is_not_a_channel_workspace() {
        let data_dir = Path::new("/tmp/jyc-data");
        let agents_dir = resolve_agents_dir(data_dir);
        let chan_ws = resolve_workspace(data_dir, "work");
        assert!(chan_ws.starts_with(data_dir));
        assert!(
            !agents_dir.starts_with(data_dir) || agents_dir == data_dir.join("agents"),
            "agents root should resolve via data_dir, not under data_dir"
        );
    }

    /// The staging root must not be reachable as a topic path — for the
    /// "agents" channel a topic named `workspace` *is* its channel workspace,
    /// which is what staging under a channel workspace used to collide with.
    #[test]
    fn test_inbound_dir_is_outside_every_topic_root() {
        let staging = inbound_dir();
        assert_eq!(staging.file_name().unwrap(), INBOUND_DIR_NAME);

        let data_dir = Path::new("/tmp/jyc-data");
        let agents_dir = resolve_agents_dir(data_dir);
        assert!(
            !staging.starts_with(&agents_dir),
            "staging {staging:?} must stay outside the agents topic root {agents_dir:?}"
        );
        if jyc_utils::paths::data_dir().is_some() {
            assert_eq!(
                staging.parent(),
                agents_dir.parent(),
                "staging is a sibling of the agents root, not a topic path"
            );
        }
    }

    // === topic layout: home / state dir / working directory ===

    #[test]
    fn topic_home_is_the_inverse_of_the_default_dir() {
        let home = Path::new("/data/agents/a/t1");
        assert_eq!(
            default_topic_dir(home),
            PathBuf::from("/data/agents/a/t1/.data")
        );
        assert_eq!(topic_home(&default_topic_dir(home)), home);

        // Everything that is not `<home>/.data` is its own home: a pinned
        // user dir, and both pre-`.data` topic layouts.
        assert_eq!(
            topic_home(Path::new("/home/u/proj")),
            Path::new("/home/u/proj")
        );
        assert_eq!(topic_home(home), home);
    }

    #[test]
    fn a_fresh_topic_works_in_dot_data() {
        let tmp = tempdir().unwrap();
        let agents = tmp.path().join("agents");
        assert_eq!(
            topic_work_dir(&agents, "a", "t1"),
            agents.join("a").join("t1").join(TOPIC_DIR_NAME),
            "a topic with no directory yet gets the new default"
        );
        // The self-named topic is the agent's own root, not one level deeper.
        assert_eq!(
            topic_work_dir(&agents, "a", "a"),
            agents.join("a").join(TOPIC_DIR_NAME)
        );
    }

    #[test]
    fn an_existing_topic_keeps_the_directory_it_was_created_with() {
        let tmp = tempdir().unwrap();
        let agents = tmp.path().join("agents");

        // Pre-`.data` pipe topic: the home itself was the working directory.
        let pipe = agents.join("a").join("weather");
        std::fs::create_dir_all(pipe.join(".jyc")).unwrap();
        assert_eq!(topic_work_dir(&agents, "a", "weather"), pipe);
        assert!(
            !pipe.join(TOPIC_DIR_NAME).exists(),
            "an existing topic does not grow a .data"
        );

        // Pre-`.data` self-named agents topic: one level deeper.
        let agent = agents.join("b");
        std::fs::create_dir_all(agent.join("b").join(".jyc")).unwrap();
        assert_eq!(topic_work_dir(&agents, "b", "b"), agent.join("b"));
        // A fresh sibling topic of that same agent still gets the default.
        assert_eq!(
            topic_work_dir(&agents, "b", "plan-7"),
            agent.join("plan-7").join(TOPIC_DIR_NAME)
        );
    }

    #[test]
    fn activate_registers_state_at_the_home_not_in_the_work_dir() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("agents");
        let home = workspace.join("a").join("t1");
        let registered = jyc_types::state_dir::registered_state;

        // New layout: working dir is `<home>/.data`, state goes to `<home>/.jyc`.
        activate_workspace_state("layout-new", &default_topic_dir(&home), &workspace);
        assert_eq!(registered("layout-new"), Some(home.join(".jyc")));
        jyc_types::state_dir::unregister("layout-new");

        // Pre-`.data` layout: the dir *is* the home, state stays in-dir.
        activate_workspace_state("layout-old", &home, &workspace);
        assert_eq!(registered("layout-old"), Some(home.join(".jyc")));
        jyc_types::state_dir::unregister("layout-old");

        // A user dir outside the workspace is never registered (#825).
        let outside = tmp.path().join("user-repo");
        activate_workspace_state("layout-pin", &outside, &workspace);
        assert_eq!(registered("layout-pin"), None);
    }

    /// The new layout keeps the state dir at `<home>/.jyc` — the very thing
    /// that used to mean "the home is the working directory". A topic that
    /// works in `.data` must not move back to its home because its state is
    /// there, or every topic would shift on the next fresh resolution.
    #[test]
    fn state_beside_dot_data_does_not_move_the_topic_to_its_home() {
        let tmp = tempdir().unwrap();
        let agents = tmp.path().join("agents");
        let home = agents.join("a").join("plan-7");
        std::fs::create_dir_all(home.join(".jyc")).unwrap();
        std::fs::create_dir_all(home.join(TOPIC_DIR_NAME)).unwrap();
        assert_eq!(
            topic_work_dir(&agents, "a", "plan-7"),
            home.join(TOPIC_DIR_NAME),
            "the state dir is not a layout marker once `.data` is in use"
        );
    }

    /// A recorded working directory is the answer, whatever the shapes on disk
    /// would suggest — that is what keeps an existing topic where it was.
    #[test]
    fn a_recorded_working_dir_wins_over_the_layout() {
        let tmp = tempdir().unwrap();
        let agents = tmp.path().join("agents");
        let home = agents.join("a").join("weather");
        std::fs::create_dir_all(home.join(".jyc")).unwrap();
        std::fs::create_dir_all(home.join(TOPIC_DIR_NAME)).unwrap();
        record_topic_work_dir(&home.join(".jyc"), &home).unwrap();
        assert_eq!(
            topic_work_dir(&agents, "a", "weather"),
            home,
            "the record is read before the layout"
        );
        // Written once: a later call does not move the topic.
        record_topic_work_dir(&home.join(".jyc"), &home.join(TOPIC_DIR_NAME)).unwrap();
        assert_eq!(topic_work_dir(&agents, "a", "weather"), home);
    }

    /// The restart scenario end to end: activation records the working dir it
    /// was given, so resolving it again (a fresh process, no in-memory pin)
    /// answers the same directory even though the state dir now exists.
    #[test]
    fn activation_records_the_working_dir_it_was_given() {
        let tmp = tempdir().unwrap();
        let agents = tmp.path().join("agents");
        let home = agents.join("a").join("record-me");
        std::fs::create_dir_all(&home).unwrap();
        let work = default_topic_dir(&home);
        activate_workspace_state("record-me", &work, &agents);
        assert_eq!(
            topic_work_dir(&agents, "a", "record-me"),
            work,
            "recorded, so a restart resolves the same directory"
        );
    }
}
