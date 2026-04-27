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

    /// Verify that close_thread cancels the per-thread CancellationToken,
    /// which is the mechanism that interrupts the SSE stream in prompt_with_sse.
    #[tokio::test]
    async fn test_close_thread_cancels_token() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let thread_dir = workspace.join("test_thread");
        std::fs::create_dir_all(&thread_dir).unwrap();

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

        // Insert a cancellation token manually (simulating what create_and_enqueue does)
        let token = tokio_util::sync::CancellationToken::new();
        {
            let mut cancels = thread_manager.thread_cancels.lock().await;
            cancels.insert("test_thread".to_string(), token.clone());
        }
        assert!(!token.is_cancelled());

        thread_manager.close_thread("test_thread").await.unwrap();

        assert!(token.is_cancelled(), "close_thread should cancel the per-thread token");
    }

    /// Verify that a deleted thread directory is correctly detected,
    /// which is the guard condition used in process_message to skip
    /// reply delivery when the thread was closed during AI processing.
    #[tokio::test]
    async fn test_deleted_thread_directory_detection() {
        let tmp = TempDir::new().unwrap();
        let thread_path = tmp.path().join("test_thread");
        std::fs::create_dir_all(&thread_path).unwrap();
        assert!(thread_path.exists(), "Thread directory should exist initially");

        std::fs::remove_dir_all(&thread_path).unwrap();
        assert!(!thread_path.exists(), "Thread directory should not exist after deletion");

        // This is the same check used in process_message():
        // if !store_result.thread_path.exists() { return Ok(()); }
        // The guard correctly skips reply delivery when the directory is gone.
    }

    #[tokio::test]
    async fn test_close_thread_removes_symlink_preserves_shared_repo() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let shared_repo = workspace.join("repos").join("pr-42");
        std::fs::create_dir_all(&shared_repo).unwrap();
        std::fs::write(shared_repo.join("test.txt"), "shared content").unwrap();

        let thread_dir = workspace.join("pr-42");
        std::fs::create_dir_all(&thread_dir).unwrap();
        std::os::unix::fs::symlink(&shared_repo, thread_dir.join("repo")).unwrap();

        assert!(thread_dir.join("repo").exists());
        assert!(shared_repo.join("test.txt").exists());

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

        let result = thread_manager.close_thread("pr-42").await;
        assert!(result.is_ok());
        assert!(!thread_dir.exists(), "Thread directory should be deleted");
        assert!(!shared_repo.exists(), "Orphaned shared repo should be cleaned up");
    }

    #[tokio::test]
    async fn test_close_thread_preserves_shared_repo_when_still_referenced() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let shared_repo = workspace.join("repos").join("pr-42");
        std::fs::create_dir_all(&shared_repo).unwrap();
        std::fs::write(shared_repo.join("test.txt"), "shared content").unwrap();

        // Thread 1 with symlink
        let thread1_dir = workspace.join("pr-42");
        std::fs::create_dir_all(&thread1_dir).unwrap();
        std::os::unix::fs::symlink(&shared_repo, thread1_dir.join("repo")).unwrap();

        // Thread 2 also with symlink to same shared repo
        let thread2_dir = workspace.join("review-pr-42");
        std::fs::create_dir_all(&thread2_dir).unwrap();
        std::os::unix::fs::symlink(&shared_repo, thread2_dir.join("repo")).unwrap();

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

        // Close thread 1 — shared repo should still be referenced by thread 2
        let result = thread_manager.close_thread("pr-42").await;
        assert!(result.is_ok());
        assert!(!thread1_dir.exists(), "Thread 1 should be deleted");
        assert!(shared_repo.exists(), "Shared repo should be preserved (still referenced by thread 2)");
        assert!(shared_repo.join("test.txt").exists(), "Shared repo content should be intact");
    }

    #[tokio::test]
    async fn test_close_thread_both_symlinks_removes_shared_repo() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let shared_repo = workspace.join("repos").join("pr-42");
        std::fs::create_dir_all(&shared_repo).unwrap();
        std::fs::write(shared_repo.join("test.txt"), "shared content").unwrap();

        let thread1_dir = workspace.join("pr-42");
        std::fs::create_dir_all(&thread1_dir).unwrap();
        std::os::unix::fs::symlink(&shared_repo, thread1_dir.join("repo")).unwrap();

        let thread2_dir = workspace.join("review-pr-42");
        std::fs::create_dir_all(&thread2_dir).unwrap();
        std::os::unix::fs::symlink(&shared_repo, thread2_dir.join("repo")).unwrap();

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

        // Close thread 1
        thread_manager.close_thread("pr-42").await.unwrap();
        assert!(shared_repo.exists(), "Shared repo should be preserved after closing thread 1");

        // Close thread 2 — now no more references
        thread_manager.close_thread("review-pr-42").await.unwrap();
        assert!(!shared_repo.exists(), "Orphaned shared repo should be cleaned up when all references gone");
    }

    /// Verify that SessionStatus error events are correctly delivered through
    /// the event bus pub/sub mechanism. This tests the event bus primitive only
    /// — it does NOT exercise the spawn_worker error-publishing code path
    /// (which would require mocking AgentService to return an error).
    #[tokio::test]
    async fn test_event_bus_session_status_error_delivery() {
        use crate::core::thread_event::ThreadEvent;
        use crate::core::thread_event_bus::{SimpleThreadEventBus, ThreadEventBus};

        let event_bus = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx = event_bus.subscribe().await.unwrap();

        let event = ThreadEvent::SessionStatus {
            thread_name: "test_thread".to_string(),
            status_type: "error".to_string(),
            attempt: None,
            message: Some("processing failed".to_string()),
            timestamp: chrono::Utc::now(),
        };

        event_bus.publish(event.clone()).await.unwrap();

        let received = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            rx.recv(),
        ).await;

        match received {
            Ok(Some(ThreadEvent::SessionStatus { status_type, message, .. })) => {
                assert_eq!(status_type, "error");
                assert_eq!(message.as_deref(), Some("processing failed"));
            }
            Ok(other) => panic!("Expected SessionStatus error event, got: {:?}", other),
            Err(_) => panic!("Timed out waiting for error event"),
        }
    }
}