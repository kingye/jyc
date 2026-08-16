//! Central topic path resolution.
//!
//! The topic directory follows the convention:
//!   `<workdir>/<channel>/workspace/<topic_name>/`

use std::path::{Path, PathBuf};

/// Resolve the workspace directory for a channel.
///
/// Convention: `<workdir>/<channel>/workspace/`
pub fn resolve_workspace(workdir: &Path, channel: &str) -> PathBuf {
    workdir.join(channel).join("workspace")
}

/// Resolve the shared repo directory for a repo group key.
///
/// Convention: `<workspace>/repos/<group_key>/`
pub fn resolve_shared_repo_dir(workspace: &Path, group_key: &str) -> PathBuf {
    workspace.join("repos").join(group_key)
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

/// Compute the repo group key from a `repo_group` config value and issue/PR number.
///
/// Returns `"{repo_group}-{number}"`.
/// Works for GitHub (u64), Gitee issues (string like "IJROW7"), and Gitee PRs (u64).
pub fn compute_repo_group_key(repo_group: &str, number: &str) -> String {
    format!("{}-{}", repo_group, number)
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

/// Resolve the topics root for an agent.
///
/// Convention: `<workdir>/agents/<agent>/` — topics of patterns that
/// reference an agent via `agent = "<name>"` live here instead of the
/// legacy `<workdir>/<channel>/workspace/`. See `docs/agents-migration.md`.
pub fn resolve_agent_topics_dir(workdir: &Path, agent: &str) -> PathBuf {
    workdir.join("agents").join(agent)
}

/// Resolve a channel's state directory.
///
/// Convention: `<workdir>/channels/<channel>/` — channel-owned data
/// (protocol state like `.github`/`.gitee` poll cursors). Topics no longer
/// live here once their pattern references an agent.
pub fn resolve_channel_state_dir(workdir: &Path, channel: &str) -> PathBuf {
    workdir.join("channels").join(channel)
}

/// Lazy one-time migration: if `new` does not exist and `legacy` does,
/// rename `legacy` into `new`'s place (creating parent dirs). No-op when
/// both exist (new wins) or neither exists.
pub fn migrate_dir_if_needed(legacy: &Path, new: &Path) {
    if new.exists() || !legacy.exists() {
        return;
    }
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::rename(legacy, new) {
        Ok(()) => {
            tracing::info!(
                from = %legacy.display(),
                to = %new.display(),
                "Migrated directory to new layout"
            );
        }
        Err(e) => {
            tracing::warn!(
                from = %legacy.display(),
                to = %new.display(),
                error = %e,
                "Failed to migrate directory; leaving legacy path in place"
            );
        }
    }
}

/// Resolve the effective topic directory override for a matched pattern.
///
/// Precedence (first hit wins):
/// 1. explicit `metadata_override` (dashboard `create_topic` path)
/// 2. the pattern's `topic_path` override (custom path, `~`-expanded)
/// 3. an agent-routed pattern: `<data_root>/agents/<agent>/<topic>` —
///    lazily migrating a pre-existing legacy
///    `<data_root>/<channel>/workspace/<topic>` dir into place
///
/// Shared by `MessageRouter` and the `send_to_topic` tool so agent-routed
/// topics resolve identically everywhere.
pub fn resolve_topic_path_override(
    pattern: Option<&jyc_types::ChannelPattern>,
    topic_name: &str,
    data_root: &Path,
    channel_name: &str,
    metadata_override: Option<&str>,
) -> Option<PathBuf> {
    if let Some(p) = metadata_override {
        return Some(PathBuf::from(p));
    }
    let pattern = pattern?;
    if let Some(tp) = &pattern.topic_path {
        return Some(resolve_topic_path(tp, data_root));
    }
    let agent = pattern.agent.as_deref()?;
    let dir = resolve_agent_topics_dir(data_root, agent).join(topic_name);
    let legacy = data_root
        .join(channel_name)
        .join("workspace")
        .join(topic_name);
    migrate_dir_if_needed(&legacy, &dir);
    Some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email_parser;
    use crate::message_storage::MessageStorage;
    use jyc_types::{ChannelPattern, InboundMessage, MessageContent};
    use std::collections::HashMap;
    use tempfile::tempdir;

    #[test]
    fn test_resolve_agent_topics_dir() {
        let root = Path::new("/data");
        assert_eq!(
            resolve_agent_topics_dir(root, "jyc"),
            PathBuf::from("/data/agents/jyc")
        );
    }

    #[test]
    fn test_resolve_channel_state_dir() {
        let root = Path::new("/data");
        assert_eq!(
            resolve_channel_state_dir(root, "jyc_repo"),
            PathBuf::from("/data/channels/jyc_repo")
        );
    }

    #[test]
    fn test_migrate_dir_if_needed_moves_legacy() {
        let tmp = tempdir().unwrap();
        let legacy = tmp.path().join("local_dev/workspace/jyc");
        let new = tmp.path().join("agents/jyc/jyc");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("marker"), "x").unwrap();

        migrate_dir_if_needed(&legacy, &new);

        assert!(!legacy.exists());
        assert!(new.join("marker").exists());
    }

    #[test]
    fn test_migrate_dir_if_needed_new_wins() {
        let tmp = tempdir().unwrap();
        let legacy = tmp.path().join("ch/workspace/t");
        let new = tmp.path().join("agents/a/t");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&new).unwrap();

        migrate_dir_if_needed(&legacy, &new);

        // Both kept; legacy untouched when the new dir already exists
        assert!(legacy.exists());
        assert!(new.exists());
    }

    #[test]
    fn test_migrate_dir_if_needed_noop_when_neither() {
        let tmp = tempdir().unwrap();
        let legacy = tmp.path().join("ch/workspace/t");
        let new = tmp.path().join("agents/a/t");
        migrate_dir_if_needed(&legacy, &new);
        assert!(!new.exists());
    }

    fn pattern_with(agent: Option<&str>, topic_path: Option<&str>) -> ChannelPattern {
        ChannelPattern {
            name: "p".into(),
            agent: agent.map(|s| s.to_string()),
            topic_path: topic_path.map(|s| s.to_string()),
            ..ChannelPattern::default()
        }
    }

    #[test]
    fn test_resolve_topic_path_override_precedence() {
        let root = Path::new("/data");
        let p = pattern_with(Some("a"), None);

        // 1. metadata override wins
        assert_eq!(
            resolve_topic_path_override(Some(&p), "t", root, "ch", Some("/abs/x")),
            Some(PathBuf::from("/abs/x"))
        );
        // 2. pattern topic_path beats agent dir
        let p2 = pattern_with(Some("a"), Some("~/proj"));
        let r = resolve_topic_path_override(Some(&p2), "t", root, "ch", None).unwrap();
        assert!(r.ends_with("proj"));
        // 3. agent dir
        assert_eq!(
            resolve_topic_path_override(Some(&p), "t", root, "ch", None),
            Some(PathBuf::from("/data/agents/a/t"))
        );
        // 4. no pattern
        assert_eq!(
            resolve_topic_path_override(None, "t", root, "ch", None),
            None
        );
    }

    #[test]
    fn test_resolve_topic_path_override_migrates_legacy() {
        let tmp = tempdir().unwrap();
        let legacy = tmp.path().join("ch/workspace/t");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("marker"), "x").unwrap();

        let p = pattern_with(Some("a"), None);
        let dir = resolve_topic_path_override(Some(&p), "t", tmp.path(), "ch", None).unwrap();

        assert_eq!(dir, tmp.path().join("agents/a/t"));
        assert!(!legacy.exists());
        assert!(dir.join("marker").exists());
    }

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

    #[test]
    fn test_resolve_shared_repo_dir() {
        let ws = Path::new("/data/github/workspace");
        let shared = resolve_shared_repo_dir(ws, "pr-42");
        assert_eq!(shared, PathBuf::from("/data/github/workspace/repos/pr-42"));
    }

    #[test]
    fn test_compute_repo_group_key() {
        assert_eq!(compute_repo_group_key("pr", "42"), "pr-42");
        assert_eq!(compute_repo_group_key("repo", "1"), "repo-1");
        assert_eq!(compute_repo_group_key("pr", "IJROW7"), "pr-IJROW7");
    }

    #[tokio::test]
    async fn test_symlink_creation_with_repo_group_key() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("github").join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        let group_key = compute_repo_group_key("pr", "42");
        let shared_repo_dir = resolve_shared_repo_dir(&workspace, &group_key);
        let topic_path = workspace.join("pr-42");

        tokio::fs::create_dir_all(&shared_repo_dir).await.unwrap();
        tokio::fs::create_dir_all(&topic_path).await.unwrap();

        let symlink_path = topic_path.join("repo");
        assert!(!symlink_path.exists());

        std::os::unix::fs::symlink(&shared_repo_dir, &symlink_path).unwrap();
        assert!(symlink_path.exists());
        assert!(
            tokio::fs::symlink_metadata(&symlink_path)
                .await
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let target = std::fs::read_link(&symlink_path).unwrap();
        assert_eq!(target, shared_repo_dir);
    }

    #[test]
    fn test_repo_group_backward_compatibility_no_field() {
        let pattern: jyc_types::ChannelPattern = toml::from_str(
            r#"
            name = "test"
            [rules]
        "#,
        )
        .unwrap();
        assert!(
            pattern.repo_group.is_none(),
            "repo_group should default to None when omitted from config"
        );
    }

    #[test]
    fn test_repo_group_set_via_serde() {
        let pattern: jyc_types::ChannelPattern = toml::from_str(
            r#"
            name = "test"
            repo_group = "pr"
            [rules]
        "#,
        )
        .unwrap();
        assert_eq!(pattern.repo_group.as_deref(), Some("pr"));
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
}
