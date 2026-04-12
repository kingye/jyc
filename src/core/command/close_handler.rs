use anyhow::Result;
#[allow(unused_imports)]
use anyhow::Context;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::core::thread_manager::ThreadManager;
use crate::services::static_agent::StaticAgentService;

/// /close command — close and delete thread directory.
pub struct CloseCommandHandler {
    thread_manager: Arc<ThreadManager>,
}

impl CloseCommandHandler {
    pub fn new(thread_manager: Arc<ThreadManager>) -> Self {
        Self { thread_manager }
    }
}

#[async_trait]
impl CommandHandler for CloseCommandHandler {
    fn name(&self) -> &str {
        "/close"
    }

    fn description(&self) -> &str {
        "Close thread and delete its directory"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let thread_name = context
            .thread_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        if thread_name.is_empty() {
            return Ok(CommandResult {
                success: false,
                message: "Failed to determine thread name".into(),
                error: Some("Thread path is invalid".into()),
                requires_restart: false,
            });
        }

        match self.thread_manager.close_thread(thread_name).await {
            Ok(()) => Ok(CommandResult {
                success: true,
                message: format!(
                    "Thread '{}' closed and directory deleted.",
                    thread_name
                ),
                error: None,
                requires_restart: false,
            }),
            Err(e) => Ok(CommandResult {
                success: false,
                message: format!("Failed to close thread '{}'", thread_name),
                error: Some(e.context("close_thread failed").to_string()),
                requires_restart: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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

    fn test_context(thread_path: &std::path::Path) -> CommandContext {
        CommandContext {
            args: vec![],
            thread_path: thread_path.to_path_buf(),
            config: test_config(),
            channel: "test".into(),
            agent: None,
            template_dir: PathBuf::from("/tmp/test/templates"),
        }
    }

    #[tokio::test]
    async fn test_close_command_deletes_thread_directory() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let thread_dir = workspace.join("test_thread");
        std::fs::create_dir_all(&thread_dir).unwrap();
        std::fs::write(thread_dir.join("test.txt"), "content").unwrap();

        let storage = Arc::new(crate::core::message_storage::MessageStorage::new(&workspace));
        
        let thread_manager = Arc::new(ThreadManager::new(
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
        ));

        let handler = CloseCommandHandler::new(thread_manager);
        let ctx = test_context(&thread_dir);

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!thread_dir.exists());
        assert!(result.message.contains("test_thread"));
    }

    #[tokio::test]
    async fn test_close_command_nonexistent_thread_succeeds() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let thread_dir = workspace.join("nonexistent_thread");

        let storage = Arc::new(crate::core::message_storage::MessageStorage::new(&workspace));
        
        let thread_manager = Arc::new(ThreadManager::new(
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
        ));

        let handler = CloseCommandHandler::new(thread_manager);
        let ctx = test_context(&thread_dir);

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_close_command_invalid_thread_path() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        
        let storage = Arc::new(crate::core::message_storage::MessageStorage::new(&workspace));
        
        let thread_manager = Arc::new(ThreadManager::new(
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
        ));

        let handler = CloseCommandHandler::new(thread_manager);
        
        // Test with root path which has no parent file_name
        let ctx = CommandContext {
            args: vec![],
            thread_path: PathBuf::from("/"),
            config: test_config(),
            channel: "test".into(),
            agent: None,
            template_dir: PathBuf::from("/tmp/test/templates"),
        };

        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }
}