#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use arc_swap::ArcSwap;
    use tempfile::TempDir;
    use crate::services::static_agent::StaticAgentService;

    fn test_config() -> Arc<ArcSwap<crate::config::types::AppConfig>> {
        Arc::new(ArcSwap::from_pointee(
            crate::config::load_config_from_str(
                r#"
[general]
[channels.test]
type = "email"
[channels.test.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.test.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "opencode"
"#,
            )
            .unwrap(),
        ))
    }

    #[tokio::test]
    async fn test_close_thread_deletes_directory() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let thread_dir = workspace.join("test_thread");
        std::fs::create_dir_all(&thread_dir).unwrap();
        std::fs::write(thread_dir.join("test.txt"), "content").unwrap();

        let storage = Arc::new(crate::core::message_storage::MessageStorage::new(&workspace));
        
        let thread_manager = crate::core::thread_manager::ThreadManager::new(
            3,
            10,
            storage,
            Arc::new(crate::channels::email::outbound::EmailOutboundAdapter::new(
                &crate::config::types::SmtpConfig {
                    host: "smtp.example.com".into(),
                    port: 465,
                    secure: true,
                    username: "test".into(),
                    password: "test".into(),
                    from_address: Some("test@example.com".into()),
                    from_name: None,
                },
                Arc::new(crate::core::message_storage::MessageStorage::new(&workspace)),
            )),
            Arc::new(StaticAgentService::new("test reply")),
            tokio_util::sync::CancellationToken::new(),
            crate::config::types::HeartbeatConfig::default(),
            "".into(),
            PathBuf::from("/tmp/templates"),
            test_config(),
            "test".to_string(),
            workspace.clone(),
            crate::core::metrics::MetricsHandle::noop(),
        );

        let result = thread_manager.close_thread("test_thread").await;
        assert!(result.is_ok());
        assert!(!thread_dir.exists());
    }

    #[tokio::test]
    async fn test_close_nonexistent_thread_succeeds() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let storage = Arc::new(crate::core::message_storage::MessageStorage::new(&workspace));
        
        let thread_manager = crate::core::thread_manager::ThreadManager::new(
            3,
            10,
            storage,
            Arc::new(crate::channels::email::outbound::EmailOutboundAdapter::new(
                &crate::config::types::SmtpConfig {
                    host: "smtp.example.com".into(),
                    port: 465,
                    secure: true,
                    username: "test".into(),
                    password: "test".into(),
                    from_address: Some("test@example.com".into()),
                    from_name: None,
                },
                Arc::new(crate::core::message_storage::MessageStorage::new(&workspace)),
            )),
            Arc::new(StaticAgentService::new("test reply")),
            tokio_util::sync::CancellationToken::new(),
            crate::config::types::HeartbeatConfig::default(),
            "".into(),
            PathBuf::from("/tmp/templates"),
            test_config(),
            "test".to_string(),
            workspace.clone(),
            crate::core::metrics::MetricsHandle::noop(),
        );

        let result = thread_manager.close_thread("test_thread").await;
        assert!(result.is_ok());

    }

    #[tokio::test]
    async fn test_close_thread_removes_subdirectories() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let thread_dir = workspace.join("test_thread");
        std::fs::create_dir_all(thread_dir.join("subdir1")).unwrap();
        std::fs::create_dir_all(thread_dir.join("subdir2")).unwrap();
        std::fs::write(thread_dir.join("file1.txt"), "content1").unwrap();
        std::fs::write(thread_dir.join("subdir1/file2.txt"), "content2").unwrap();

        let storage = Arc::new(crate::core::message_storage::MessageStorage::new(&workspace));
        
        let thread_manager = crate::core::thread_manager::ThreadManager::new(
            3,
            10,
            storage,
            Arc::new(crate::channels::email::outbound::EmailOutboundAdapter::new(
                &crate::config::types::SmtpConfig {
                    host: "smtp.example.com".into(),
                    port: 465,
                    secure: true,
                    username: "test".into(),
                    password: "test".into(),
                    from_address: Some("test@example.com".into()),
                    from_name: None,
                },
                Arc::new(crate::core::message_storage::MessageStorage::new(&workspace)),
            )),
            Arc::new(StaticAgentService::new("test reply")),
            tokio_util::sync::CancellationToken::new(),
            crate::config::types::HeartbeatConfig::default(),
            "".into(),
            PathBuf::from("/tmp/templates"),
            test_config(),
            "test".to_string(),
            workspace.clone(),
            crate::core::metrics::MetricsHandle::noop(),
        );

        let result = thread_manager.close_thread("test_thread").await;
        assert!(result.is_ok());
        assert!(!thread_dir.exists());
    }

    /// Verify that a closed dummy channel returns None immediately.
    /// This is the mechanism used when live_injection is disabled:
    /// the SSE select loop's pending_rx arm fires instantly with None,
    /// effectively skipping live injection.
    #[tokio::test]
    async fn test_dummy_channel_returns_none_immediately() {
        use tokio::sync::mpsc;
        use crate::core::thread_manager::QueueItem;

        let (_tx, mut dummy_rx) = mpsc::channel::<QueueItem>(1);
        drop(_tx); // Close the sender immediately

        // recv() should return None instantly since sender is dropped
        let result = dummy_rx.recv().await;
        assert!(result.is_none(), "Closed channel should return None");
    }

    /// Verify that the real channel still works for live injection.
    /// Messages sent while the receiver is alive should be received.
    #[tokio::test]
    async fn test_real_channel_receives_messages() {
        use tokio::sync::mpsc;
        use crate::core::thread_manager::QueueItem;
        use crate::channels::types::{InboundMessage, MessageContent, PatternMatch};
        use std::collections::HashMap;

        let (tx, mut rx) = mpsc::channel::<QueueItem>(10);

        let item = QueueItem {
            message: InboundMessage {
                id: "1".to_string(),
                channel: "test".to_string(),
                channel_uid: "1".to_string(),
                sender: "user".to_string(),
                sender_address: "user@test".to_string(),
                recipients: vec![],
                topic: "test".to_string(),
                content: MessageContent::default(),
                timestamp: chrono::Utc::now(),
                thread_refs: None,
                reply_to_id: None,
                external_id: None,
                attachments: vec![],
                metadata: HashMap::new(),
                matched_pattern: None,
            },
            pattern_match: PatternMatch {
                pattern_name: "test".to_string(),
                channel: "test".to_string(),
                matches: HashMap::new(),
            },
            attachment_config: None,
            template: None,
            live_injection: true,
        };

        tx.send(item).await.unwrap();
        let received = rx.recv().await;
        assert!(received.is_some(), "Real channel should receive messages");
        assert_eq!(received.unwrap().message.id, "1");
    }
}