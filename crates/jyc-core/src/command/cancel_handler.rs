use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::thread_manager::ThreadManager;

/// /cancel command — cancel the current AI processing for this thread.
///
/// Triggers the per-thread cancellation token, causing the agent loop to
/// break at the next iteration check. The thread directory and queue are
/// preserved.
pub struct CancelCommandHandler {
    thread_manager: Arc<ThreadManager>,
}

impl CancelCommandHandler {
    pub fn new(thread_manager: Arc<ThreadManager>) -> Self {
        Self { thread_manager }
    }
}

#[async_trait]
impl CommandHandler for CancelCommandHandler {
    fn name(&self) -> &str {
        "/cancel"
    }

    fn description(&self) -> &str {
        "Cancel the current AI processing for this thread"
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
                message: format!(
                    "Failed to determine thread name from path: {:?}",
                    context.thread_path
                ),
                error: Some("Thread directory name could not be extracted".into()),
            });
        }

        self.thread_manager.cancel_thread(thread_name).await;

        Ok(CommandResult {
            success: true,
            message: format!("AI processing cancelled for thread '{}'.", thread_name),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_storage::MessageStorage;
    use crate::metrics::MetricsCollector;
    use crate::static_agent::StaticAgentService;
    use crate::thread_manager::ThreadManager;
    use arc_swap::ArcSwap;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn make_thread_manager(workspace: &std::path::Path) -> Arc<ThreadManager> {
        let storage = Arc::new(MessageStorage::new(workspace));
        let cancel = CancellationToken::new();
        let metrics_cancel = CancellationToken::new();
        let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();
        let config = Arc::new(ArcSwap::from_pointee(
            jyc_types::load_config_from_str(
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
mode = "agent"
"#,
            )
            .unwrap(),
        ));

        Arc::new(ThreadManager::new_with_options(
            1,
            10,
            storage,
            // OutboundAdapter not needed for cancel tests — cancel_thread only
            // touches thread_cancels. We use a panic-on-send stub via
            // StaticAgentService which is sufficient.
            Arc::new(NoopOutbound),
            Arc::new(StaticAgentService::new("ok")),
            cancel,
            false,
            workspace.join("templates"),
            config,
            "test".to_string(),
            "websocket".to_string(),
            workspace.parent().unwrap_or(workspace).to_path_buf(),
            workspace.to_path_buf(),
            metrics,
        ))
    }

    /// Minimal outbound adapter that does nothing — sufficient for cancel
    /// tests which never send replies.
    struct NoopOutbound;

    #[async_trait::async_trait]
    impl jyc_types::OutboundAdapter for NoopOutbound {
        fn channel_type(&self) -> &str {
            "test"
        }
        async fn connect(&self) -> Result<()> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<()> {
            Ok(())
        }
        fn clean_body(&self, raw_body: &str) -> String {
            raw_body.to_string()
        }
        async fn send_reply(
            &self,
            _original: &jyc_types::InboundMessage,
            _reply_text: &str,
            _thread_path: &std::path::Path,
            _message_dir: &str,
            _attachments: Option<&[jyc_types::OutboundAttachment]>,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "noop".to_string(),
            })
        }
        async fn send_message(
            &self,
            _recipient: &str,
            _subject: &str,
            _body: &str,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "noop".to_string(),
            })
        }
    }

    fn test_context(thread_path: &std::path::Path) -> CommandContext {
        CommandContext {
            args: vec![],
            thread_path: thread_path.to_path_buf(),
            config: Arc::new(
                jyc_types::load_config_from_str(
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
mode = "agent"
"#,
                )
                .unwrap(),
            ),
            channel: "test".into(),
            agent: None,
            template_dirs: std::path::PathBuf::from("/tmp/test/templates").into(),
        }
    }

    #[tokio::test]
    async fn test_cancel_no_active_token_is_noop() {
        let tmp = tempdir().unwrap();
        let tm = make_thread_manager(tmp.path());

        // No thread is processing — cancel_thread should not panic
        tm.cancel_thread("nonexistent").await;
    }

    #[tokio::test]
    async fn test_cancel_triggers_token() {
        let tmp = tempdir().unwrap();
        let tm = make_thread_manager(tmp.path());

        // Manually insert a cancellation token (simulating an active worker)
        let token = CancellationToken::new();
        {
            let mut cancels = tm.thread_cancels.lock().await;
            cancels.insert("my-thread".to_string(), token.clone());
        }

        assert!(!token.is_cancelled());
        tm.cancel_thread("my-thread").await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancel_handler_empty_thread_name() {
        let tmp = tempdir().unwrap();
        let tm = make_thread_manager(tmp.path());
        let handler = CancelCommandHandler::new(tm);

        // Use root path "/" which has no file name
        let ctx = test_context(std::path::Path::new("/"));
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_cancel_handler_success() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path();
        let thread_dir = workspace.join("my-thread");
        tokio::fs::create_dir_all(&thread_dir).await.unwrap();

        let tm = make_thread_manager(workspace);

        // Insert a token to simulate active processing
        let token = CancellationToken::new();
        {
            let mut cancels = tm.thread_cancels.lock().await;
            cancels.insert("my-thread".to_string(), token.clone());
        }

        let handler = CancelCommandHandler::new(tm.clone());
        let ctx = test_context(&thread_dir);
        let result = handler.execute(ctx).await.unwrap();

        assert!(result.success);
        assert!(result.message.contains("my-thread"));
        assert!(token.is_cancelled());
    }
}
