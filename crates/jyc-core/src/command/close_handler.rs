use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::thread_manager::ThreadManager;

/// /close command — close and delete thread directory.
pub struct CloseCommandHandler {
    thread_manager: Arc<ThreadManager>,
}

impl CloseCommandHandler {
    pub fn new(thread_manager: Arc<ThreadManager>) -> Self {
        Self { thread_manager }
    }

    /// Returns `true` if the args contain an explicit confirmation flag.
    ///
    /// Accepted tokens: `--confirm`, `-y`. Plain `/close` (no args) returns
    /// `false` so the handler can emit a warning instead of deleting.
    fn is_confirmed(args: &[String]) -> bool {
        args.iter().any(|a| a == "--confirm" || a == "-y")
    }
}

#[async_trait]
impl CommandHandler for CloseCommandHandler {
    fn name(&self) -> &str {
        "/close"
    }

    fn description(&self) -> &str {
        "Close and delete this thread (requires --confirm or -y)"
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
                append_body: None,
            });
        }

        // Require explicit confirmation to prevent accidental thread deletion.
        // Accept `--confirm` or `-y`. Plain `/close` returns a warning instead
        // of performing the destructive action.
        if !Self::is_confirmed(&context.args) {
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "⚠️  /close will PERMANENTLY delete thread '{thread_name}' and all its data \
                     (chat history, AI session, attachments). This cannot be undone.\n\
                     \n\
                     To proceed, send: /close -y  (or /close --confirm)"
                ),
                error: None,
                append_body: None,
            });
        }

        match self.thread_manager.close_thread(thread_name).await {
            Ok(()) => {
                tracing::info!(thread = %thread_name, "Thread closed successfully via /close command");
                Ok(CommandResult {
                    success: true,
                    message: format!("Thread '{}' closed and directory deleted.", thread_name),
                    error: None,
                    append_body: None,
                })
            }
            Err(e) => Ok(CommandResult {
                success: false,
                message: format!("Failed to close thread '{}'", thread_name),
                error: Some(e.context("close_thread failed").to_string()),
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
    fn is_confirmed_accepts_long_flag() {
        assert!(CloseCommandHandler::is_confirmed(&args(&["--confirm"])));
    }

    #[test]
    fn is_confirmed_accepts_short_flag() {
        assert!(CloseCommandHandler::is_confirmed(&args(&["-y"])));
    }

    #[test]
    fn is_confirmed_rejects_empty_args() {
        assert!(!CloseCommandHandler::is_confirmed(&args(&[])));
    }

    #[test]
    fn is_confirmed_rejects_unknown_flag() {
        assert!(!CloseCommandHandler::is_confirmed(&args(&["--yes"])));
        assert!(!CloseCommandHandler::is_confirmed(&args(&["--foo"])));
    }

    #[test]
    fn is_confirmed_accepts_flag_mixed_with_unknown_args() {
        // "-y" present wins, even mixed with unknowns
        assert!(CloseCommandHandler::is_confirmed(&args(&["--foo", "-y"])));
        assert!(CloseCommandHandler::is_confirmed(&args(&[
            "--confirm",
            "extra-junk"
        ])));
    }
}
