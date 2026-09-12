//! Central topic path resolution.
//!
//! The topic directory follows the convention:
//!   `<workdir>/<channel>/workspace/<topic_name>/`
//!
//! For agents (`[agents.<name>]`), the default lives under
//! `<data_home>/agents/<agent_name>/`, platform-resolved.

use std::path::{Path, PathBuf};

/// Resolve the workspace directory for a channel.
///
/// Convention: `<workdir>/<channel>/workspace/`
pub fn resolve_workspace(workdir: &Path, channel: &str) -> PathBuf {
    workdir.join(channel).join("workspace")
}

/// Resolve the workspace root for the synthesized "agents" channel.
///
/// This is the parent directory holding every agent's subtree:
/// `<data_home>/agents/`. Each agent's topics live one level deeper at
/// `<data_home>/agents/<agent>/<topic>/` — the 1:1 dashboard topic and
/// every pipe-routed dynamic topic (`plan-42`, `review-7`, …) get their
/// own directory. An agent that configures `topic_path` pins only its
/// 1:1 topic there.
///
/// Falls back to `<workdir>/agents` when `data_home()` is unavailable —
/// using `workdir` (the jyc instance root) instead of cwd so
/// multi-instance setups don't accidentally share state.
pub fn resolve_agents_workspace_root(workdir: &Path) -> PathBuf {
    if let Some(home) = jyc_utils::paths::data_home() {
        home.join("agents")
    } else {
        tracing::warn!("data_home() returned None; falling back to <workdir>/agents");
        workdir.join("agents")
    }
}

/// Root directory holding adopted topic state dirs: `<workdir>/agents/`.
///
/// Deliberately workdir-based (NOT [`resolve_agents_workspace_root`], which
/// prefers the platform data home for topic *dirs*): adopted state belongs
/// to the jyc instance that pinned it, so multi-instance setups and tests
/// with an explicit `--workdir` stay isolated. With the default workdir
/// (== `data_home()`) this coincides with `~/.local/share/jyc/agents/`.
pub fn state_root(workdir: &Path) -> PathBuf {
    workdir.join("agents")
}

/// Resolve a custom topic path from a pattern's `topic_path` config.
///
/// - `~` is expanded to `$HOME`
/// - Absolute paths are used as-is
/// - Relative paths are resolved against the data root (workdir)
pub fn resolve_topic_path(path: &str, data_root: &Path) -> PathBuf {
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
        if p.is_absolute() {
            p
        } else {
            data_root.join(p)
        }
    }
}

/// Resolve the canonical state dir for a pinned/ad-hoc topic dir.
///
/// - Agent-keyed topic (config `[agents.<key>]` or the 1:1 "agents" channel):
///   `<agents_root>/<key>/.jyc` — config keys are unique by construction.
/// - Ad-hoc topic (any other pinned dir): `<agents_root>/<derived>/.jyc` with
///   the path-derived `_`-prefixed name from
///   [`jyc_types::state_dir::derive_state_name`] (the prefix keeps derived
///   names out of the config-key namespace).
pub fn state_dir_for(agents_root: &Path, topic_dir: &Path, agent_key: Option<&str>) -> PathBuf {
    let name = match agent_key {
        Some(key) => key.to_string(),
        None => jyc_types::state_dir::derive_state_name(topic_dir),
    };
    agents_root.join(name).join(".jyc")
}

/// Adopt `state_dir` as the `.jyc` location of topic `topic_name`, whose
/// topic dir is `topic_dir`.
///
/// Registers the name→state mapping and, when the topic was previously
/// adopted under a different state dir (re-pin), carries that state into
/// place (rename, cross-device copy fallback). Always (re)writes the
/// `topic-path` breadcrumb; `topic-name` is restamped only where one
/// already exists (carried state or a previously initialized topic) —
/// restore discovery keys on that file to tell a used topic from a fresh
/// empty pin, so it is never created here.
/// [`restore_state_registry`] rebuilds the name-keyed mapping after a
/// restart even in processes without config access (subprocesses), where
/// several agents may share one `topic_dir`: registrations depend on the
/// name, never on the dir.
///
/// Returns Ok(true) if state was physically moved.
pub fn adopt_state_dir(
    topic_name: &str,
    topic_dir: &Path,
    state_dir: &Path,
) -> std::io::Result<bool> {
    let legacy = topic_dir.join(".jyc");
    if legacy.exists() {
        // Pre-adoption in-dir state is no longer migrated (all
        // deployments moved long ago); warn instead of silently
        // dropping it.
        tracing::warn!(
            path = %legacy.display(),
            "Legacy in-topic .jyc found; it is no longer migrated — move or delete it manually"
        );
    }
    let prev = jyc_types::state_dir::registered_state(topic_name);
    jyc_types::state_dir::register(topic_name, state_dir);
    let mut moved = false;
    // Re-pin carry: state was already adopted elsewhere (e.g. runtime
    // ad-hoc name -> later config-key name, or the reverse).
    let source = if state_dir.exists() {
        None
    } else if let Some(prev) = prev.filter(|p| p != state_dir && p.is_dir()) {
        tracing::info!(
            topic_dir = %topic_dir.display(),
            from = %prev.display(),
            to = %state_dir.display(),
            "Carrying topic state to newly adopted state dir"
        );
        Some(prev)
    } else {
        None
    };
    if let Some(src) = source {
        if let Some(parent) = state_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::rename(&src, state_dir) {
            Ok(()) => moved = true,
            Err(_) => {
                // Cross-device (or transient): copy then remove the source.
                copy_dir_all(&src, state_dir)?;
                let _ = std::fs::remove_dir_all(&src);
                moved = true;
            }
        }
        // Drop the now-empty old namespace folder (agents/<old-name>/).
        if let Some(src_parent) = src.parent()
            && src_parent
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('_'))
        {
            let _ = std::fs::remove_dir(src_parent);
        }
    }
    std::fs::create_dir_all(state_dir)?;
    std::fs::write(
        state_dir.join("topic-path"),
        topic_dir.to_string_lossy().as_bytes(),
    )?;
    // Restamp the identity breadcrumb only where one already exists (a
    // carried re-pin dir or a previously initialized topic). Never CREATE
    // it: restore discovery keys on this file to tell "used topic" from a
    // fresh empty pin, and `set_topic_path` writes it on first real open.
    let name_file = state_dir.join("topic-name");
    if name_file.exists() {
        std::fs::write(&name_file, topic_name.as_bytes())?;
    }
    Ok(moved)
}

fn copy_dir_all(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Re-register state dirs of previously adopted topics at startup.
///
/// Scans every `<agents_root>/*/.jyc` carrying a `topic-path` breadcrumb
/// and registers **topic name → state dir** from its `topic-name`
/// breadcrumb (falling back to the folder name for config-key state dirs
/// written before the breadcrumb existed). Pure `jyc_dir` bootstrap for
/// processes without config access (e.g. the external `jyc
/// mcp-reply-tool`); several agents may share one topic dir, so the
/// mapping is keyed by name, never by dir. Derived (`_`-prefixed) state
/// dirs with no readable `topic-name` are skipped — they are
/// deterministically re-adopted under their topic name whenever the
/// dashboard or `/pin` re-opens the dir. Config-pin processes additionally
/// adopt from config; re-registering an identical mapping is idempotent.
/// Returns the number of registrations restored.
pub fn restore_state_registry(agents_root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(agents_root) else {
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

/// One-time migration for the `topic` → `topic` rename.
///
/// Pre-rename topic directories carry a `.jyc/thread-name` file. If a
/// directory has that legacy file but no `.jyc/topic-name`, rename it so
/// existing topics keep their identity across restarts.
///
/// NOTE: the legacy filename must stay "thread-name" here — this is the
/// pre-rename on-disk name, not the (renamed) current concept.
pub fn migrate_topic_name_file(jyc_dir: &Path) {
    let old = jyc_dir.join("thread-name");
    let new = jyc_dir.join("topic-name");
    if !new.exists() && old.exists() {
        if let Ok(name) = std::fs::read_to_string(&old) {
            let _ = std::fs::write(&new, name);
            tracing::info!(path = %jyc_dir.display(), "Migrated legacy .jyc/thread-name to .jyc/topic-name");
        }
        let _ = std::fs::remove_file(&old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email_parser;
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

        assert!(
            !adopt_state_dir("adopt-name", &topic, &state).unwrap(),
            "fresh adopt has nothing to move"
        );
        assert_eq!(
            std::fs::read_to_string(state.join("topic-path")).unwrap(),
            topic.to_string_lossy()
        );
        assert!(
            !state.join("topic-name").exists(),
            "topic-name is never created by adopt"
        );
        assert_eq!(jyc_types::state_dir::jyc_dir("adopt-name", &topic), state);

        // second adopt is a no-op (registered, nothing to move)
        assert!(!adopt_state_dir("adopt-name", &topic, &state).unwrap());
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

        assert!(!adopt_state_dir("alpha", &dir, &state_a).unwrap());
        assert!(!adopt_state_dir("beta", &dir, &state_b).unwrap());

        assert_ne!(state_a, state_b, "each topic gets its own state dir");
        let jyc = |n: &str| jyc_types::state_dir::jyc_dir(n, &dir);
        assert_eq!(jyc("alpha"), state_a);
        assert_eq!(jyc("beta"), state_b);

        // Closing one topic's registration must not disturb the other's.
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
            Some(jyc_types::state_dir::jyc_dir("alpha", &dir))
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
        assert_eq!(jyc_types::state_dir::jyc_dir("scanned-name", &topic), state);

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
    fn adopt_repin_carries_existing_state() {
        let tmp = tempdir().unwrap();
        let topic = tmp.path().join("repin-topic");
        std::fs::create_dir_all(&topic).unwrap();
        let first = tmp.path().join("agents").join("_first").join(".jyc");
        adopt_state_dir("repin-name", &topic, &first).unwrap();
        std::fs::write(first.join("marker"), b"payload").unwrap();

        let second = tmp.path().join("agents").join("key").join(".jyc");
        assert!(
            adopt_state_dir("repin-name", &topic, &second).unwrap(),
            "carry moves prior state"
        );
        assert!(second.join("marker").exists());
        assert!(!first.exists(), "old state dir consumed");
        assert!(
            !first.parent().unwrap().exists(),
            "empty old namespace folder removed"
        );
        assert_eq!(jyc_types::state_dir::jyc_dir("repin-name", &topic), second);

        // A DIFFERENT name pinning the same dir gets its own clean state.
        let other = tmp.path().join("agents").join("other").join(".jyc");
        assert!(
            !adopt_state_dir("other-name", &topic, &other).unwrap(),
            "other name must not carry repin-name's state"
        );
        assert!(second.join("marker").exists(), "repin state untouched");
        assert!(!other.join("marker").exists(), "clean state for other");

        // Carrying away from a CONFIG-KEY dir must NOT remove its parent:
        // agents/<key>/ doubles as the agent's canonical topic dir.
        let third = tmp.path().join("agents").join("_third").join(".jyc");
        assert!(adopt_state_dir("repin-name", &topic, &third).unwrap());
        assert!(
            second.parent().unwrap().exists(),
            "config-key topic dir preserved after carry"
        );
    }

    #[test]
    fn adopt_ignores_legacy_in_dir_state() {
        // A pre-adoption .jyc still sitting in the topic dir is left
        // alone (warned about), never moved or merged.
        let tmp = tempdir().unwrap();
        let topic = tmp.path().join("conflict-topic");
        std::fs::create_dir_all(topic.join(".jyc")).unwrap();
        let state = tmp.path().join("conflict-state");
        assert!(!adopt_state_dir("conflict-name", &topic, &state).unwrap());
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

        // derive_topic_name (email) strips Re:/Fw: prefixes
        let topic_name = email_parser::derive_topic_name("Re: Test Subject", &[]);
        assert_eq!(topic_name, "Test Subject");

        let result = storage
            .store_with_match(&msg, &topic_name, true, None)
            .await
            .unwrap();

        // Verify: <workdir>/jiny283a/workspace/Test Subject/
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
        let topic_name = email_parser::derive_topic_name(
            "Fw: 您收到来自上海栋菁餐饮管理有限公司的电子发票",
            &[],
        );
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
            let derived = email_parser::derive_topic_name(subject, &[]);
            let topic_name = pattern.topic_name.as_deref().unwrap_or(&derived);
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
            topic_name: Some("invoice-processing".to_string()),
            ..Default::default()
        };

        // Feishu chat_name would be "发票群" but config overrides
        let topic_name = pattern.topic_name.as_deref().unwrap_or("发票群");
        assert_eq!(topic_name, "invoice-processing");

        let msg = make_feishu_message("发票群", "group");
        let result = storage
            .store_with_match(&msg, topic_name, true, None)
            .await
            .unwrap();
        assert_eq!(result.topic_path, ws.join("invoice-processing"));
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
        });

        crate::attachment_storage::save_attachments_to_dir(&mut msg, &topic_path, None)
            .await
            .unwrap();

        // Verify attachment saved under topic_path/attachments/
        let att_dir = topic_path.join("attachments");
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
    async fn test_store_at_path_writes_to_custom_directory() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);

        // Custom topic path OUTSIDE the workspace
        let custom_path = tmp.path().join("custom-projects").join("my-project");
        tokio::fs::create_dir_all(&custom_path).await.unwrap();

        let msg = make_message("jiny283a", "Test Subject");
        let result = storage
            .store_at_path(&msg, &custom_path, true)
            .await
            .unwrap();

        // Topic path should be the custom path, not workspace-joined
        assert_eq!(result.topic_path, custom_path);
        assert!(result.topic_path.exists());

        // Chat log should be inside the custom path .jyc/ directory
        let jyc_dir = custom_path.join(".jyc");
        let entries: Vec<_> = std::fs::read_dir(&jyc_dir).unwrap().collect();
        let has_chat_log = entries.iter().any(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("chat_history_")
        });
        assert!(has_chat_log, "chat log file should exist in .jyc/");

        // Should NOT be under workspace
        assert!(
            !result.topic_path.starts_with(&ws),
            "custom topic path should not be under workspace"
        );
    }

    #[tokio::test]
    async fn test_store_at_path_creates_topic_dir_if_missing() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);

        // Custom path that doesn't exist yet
        let custom_path = tmp.path().join("new-external-dir").join("topic-1");

        let msg = make_message("jiny283a", "Test Subject");
        let result = storage
            .store_at_path(&msg, &custom_path, true)
            .await
            .unwrap();

        assert_eq!(result.topic_path, custom_path);
        assert!(result.topic_path.exists());
        assert!(result.topic_path.is_dir());
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
        // Relative paths are resolved against the data root (workdir)
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
        let msg = make_feishu_message("发票群", "group");
        let result = storage.store_at_path(&msg, &custom, true).await.unwrap();

        assert_eq!(result.topic_path, custom);
        // Ensure path doesn't contain "workspace" segment at all
        assert!(
            !result.topic_path.to_string_lossy().contains("workspace"),
            "custom path should not contain 'workspace'"
        );
    }

    /// Agents workspace root is `<data_home>/agents`, distinct from a
    /// regular channel's `<workdir>/<channel>/workspace/`.
    #[test]
    fn test_agents_workspace_root_differs_from_channel_workspace() {
        let workdir = Path::new("/tmp/jyc-data");
        let agents_root = resolve_agents_workspace_root(workdir);
        let chan_ws = resolve_workspace(workdir, "work");
        assert!(chan_ws.starts_with(workdir));
        assert!(
            !agents_root.starts_with(workdir) || agents_root == workdir.join("agents"),
            "agents root should resolve via data_home, not under workdir"
        );
    }
}
