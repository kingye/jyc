use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::topic_manager::TopicManager;

/// /cancel command — cancel the current AI processing for this topic.
///
/// Triggers the per-topic cancellation token, causing the agent loop to
/// break at the next iteration check. The topic directory and queue are
/// preserved.
pub struct CancelCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl CancelCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }
}

#[async_trait]
impl CommandHandler for CancelCommandHandler {
    fn name(&self) -> &str {
        "/cancel"
    }

    fn description(&self) -> &str {
        "Cancel the current AI processing for this topic"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let topic_name = context
            .topic_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        if topic_name.is_empty() {
            return Ok(CommandResult {
                success: false,
                message: format!(
                    "Failed to determine topic name from path: {:?}",
                    context.topic_path
                ),
                error: Some("Topic directory name could not be extracted".into()),
                append_body: None,
            });
        }

        let cancelled = self.topic_manager.cancel_topic(topic_name).await;

        if cancelled {
            Ok(CommandResult {
                success: true,
                message: format!("AI processing cancelled for topic '{}'.", topic_name),
                error: None,
                append_body: None,
            })
        } else {
            Ok(CommandResult {
                success: false,
                message: format!(
                    "No active AI processing for topic '{}' (nothing to cancel).",
                    topic_name
                ),
                error: None,
                append_body: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_storage::MessageStorage;
    use crate::metrics::MetricsCollector;
    use crate::static_agent::StaticAgentService;
    use crate::topic_manager::TopicManager;
    use arc_swap::ArcSwap;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn make_topic_manager(workspace: &std::path::Path) -> Arc<TopicManager> {
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

        Arc::new(TopicManager::new_with_options(
            1,
            10,
            storage,
            // OutboundAdapter not needed for cancel tests — cancel_topic only
            // touches topic_cancels. We use a panic-on-send stub via
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
            None,
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
            _topic_path: &std::path::Path,
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

    fn test_context(topic_path: &std::path::Path) -> CommandContext {
        CommandContext {
            args: vec![],
            topic_path: topic_path.to_path_buf(),
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
            channel_type: "websocket".to_string(),
            config_path: None,
            topic: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn test_cancel_no_active_token_is_noop() {
        let tmp = tempdir().unwrap();
        let tm = make_topic_manager(tmp.path());

        // No topic is processing — cancel_topic should not panic
        tm.cancel_topic("nonexistent").await;
    }

    #[tokio::test]
    async fn test_cancel_triggers_token() {
        let tmp = tempdir().unwrap();
        let tm = make_topic_manager(tmp.path());

        // Manually insert a cancellation token (simulating an active worker)
        let token = CancellationToken::new();
        {
            let mut cancels = tm.topic_cancels.lock().await;
            cancels.insert("my-topic".to_string(), token.clone());
        }

        assert!(!token.is_cancelled());
        tm.cancel_topic("my-topic").await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancel_handler_empty_topic_name() {
        let tmp = tempdir().unwrap();
        let tm = make_topic_manager(tmp.path());
        let handler = CancelCommandHandler::new(tm);

        // Use root path "/" which has no file name
        let ctx = test_context(std::path::Path::new("/"));
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_cancel_handler_no_active_processing_reports_failure() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path();
        let topic_dir = workspace.join("idle-topic");
        tokio::fs::create_dir_all(&topic_dir).await.unwrap();

        let tm = make_topic_manager(workspace);
        let handler = CancelCommandHandler::new(tm);
        let ctx = test_context(&topic_dir);
        let result = handler.execute(ctx).await.unwrap();

        // Must NOT claim success when nothing was actually cancelled
        assert!(!result.success);
        assert!(result.message.contains("No active AI processing"));
    }

    #[tokio::test]
    async fn test_cancel_handler_success() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path();
        let topic_dir = workspace.join("my-topic");
        tokio::fs::create_dir_all(&topic_dir).await.unwrap();

        let tm = make_topic_manager(workspace);

        // Insert a token to simulate active processing
        let token = CancellationToken::new();
        {
            let mut cancels = tm.topic_cancels.lock().await;
            cancels.insert("my-topic".to_string(), token.clone());
        }

        let handler = CancelCommandHandler::new(tm.clone());
        let ctx = test_context(&topic_dir);
        let result = handler.execute(ctx).await.unwrap();

        assert!(result.success);
        assert!(result.message.contains("my-topic"));
        assert!(token.is_cancelled());
    }
}
