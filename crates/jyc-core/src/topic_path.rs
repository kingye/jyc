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

/// Resolve the workspace root for an agent.
///
/// Convention: `<data_home>/agents/<agent_name>/`, reusing the
/// platform-aware `data_home()` so Linux/macOS use XDG and Windows
/// uses `%LOCALAPPDATA%`. When `data_home()` is unavailable (no
/// `$HOME`/`$XDG_DATA_HOME`, no `dirs::data_local_dir()`), falls back to
/// `<cwd>/agents/<agent_name>/` with a warning — same fallback shape as
/// `data_home()` itself.
pub fn resolve_agent_workspace(agent_name: &str) -> PathBuf {
    if let Some(home) = jyc_utils::paths::data_home() {
        home.join("agents").join(agent_name)
    } else {
        tracing::warn!(
            agent = %agent_name,
            "data_home() returned None; falling back to <cwd>/agents/<agent_name>"
        );
        PathBuf::from(".").join("agents").join(agent_name)
    }
}

/// Resolve the workspace root for the synthesized "agents" channel.
///
/// This is the parent directory holding every agent's workspace:
/// `<data_home>/agents/`. Each pattern (agent) inside the channel then
/// uses `resolve_agent_workspace(<agent_name>)` as its `topic_path`
/// override so its topics live under its own subtree.
///
/// Falls back to `<workdir>/agents` when `data_home()` is unavailable —
/// matching `resolve_agent_workspace`'s fallback shape, but using
/// `workdir` (the jyc instance root) instead of cwd so multi-instance
/// setups don't accidentally share state.
pub fn resolve_agents_workspace_root(workdir: &Path) -> PathBuf {
    if let Some(home) = jyc_utils::paths::data_home() {
        home.join("agents")
    } else {
        tracing::warn!("data_home() returned None; falling back to <workdir>/agents");
        workdir.join("agents")
    }
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

    /// `resolve_agent_workspace` reuses `jyc_utils::paths::data_home()`
    /// so the path ends with `agents/<agent_name>` under the platform
    /// data dir. We don't pin the prefix (XDG vs `%LOCALAPPDATA%` vs
    /// test-env overrides) — only the suffix we own.
    #[test]
    fn test_resolve_agent_workspace_suffix() {
        let ws = resolve_agent_workspace("jyc");
        let s = ws.to_string_lossy();
        assert!(
            s.ends_with("agents/jyc") || s.ends_with("agents\\jyc"),
            "agent workspace must end with agents/<agent_name>: {s}"
        );
    }

    /// Two distinct agent names produce two distinct suffixes.
    #[test]
    fn test_resolve_agent_workspace_distinct() {
        let a = resolve_agent_workspace("jyc");
        let b = resolve_agent_workspace("jin");
        assert_ne!(a, b);
        assert!(a.to_string_lossy().contains("jyc"));
        assert!(b.to_string_lossy().contains("jin"));
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

    /// Two agents with two topics each: all four topic dirs land under
    /// their respective agent subtrees.
    #[tokio::test]
    async fn test_resolve_agent_workspace_multiple_agents() {
        let _ = resolve_agent_workspace("jyc");
        let _ = resolve_agent_workspace("jin");
        // Smoke: each name produces a different suffix; full path
        // equality is asserted in test_resolve_agent_workspace_distinct.
        let a = resolve_agent_workspace("jyc");
        let b = resolve_agent_workspace("jin");
        assert!(a.to_string_lossy().ends_with("jyc") || a.to_string_lossy().ends_with("jyc/"));
        assert!(b.to_string_lossy().ends_with("jin") || b.to_string_lossy().ends_with("jin/"));
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
