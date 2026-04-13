//! Central thread path resolution.
//!
//! The thread directory follows the convention:
//!   `<workdir>/<channel>/workspace/<thread_name>/`
//!
//! This module provides a single source of truth for resolving thread paths,
//! preventing the double-nesting bugs that occur when path segments are
//! added multiple times in different modules.

use std::path::{Path, PathBuf};

/// Resolve the full thread directory path.
///
/// Convention: `<workdir>/<channel>/workspace/<thread_name>/`
///
/// - `workdir`: the jyc data root (e.g., `/home/user/jyc-data`)
/// - `channel`: the channel config name (e.g., `jiny283a`, `feishu_bot`)
/// - `thread_name`: the thread name (e.g., `invoice-processing`, `self-hosting-jyc`)
pub fn resolve_thread_path(workdir: &Path, channel: &str, thread_name: &str) -> PathBuf {
    workdir.join(channel).join("workspace").join(thread_name)
}

/// Resolve the workspace directory for a channel.
///
/// Convention: `<workdir>/<channel>/workspace/`
pub fn resolve_workspace(workdir: &Path, channel: &str) -> PathBuf {
    workdir.join(channel).join("workspace")
}

/// Resolve the attachments directory for a thread.
///
/// Convention: `<thread_path>/attachments/`
pub fn resolve_attachments_dir(thread_path: &Path) -> PathBuf {
    thread_path.join("attachments")
}

/// Resolve the messages directory for a thread.
///
/// Convention: `<thread_path>/messages/`
pub fn resolve_messages_dir(thread_path: &Path) -> PathBuf {
    thread_path.join("messages")
}

/// Resolve the .jyc state directory for a thread.
///
/// Convention: `<thread_path>/.jyc/`
pub fn resolve_jyc_dir(thread_path: &Path) -> PathBuf {
    thread_path.join(".jyc")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::{
        ChannelMatcher, ChannelPattern, InboundMessage, MessageContent, PatternMatch, PatternRules,
    };
    use crate::core::email_parser;
    use crate::core::message_storage::MessageStorage;
    use std::collections::HashMap;
    use tempfile::tempdir;

    // === Unit tests for path resolution ===

    #[test]
    fn test_resolve_thread_path() {
        let path = resolve_thread_path(
            Path::new("/home/user/jyc-data"),
            "jiny283a",
            "invoice-processing",
        );
        assert_eq!(
            path,
            PathBuf::from("/home/user/jyc-data/jiny283a/workspace/invoice-processing")
        );
    }

    #[test]
    fn test_resolve_workspace() {
        let path = resolve_workspace(Path::new("/home/user/jyc-data"), "feishu_bot");
        assert_eq!(
            path,
            PathBuf::from("/home/user/jyc-data/feishu_bot/workspace")
        );
    }

    #[test]
    fn test_resolve_attachments_dir() {
        let thread = PathBuf::from("/data/jiny283/workspace/invoices");
        assert_eq!(
            resolve_attachments_dir(&thread),
            PathBuf::from("/data/jiny283/workspace/invoices/attachments")
        );
    }

    #[test]
    fn test_resolve_jyc_dir() {
        let thread = PathBuf::from("/data/channel/workspace/thread");
        assert_eq!(
            resolve_jyc_dir(&thread),
            PathBuf::from("/data/channel/workspace/thread/.jyc")
        );
    }

    // === End-to-end: thread name from email subject → correct path ===

    #[test]
    fn test_email_subject_to_thread_path() {
        let workdir = Path::new("/home/user/jyc-data");
        let channel = "jiny283a";

        // Email subject → thread name
        let subject = "Re: Fw: Invoice for office supplies";
        let thread_name = email_parser::derive_thread_name(subject, &[]);
        assert_eq!(thread_name, "Invoice for office supplies");

        // Thread name → full path
        let path = resolve_thread_path(workdir, channel, &thread_name);
        assert_eq!(
            path,
            PathBuf::from("/home/user/jyc-data/jiny283a/workspace/Invoice for office supplies")
        );

        // No double nesting
        assert!(!path.to_string_lossy().contains("workspace/jiny283a/workspace"));
    }

    #[test]
    fn test_email_subject_with_prefix_strip_to_thread_path() {
        let workdir = Path::new("/data");
        let channel = "jiny283";
        let prefixes = vec!["Bs:".to_string()];

        let subject = "Re: Bs: self-hosting-jyc";
        let thread_name = email_parser::derive_thread_name(subject, &prefixes);
        assert_eq!(thread_name, "self-hosting-jyc");

        let path = resolve_thread_path(workdir, channel, &thread_name);
        assert_eq!(
            path,
            PathBuf::from("/data/jiny283/workspace/self-hosting-jyc")
        );
    }

    #[test]
    fn test_email_subject_chinese_to_thread_path() {
        let workdir = Path::new("/data");
        let channel = "jiny283a";

        let subject = "Fw: 您收到来自上海栋菁餐饮管理有限公司的电子发票";
        let thread_name = email_parser::derive_thread_name(subject, &[]);
        assert_eq!(thread_name, "您收到来自上海栋菁餐饮管理有限公司的电子发票");

        let path = resolve_thread_path(workdir, channel, &thread_name);
        assert!(path.to_string_lossy().ends_with("您收到来自上海栋菁餐饮管理有限公司的电子发票"));
    }

    // === End-to-end: thread name from config override → correct path ===

    fn make_test_message(topic: &str) -> InboundMessage {
        InboundMessage {
            id: "1".to_string(),
            channel: "test".to_string(),
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

    #[test]
    fn test_config_thread_name_override_to_thread_path() {
        let workdir = Path::new("/home/user/jyc-data");
        let channel = "jiny283a";

        // Pattern has thread_name override
        let pattern = ChannelPattern {
            name: "invoices".to_string(),
            thread_name: Some("invoice-processing".to_string()),
            ..Default::default()
        };

        // Different subjects all resolve to same thread path
        for subject in &[
            "Invoice for food",
            "发票 office supplies",
            "Receipt from hotel",
        ] {
            let thread_name = pattern
                .thread_name
                .as_deref()
                .unwrap_or(subject);

            let path = resolve_thread_path(workdir, channel, thread_name);
            assert_eq!(
                path,
                PathBuf::from("/home/user/jyc-data/jiny283a/workspace/invoice-processing"),
                "Subject '{}' should resolve to invoice-processing",
                subject
            );
        }
    }

    #[test]
    fn test_config_no_override_falls_back_to_derived() {
        let workdir = Path::new("/data");
        let channel = "jiny283a";

        // Pattern without thread_name override
        let pattern = ChannelPattern {
            name: "general".to_string(),
            thread_name: None,
            ..Default::default()
        };

        let subject = "Some email topic";
        let derived = email_parser::derive_thread_name(subject, &[]);
        let thread_name = pattern
            .thread_name
            .as_deref()
            .unwrap_or(&derived);

        let path = resolve_thread_path(workdir, channel, thread_name);
        assert_eq!(
            path,
            PathBuf::from("/data/jiny283a/workspace/Some email topic")
        );
    }

    // === End-to-end: storage creates correct path ===

    #[tokio::test]
    async fn test_storage_creates_correct_thread_directory() {
        let tmp = tempdir().unwrap();
        let workdir = tmp.path();
        let channel = "jiny283a";

        let workspace_dir = resolve_workspace(workdir, channel);
        tokio::fs::create_dir_all(&workspace_dir).await.unwrap();

        let storage = MessageStorage::new(&workspace_dir);
        let message = make_test_message("Test Subject");

        let result = storage
            .store_with_match(&message, "invoice-processing", true, None)
            .await
            .unwrap();

        // Thread path should be workspace/thread_name — no double nesting
        let expected = workspace_dir.join("invoice-processing");
        assert_eq!(result.thread_path, expected);
        assert!(result.thread_path.exists());

        // Verify no double nesting
        let path_str = result.thread_path.to_string_lossy();
        assert!(!path_str.contains("workspace/jiny283a/workspace"));
        assert!(path_str.ends_with("invoice-processing"));
    }

    #[tokio::test]
    async fn test_storage_with_derived_thread_name() {
        let tmp = tempdir().unwrap();
        let workdir = tmp.path();
        let channel = "feishu_bot";

        let workspace_dir = resolve_workspace(workdir, channel);
        tokio::fs::create_dir_all(&workspace_dir).await.unwrap();

        let storage = MessageStorage::new(&workspace_dir);
        let message = make_test_message("旅行计划");

        let result = storage
            .store_with_match(&message, "旅行计划", true, None)
            .await
            .unwrap();

        let expected = workspace_dir.join("旅行计划");
        assert_eq!(result.thread_path, expected);
        assert!(result.thread_path.exists());
    }

    // === Attachment path verification ===

    #[test]
    fn test_attachment_dir_no_double_nesting() {
        let workdir = Path::new("/data");
        let channel = "jiny283a";
        let thread_name = "invoice-processing";

        let thread_path = resolve_thread_path(workdir, channel, thread_name);
        let attachments = resolve_attachments_dir(&thread_path);

        assert_eq!(
            attachments,
            PathBuf::from("/data/jiny283a/workspace/invoice-processing/attachments")
        );

        // No double nesting
        let path_str = attachments.to_string_lossy();
        assert_eq!(path_str.matches("workspace").count(), 1);
        assert_eq!(path_str.matches("jiny283a").count(), 1);
    }

    // === .jyc state dir verification ===

    #[test]
    fn test_jyc_dir_no_double_nesting() {
        let workdir = Path::new("/data");
        let channel = "feishu_bot";
        let thread_name = "self-hosting-jyc";

        let thread_path = resolve_thread_path(workdir, channel, thread_name);
        let jyc_dir = resolve_jyc_dir(&thread_path);

        assert_eq!(
            jyc_dir,
            PathBuf::from("/data/feishu_bot/workspace/self-hosting-jyc/.jyc")
        );
    }

    // === Full chain: workdir → channel → thread → subdirectory ===

    #[test]
    fn test_full_path_chain_consistency() {
        let workdir = Path::new("/home/jiny/projects/jyc-data");
        let channel = "jiny283a";
        let thread_name = "invoice-processing";

        let workspace = resolve_workspace(workdir, channel);
        let thread_path = resolve_thread_path(workdir, channel, thread_name);
        let attachments = resolve_attachments_dir(&thread_path);
        let jyc_dir = resolve_jyc_dir(&thread_path);
        let messages = resolve_messages_dir(&thread_path);

        // All paths share the same prefix
        assert!(thread_path.starts_with(&workspace));
        assert!(attachments.starts_with(&thread_path));
        assert!(jyc_dir.starts_with(&thread_path));
        assert!(messages.starts_with(&thread_path));

        // Exact paths
        assert_eq!(workspace, PathBuf::from("/home/jiny/projects/jyc-data/jiny283a/workspace"));
        assert_eq!(thread_path, PathBuf::from("/home/jiny/projects/jyc-data/jiny283a/workspace/invoice-processing"));
        assert_eq!(attachments, PathBuf::from("/home/jiny/projects/jyc-data/jiny283a/workspace/invoice-processing/attachments"));
        assert_eq!(jyc_dir, PathBuf::from("/home/jiny/projects/jyc-data/jiny283a/workspace/invoice-processing/.jyc"));
        assert_eq!(messages, PathBuf::from("/home/jiny/projects/jyc-data/jiny283a/workspace/invoice-processing/messages"));
    }

    // === Feishu chat_name → thread path ===

    fn make_feishu_message(chat_name: Option<&str>, chat_type: &str, sender_name: Option<&str>, chat_id: Option<&str>) -> InboundMessage {
        let mut metadata = HashMap::new();
        if let Some(name) = chat_name {
            metadata.insert("chat_name".to_string(), serde_json::json!(name));
        }
        metadata.insert("chat_type".to_string(), serde_json::json!(chat_type));
        if let Some(name) = sender_name {
            metadata.insert("sender_name".to_string(), serde_json::json!(name));
        }
        if let Some(id) = chat_id {
            metadata.insert("chat_id".to_string(), serde_json::json!(id));
        }

        InboundMessage {
            id: "1".to_string(),
            channel: "feishu_bot".to_string(),
            channel_uid: "1".to_string(),
            sender: "ou_xxx".to_string(),
            sender_address: "ou_xxx".to_string(),
            recipients: vec![],
            topic: "".to_string(),
            content: MessageContent::default(),
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata,
            matched_pattern: None,
        }
    }

    #[test]
    fn test_feishu_group_chat_name_to_thread_path() {
        use crate::channels::feishu::inbound::FeishuMatcher;

        let workdir = Path::new("/data");
        let channel = "feishu_bot";
        let message = make_feishu_message(Some("self-hosting-jyc"), "group", None, None);

        let matcher = FeishuMatcher;
        let thread_name = matcher.derive_thread_name(&message, &[], None);
        assert_eq!(thread_name, "self-hosting-jyc");

        let path = resolve_thread_path(workdir, channel, &thread_name);
        assert_eq!(
            path,
            PathBuf::from("/data/feishu_bot/workspace/self-hosting-jyc")
        );
    }

    #[test]
    fn test_feishu_chinese_chat_name_to_thread_path() {
        use crate::channels::feishu::inbound::FeishuMatcher;

        let workdir = Path::new("/data");
        let channel = "feishu_bot";
        let message = make_feishu_message(Some("五一松赞"), "group", None, None);

        let matcher = FeishuMatcher;
        let thread_name = matcher.derive_thread_name(&message, &[], None);
        assert_eq!(thread_name, "五一松赞");

        let path = resolve_thread_path(workdir, channel, &thread_name);
        assert_eq!(
            path,
            PathBuf::from("/data/feishu_bot/workspace/五一松赞")
        );
    }

    #[test]
    fn test_feishu_p2p_sender_name_to_thread_path() {
        use crate::channels::feishu::inbound::FeishuMatcher;

        let workdir = Path::new("/data");
        let channel = "feishu_bot";
        // P2P with no chat_name but has sender_name
        let message = make_feishu_message(None, "p2p", Some("张三"), None);

        let matcher = FeishuMatcher;
        let thread_name = matcher.derive_thread_name(&message, &[], None);
        assert_eq!(thread_name, "张三");

        let path = resolve_thread_path(workdir, channel, &thread_name);
        assert_eq!(
            path,
            PathBuf::from("/data/feishu_bot/workspace/张三")
        );
    }

    #[test]
    fn test_feishu_fallback_chat_id_to_thread_path() {
        use crate::channels::feishu::inbound::FeishuMatcher;

        let workdir = Path::new("/data");
        let channel = "feishu_bot";
        // No chat_name, no sender_name, only chat_id
        let message = make_feishu_message(None, "group", None, Some("oc_abc123"));

        let matcher = FeishuMatcher;
        let thread_name = matcher.derive_thread_name(&message, &[], None);
        assert_eq!(thread_name, "feishu_chat_oc_abc123");

        let path = resolve_thread_path(workdir, channel, &thread_name);
        assert_eq!(
            path,
            PathBuf::from("/data/feishu_bot/workspace/feishu_chat_oc_abc123")
        );
    }

    #[test]
    fn test_feishu_chat_name_with_config_override() {
        let workdir = Path::new("/data");
        let channel = "feishu_bot";

        // Pattern overrides chat_name-derived thread name
        let pattern = ChannelPattern {
            name: "invoices".to_string(),
            thread_name: Some("invoice-processing".to_string()),
            ..Default::default()
        };

        // Chat name would derive "发票处理群" but config overrides to "invoice-processing"
        let derived = "发票处理群";
        let thread_name = pattern.thread_name.as_deref().unwrap_or(derived);
        assert_eq!(thread_name, "invoice-processing");

        let path = resolve_thread_path(workdir, channel, thread_name);
        assert_eq!(
            path,
            PathBuf::from("/data/feishu_bot/workspace/invoice-processing")
        );
    }
}
