use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::topic_manager::TopicManager;

/// /close command — close and delete topic directory.
pub struct CloseCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl CloseCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }

    /// Returns `true` if the args contain the explicit `--force` flag.
    ///
    /// Plain `/close` (no args) returns `false` so the handler can emit a
    /// warning instead of deleting (mirrors `/new`).
    fn is_forced(args: &[String]) -> bool {
        args.iter().any(|a| a == "--force")
    }
}

#[async_trait]
impl CommandHandler for CloseCommandHandler {
    fn name(&self) -> &str {
        "/close"
    }

    fn description(&self) -> &str {
        "Close and delete this topic (requires --force)"
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

        // Require --force to prevent accidental topic deletion. Plain
        // `/close` returns a warning instead of the destructive action.
        if !Self::is_forced(&context.args) {
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "⚠️  /close will PERMANENTLY delete topic '{topic_name}' and all its data \
                     (chat history, AI session, attachments). This cannot be undone.\n\
                     \n\
                     To proceed, send: /close --force"
                ),
                error: None,
                append_body: None,
            });
        }

        match self.topic_manager.close_topic(topic_name).await {
            Ok(()) => {
                tracing::info!(topic = %topic_name, "Topic closed successfully via /close command");
                Ok(CommandResult {
                    success: true,
                    message: format!("Topic '{}' closed and directory deleted.", topic_name),
                    error: None,
                    append_body: None,
                })
            }
            Err(e) => Ok(CommandResult {
                success: false,
                message: format!("Failed to close topic '{}'", topic_name),
                error: Some(e.context("close_topic failed").to_string()),
                append_body: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn is_forced_accepts_force_flag() {
        assert!(CloseCommandHandler::is_forced(&args(&["--force"])));
    }

    #[test]
    fn is_forced_rejects_old_confirm_tokens() {
        // `-y`/`--confirm` no longer bypass the guard — only `--force`.
        assert!(!CloseCommandHandler::is_forced(&args(&["-y"])));
        assert!(!CloseCommandHandler::is_forced(&args(&["--confirm"])));
        assert!(!CloseCommandHandler::is_forced(&args(&["--yes"])));
        assert!(!CloseCommandHandler::is_forced(&args(&["--foo"])));
    }

    #[test]
    fn is_forced_rejects_empty_args() {
        assert!(!CloseCommandHandler::is_forced(&args(&[])));
    }

    #[test]
    fn is_forced_accepts_flag_mixed_with_unknown_args() {
        assert!(CloseCommandHandler::is_forced(&args(&["--foo", "--force"])));
    }
}
