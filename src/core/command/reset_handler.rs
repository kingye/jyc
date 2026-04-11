use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// /reset command — reset opencode session for this thread.
///
/// Usage:
///   /reset    Delete the session state file; next AI prompt will start fresh
pub struct ResetCommandHandler;

#[async_trait]
impl CommandHandler for ResetCommandHandler {
    fn name(&self) -> &str {
        "/reset"
    }

    fn description(&self) -> &str {
        "Reset opencode session for this thread"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let session_path = context.thread_path.join(".jyc/opencode-session.json");

        if session_path.exists() {
            tokio::fs::remove_file(&session_path).await?;
            Ok(CommandResult {
                success: true,
                message: "/reset: session deleted. Next AI prompt will start with a fresh session.".into(),
                error: None,
                requires_restart: false,
            })
        } else {
            Ok(CommandResult {
                success: true,
                message: "/reset: no session exists. Next AI prompt will start with a fresh session.".into(),
                error: None,
                requires_restart: false,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn test_context(thread_path: &Path) -> CommandContext {
        CommandContext {
            args: vec![],
            thread_path: thread_path.to_path_buf(),
            config: Arc::new(
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
            ),
            channel: "test".into(),
            agent: None,
            template_dir: PathBuf::from("/tmp/test/templates"),
        }
    }

    #[tokio::test]
    async fn test_reset_existing_session() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("opencode-session.json"),
            r#"{"sessionId":"test-id","createdAt":"2026-01-01","lastUsedAt":"2026-01-01"}"#,
        )
        .await
        .unwrap();

        let handler = ResetCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!result.requires_restart);
        assert!(!jyc_dir.join("opencode-session.json").exists());
        assert!(result.message.contains("session deleted"));
    }

    #[tokio::test]
    async fn test_reset_no_existing_session() {
        let tmp = tempfile::tempdir().unwrap();

        let handler = ResetCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!result.requires_restart);
        assert!(result.message.contains("no session exists"));
    }
}
