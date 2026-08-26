use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::gh_watcher::render_snapshot_lines;
use crate::topic_manager::TopicManager;

/// `/gh` command — toggle the per-topic GitHub status watcher.
///
/// Usage: `/gh on` starts the watcher and replies with the first snapshot.
///        `/gh off` stops the watcher.
pub struct GhCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl GhCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }
}

#[async_trait]
impl CommandHandler for GhCommandHandler {
    fn name(&self) -> &str {
        "/gh"
    }

    fn description(&self) -> &str {
        "Toggle the per-topic GitHub status watcher (args: on|off)"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        match context.args.first().map(|s| s.as_str()) {
            Some("on") => match self.topic_manager.start_gh_watcher(&context.topic).await {
                Ok(snapshot) => {
                    let lines = render_snapshot_lines(&snapshot);
                    let message = format!("GitHub status watcher started.\n{}", lines.join("\n"));
                    Ok(CommandResult {
                        success: true,
                        message,
                        error: None,
                        append_body: None,
                    })
                }
                Err(e) => Ok(CommandResult {
                    success: false,
                    message: format!("Failed to start GitHub status watcher: {}", e),
                    error: Some(e.to_string()),
                    append_body: None,
                }),
            },
            Some("off") => match self.topic_manager.stop_gh_watcher(&context.topic).await {
                Ok(()) => Ok(CommandResult {
                    success: true,
                    message: "GitHub status watcher stopped.".to_string(),
                    error: None,
                    append_body: None,
                }),
                Err(e) => Ok(CommandResult {
                    success: false,
                    message: format!("Failed to stop GitHub status watcher: {}", e),
                    error: Some(e.to_string()),
                    append_body: None,
                }),
            },
            _ => Ok(CommandResult {
                success: false,
                message: "Usage: /gh on|off".to_string(),
                error: Some("invalid arguments".to_string()),
                append_body: None,
            }),
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

    fn test_context(topic_dir: &std::path::Path) -> CommandContext {
        CommandContext {
            args: vec!["on".to_string()],
            topic_path: topic_dir.to_path_buf(),
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
            topic: "my-topic".to_string(),
        }
    }

    #[tokio::test]
    async fn test_gh_handler_usage_error() {
        let tmp = tempdir().unwrap();
        let tm = make_topic_manager(tmp.path());
        let handler = GhCommandHandler::new(tm);

        let mut ctx = test_context(tmp.path());
        ctx.args = vec![];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("Usage"));
    }

    #[tokio::test]
    async fn test_gh_handler_on_off_lifecycle() {
        let tmp = tempdir().unwrap();
        let tm = make_topic_manager(tmp.path());
        let topic_dir = tmp.path().join("my-topic");
        tokio::fs::create_dir_all(&topic_dir).await.unwrap();

        let handler = GhCommandHandler::new(tm.clone());

        // /gh on fails because gh is not installed, but it still starts the
        // watcher and writes the enabled flag.
        let ctx = test_context(&topic_dir);
        let result = handler.execute(ctx).await.unwrap();
        assert!(tm.gh_watcher_enabled_on_disk("my-topic").await);

        // /gh off stops it.
        let mut ctx = test_context(&topic_dir);
        ctx.args = vec!["off".to_string()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!tm.gh_watcher_enabled_on_disk("my-topic").await);
    }
}
