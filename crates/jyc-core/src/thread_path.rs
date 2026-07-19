//! Central thread path resolution.
//!
//! The thread directory follows the convention:
//!   `<workdir>/<channel>/workspace/<thread_name>/`

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

/// Resolve a custom thread path from a pattern's `thread_path` config.
///
/// Expands `~` to `$HOME`. Absolute paths are used as-is.
pub fn resolve_thread_path(path: &str) -> PathBuf {
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
        PathBuf::from(path)
    }
}

/// Compute the repo group key from a `repo_group` config value and issue/PR number.
///
/// Returns `"{repo_group}-{number}"`.
/// Works for GitHub (u64), Gitee issues (string like "IJROW7"), and Gitee PRs (u64).
pub fn compute_repo_group_key(repo_group: &str, number: &str) -> String {
    format!("{}-{}", repo_group, number)
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
            thread_refs: None,
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
    fn test_resolve_thread_path_absolute() {
        let p = resolve_thread_path("/home/jiny/my-project");
        assert_eq!(p, PathBuf::from("/home/jiny/my-project"));
    }

    #[test]
    fn test_resolve_thread_path_tilde() {
        let p = resolve_thread_path("~/my-project");
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
        let thread_path = workspace.join("pr-42");

        tokio::fs::create_dir_all(&shared_repo_dir).await.unwrap();
        tokio::fs::create_dir_all(&thread_path).await.unwrap();

        let symlink_path = thread_path.join("repo");
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
    async fn test_storage_thread_path_from_email_subject() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);
        let msg = make_message("jiny283a", "Test Subject");

        // derive_thread_name (email) strips Re:/Fw: prefixes
        let thread_name = email_parser::derive_thread_name("Re: Test Subject", &[]);
        assert_eq!(thread_name, "Test Subject");

        let result = storage
            .store_with_match(&msg, &thread_name, true, None)
            .await
            .unwrap();

        // Verify: <workdir>/jiny283a/workspace/Test Subject/
        assert_eq!(result.thread_path, ws.join("Test Subject"));
        assert!(result.thread_path.exists());
        // No double nesting
        assert!(
            !result
                .thread_path
                .to_string_lossy()
                .contains("workspace/jiny283a")
        );
    }

    #[tokio::test]
    async fn test_storage_thread_path_from_chinese_subject() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);
        let thread_name = email_parser::derive_thread_name(
            "Fw: 您收到来自上海栋菁餐饮管理有限公司的电子发票",
            &[],
        );
        let msg = make_message("jiny283a", &thread_name);

        let result = storage
            .store_with_match(&msg, &thread_name, true, None)
            .await
            .unwrap();
        assert!(result.thread_path.exists());
        assert!(
            result
                .thread_path
                .to_string_lossy()
                .contains("上海栋菁餐饮")
        );
    }

    #[tokio::test]
    async fn test_storage_thread_path_from_config_override() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);

        // Pattern has thread_name override
        let pattern = ChannelPattern {
            name: "invoices".to_string(),
            thread_name: Some("invoice-processing".to_string()),
            ..Default::default()
        };

        // Different subjects all go to same thread
        for subject in &["Invoice food", "发票 office", "Receipt hotel"] {
            let derived = email_parser::derive_thread_name(subject, &[]);
            let thread_name = pattern.thread_name.as_deref().unwrap_or(&derived);
            assert_eq!(thread_name, "invoice-processing");
        }

        let msg = make_message("jiny283a", "Invoice food");
        let result = storage
            .store_with_match(&msg, "invoice-processing", true, None)
            .await
            .unwrap();

        assert_eq!(result.thread_path, ws.join("invoice-processing"));
        assert!(result.thread_path.exists());
    }

    #[tokio::test]
    async fn test_storage_thread_path_from_feishu_with_config_override() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "feishu_bot");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);

        let pattern = ChannelPattern {
            name: "invoices".to_string(),
            thread_name: Some("invoice-processing".to_string()),
            ..Default::default()
        };

        // Feishu chat_name would be "发票群" but config overrides
        let thread_name = pattern.thread_name.as_deref().unwrap_or("发票群");
        assert_eq!(thread_name, "invoice-processing");

        let msg = make_feishu_message("发票群", "group");
        let result = storage
            .store_with_match(&msg, thread_name, true, None)
            .await
            .unwrap();
        assert_eq!(result.thread_path, ws.join("invoice-processing"));
    }

    // === Attachment path (real production path) ===

    #[tokio::test]
    async fn test_attachment_saves_to_correct_thread_dir() {
        use jyc_types::MessageAttachment;

        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        let thread_path = ws.join("invoice-processing");
        tokio::fs::create_dir_all(&thread_path).await.unwrap();

        let mut msg = make_message("jiny283a", "Invoice");
        msg.attachments.push(MessageAttachment {
            filename: "test.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 5,
            content: Some(b"hello".to_vec()),
            saved_path: None,
        });

        crate::attachment_storage::save_attachments_to_dir(&mut msg, &thread_path, None)
            .await
            .unwrap();

        // Verify attachment saved under thread_path/attachments/
        let att_dir = thread_path.join("attachments");
        assert!(att_dir.exists());

        // No double nesting
        let att_path_str = att_dir.to_string_lossy();
        assert_eq!(att_path_str.matches("workspace").count(), 1);
        assert!(!att_path_str.contains("jiny283a/workspace/jiny283a"));

        // File exists
        assert!(msg.attachments[0].saved_path.is_some());
        assert!(msg.attachments[0].saved_path.as_ref().unwrap().exists());
    }

    // === store_at_path (custom thread_path override) ===

    #[tokio::test]
    async fn test_store_at_path_writes_to_custom_directory() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);

        // Custom thread path OUTSIDE the workspace
        let custom_path = tmp.path().join("custom-projects").join("my-project");
        tokio::fs::create_dir_all(&custom_path).await.unwrap();

        let msg = make_message("jiny283a", "Test Subject");
        let result = storage
            .store_at_path(&msg, &custom_path, true)
            .await
            .unwrap();

        // Thread path should be the custom path, not workspace-joined
        assert_eq!(result.thread_path, custom_path);
        assert!(result.thread_path.exists());

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
            !result.thread_path.starts_with(&ws),
            "custom thread path should not be under workspace"
        );
    }

    #[tokio::test]
    async fn test_store_at_path_creates_thread_dir_if_missing() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);

        // Custom path that doesn't exist yet
        let custom_path = tmp.path().join("new-external-dir").join("thread-1");

        let msg = make_message("jiny283a", "Test Subject");
        let result = storage
            .store_at_path(&msg, &custom_path, true)
            .await
            .unwrap();

        assert_eq!(result.thread_path, custom_path);
        assert!(result.thread_path.exists());
        assert!(result.thread_path.is_dir());
    }

    // === resolve_thread_path edge cases ===

    #[test]
    fn test_resolve_thread_path_home_only() {
        let p = resolve_thread_path("~");
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(p, PathBuf::from(home));
        } else {
            assert_eq!(p, PathBuf::from("~"));
        }
    }

    #[test]
    fn test_resolve_thread_path_relative() {
        // Relative paths are used as-is (no workspace resolution)
        let p = resolve_thread_path("my-project");
        assert_eq!(p, PathBuf::from("my-project"));
    }

    #[tokio::test]
    async fn test_thread_path_override_not_under_workspace() {
        // Verify that store_at_path produces a path completely outside
        // the standard workspace hierarchy.
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "feishu_bot");

        let custom = tmp.path().join("elsewhere");
        tokio::fs::create_dir_all(&custom).await.unwrap();

        let storage = MessageStorage::new(&ws);
        let msg = make_feishu_message("发票群", "group");
        let result = storage.store_at_path(&msg, &custom, true).await.unwrap();

        assert_eq!(result.thread_path, custom);
        // Ensure path doesn't contain "workspace" segment at all
        assert!(
            !result.thread_path.to_string_lossy().contains("workspace"),
            "custom path should not contain 'workspace'"
        );
    }
}
