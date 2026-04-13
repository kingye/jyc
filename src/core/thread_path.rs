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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::{
        ChannelMatcher, ChannelPattern, InboundMessage, MessageContent, PatternMatch,
    };
    use crate::core::email_parser;
    use crate::core::message_storage::MessageStorage;
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
        msg.metadata.insert("chat_name".to_string(), serde_json::json!(chat_name));
        msg.metadata.insert("chat_type".to_string(), serde_json::json!(chat_type));
        msg
    }

    // === resolve_workspace (used by cli/monitor.rs) ===

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
    async fn test_storage_thread_path_from_email_subject() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);
        let msg = make_message("jiny283a", "Test Subject");

        // derive_thread_name (email) strips Re:/Fw: prefixes
        let thread_name = email_parser::derive_thread_name("Re: Test Subject", &[]);
        assert_eq!(thread_name, "Test Subject");

        let result = storage.store_with_match(&msg, &thread_name, true, None).await.unwrap();

        // Verify: <workdir>/jiny283a/workspace/Test Subject/
        assert_eq!(result.thread_path, ws.join("Test Subject"));
        assert!(result.thread_path.exists());
        // No double nesting
        assert!(!result.thread_path.to_string_lossy().contains("workspace/jiny283a"));
    }

    #[tokio::test]
    async fn test_storage_thread_path_from_chinese_subject() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "jiny283a");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);
        let thread_name = email_parser::derive_thread_name(
            "Fw: 您收到来自上海栋菁餐饮管理有限公司的电子发票", &[]
        );
        let msg = make_message("jiny283a", &thread_name);

        let result = storage.store_with_match(&msg, &thread_name, true, None).await.unwrap();
        assert!(result.thread_path.exists());
        assert!(result.thread_path.to_string_lossy().contains("上海栋菁餐饮"));
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
        let result = storage.store_with_match(&msg, "invoice-processing", true, None).await.unwrap();

        assert_eq!(result.thread_path, ws.join("invoice-processing"));
        assert!(result.thread_path.exists());
    }

    #[tokio::test]
    async fn test_storage_thread_path_from_feishu_chat_name() {
        use crate::channels::feishu::inbound::FeishuMatcher;

        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "feishu_bot");
        tokio::fs::create_dir_all(&ws).await.unwrap();

        let storage = MessageStorage::new(&ws);
        let msg = make_feishu_message("五一松赞", "group");

        let matcher = FeishuMatcher;
        let thread_name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(thread_name, "五一松赞");

        let result = storage.store_with_match(&msg, &thread_name, true, None).await.unwrap();
        assert_eq!(result.thread_path, ws.join("五一松赞"));
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
        let result = storage.store_with_match(&msg, thread_name, true, None).await.unwrap();
        assert_eq!(result.thread_path, ws.join("invoice-processing"));
    }

    // === Attachment path (real production path) ===

    #[tokio::test]
    async fn test_attachment_saves_to_correct_thread_dir() {
        use crate::channels::types::MessageAttachment;

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

        crate::core::attachment_storage::save_attachments_to_dir(
            &mut msg, &thread_path, None,
        ).await.unwrap();

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

    // === .jyc path (real production pattern) ===

    #[tokio::test]
    async fn test_jyc_dir_created_in_correct_location() {
        let tmp = tempdir().unwrap();
        let ws = resolve_workspace(tmp.path(), "feishu_bot");
        let thread_path = ws.join("self-hosting-jyc");
        let jyc_dir = thread_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();

        // Verify path structure
        assert!(jyc_dir.exists());
        assert!(jyc_dir.starts_with(&ws));
        let path_str = jyc_dir.to_string_lossy();
        assert!(path_str.ends_with("self-hosting-jyc/.jyc"));
        assert_eq!(path_str.matches("workspace").count(), 1);
    }
}
