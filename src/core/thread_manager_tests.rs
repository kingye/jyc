#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;
    use crate::services::static_agent::StaticAgentService;

    fn test_config() -> Arc<crate::config::types::AppConfig> {
        Arc::new(
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
        )
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
        );

        let result = thread_manager.close_thread("nonexistent_thread").await;
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
        );

        let result = thread_manager.close_thread("test_thread").await;
        assert!(result.is_ok());
        assert!(!thread_dir.exists());
    }
}