use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::topic_manager::{PurgeOutcome, TopicManager};

/// /close command — close a topic, and with `--purge` delete its directory too.
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

    /// Returns `true` if the args ask for the topic directory to go as well.
    ///
    /// Only meaningful together with `--force`: `/close` alone never deletes
    /// anything, whatever else it is given.
    fn is_purged(args: &[String]) -> bool {
        args.iter().any(|a| a == "--purge")
    }
}

#[async_trait]
impl CommandHandler for CloseCommandHandler {
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // The topic identity is the name, not the workspace dir: a forked
        // topic shares its parent's dir, so deriving the name from the path
        // would close/purge the parent instead of the fork.
        let topic_name = context.topic_name.as_str();

        if topic_name.is_empty() {
            return Ok(CommandResult {
                success: false,
                message: "Failed to determine topic name (topic_name is empty)".to_string(),
                error: Some("Topic name is empty".into()),
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

        // Purge the directory only when it is this topic's to delete. A
        // refusal does not block the close: the topic's state goes, only
        // the directory is kept (reported in the final message).
        let mut purge_refused: Option<String> = None;
        if Self::is_purged(&context.args) {
            match self.topic_manager.purge_topic_dir(topic_name).await? {
                PurgeOutcome::Refused(reason) => {
                    tracing::info!(topic = %topic_name, reason = %reason, "Purge refused");
                    purge_refused = Some(reason);
                }
                PurgeOutcome::Deleted => {
                    tracing::info!(topic = %topic_name, dir = %context.topic_path.display(), "Purged topic dir");
                }
                PurgeOutcome::Nothing => {}
            }
        }

        // Notification-only; fires before deletion so a hook can archive
        // the topic directory while it still exists.
        context.session_end_hook("close").await;

        match self.topic_manager.close_topic(topic_name).await {
            Ok(()) => {
                tracing::info!(topic = %topic_name, "Topic closed successfully via /close command");
                // `close_topic` deletes a dir that held nothing but its own
                // state, and keeps one that belongs to somebody — a `/fork`
                // sibling, a `/spawn` working in a directory somebody chose,
                // or a spawn dir that has since gained files. Ask the disk
                // rather than restating that branch here.
                let kept = tokio::fs::try_exists(&context.topic_path)
                    .await
                    .unwrap_or(false);
                let message = match purge_refused {
                    Some(reason) => format!(
                        "Topic '{topic_name}' closed; its state is deleted and {} is kept: {reason}",
                        context.topic_path.display()
                    ),
                    None if kept => format!(
                        "Topic '{topic_name}' closed; its state is deleted and {} is kept. \
                         Add --purge to delete the directory as well:\n/close --force --purge",
                        context.topic_path.display()
                    ),
                    None => {
                        format!("Topic '{topic_name}' closed; its directory and data are deleted.")
                    }
                };
                Ok(CommandResult {
                    success: true,
                    message,
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

    #[test]
    fn is_purged_only_answers_for_its_own_flag() {
        assert!(CloseCommandHandler::is_purged(&args(&[
            "--force", "--purge"
        ])));
        assert!(CloseCommandHandler::is_purged(&args(&["--purge"])));
        assert!(!CloseCommandHandler::is_purged(&args(&["--force"])));
        assert!(!CloseCommandHandler::is_purged(&args(&[])));
    }
}
