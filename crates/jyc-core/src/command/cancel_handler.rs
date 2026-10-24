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
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // Identity is the topic name, not the workspace dir — a shared child
        // works in its parent's dir, so the path would name the parent.
        let topic_name = context.topic_name.as_str();

        if topic_name.is_empty() {
            return Ok(CommandResult {
                success: false,
                message: "Failed to determine topic name (topic_name is empty)".to_string(),
                error: Some("Topic name is empty".into()),
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
    use crate::topic_manager::TopicManager;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn make_topic_manager(workspace: &std::path::Path) -> Arc<TopicManager> {
        crate::test_support::make_topic_manager(workspace, crate::test_support::TEST_CONFIG_STR)
    }

    fn test_context(topic_path: &std::path::Path) -> CommandContext {
        crate::test_support::test_context("test-topic", topic_path)
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

        let mut ctx = test_context(std::path::Path::new("/"));
        ctx.topic_name = String::new();
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

        // Insert a token to simulate active processing, keyed by the topic
        // name — resolution must not come from the workspace dir name.
        let token = CancellationToken::new();
        {
            let mut cancels = tm.topic_cancels.lock().await;
            cancels.insert("test-topic".to_string(), token.clone());
        }

        let handler = CancelCommandHandler::new(tm.clone());
        let ctx = test_context(&topic_dir);
        let result = handler.execute(ctx).await.unwrap();

        assert!(result.success);
        assert!(result.message.contains("test-topic"));
        assert!(token.is_cancelled());
    }
}
