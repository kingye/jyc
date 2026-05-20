use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// /reset command — reset agent session for this thread.
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
        "Reset agent session for this thread"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let agent_path = context.thread_path.join(".jyc/agent-session.json");
        let context_path = context.thread_path.join(".jyc/agent-context.json");

        let mut deleted = false;
        if agent_path.exists() {
            tokio::fs::remove_file(&agent_path).await?;
            deleted = true;
        }
        if context_path.exists() {
            tokio::fs::remove_file(&context_path).await?;
            deleted = true;
        }

        if deleted {
            Ok(CommandResult {
                success: true,
                message: "/reset: session deleted. Next AI prompt will start with a fresh session.".into(),
                error: None,
            })
        } else {
            Ok(CommandResult {
                success: true,
                message: "/reset: no session exists. Next AI prompt will start with a fresh session.".into(),
                error: None,
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
            template_dir: PathBuf::from("/tmp/test/templates"),
        }
    }

    #[tokio::test]
    async fn test_reset_existing_session() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01","total_input_tokens":100,"total_output_tokens":50,"max_input_tokens":1000}"#,
        )
        .await
        .unwrap();

        let handler = ResetCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!jyc_dir.join("agent-session.json").exists());
        assert!(result.message.contains("session deleted"));
    }

    #[tokio::test]
    async fn test_reset_no_existing_session() {
        let tmp = tempfile::tempdir().unwrap();

        let handler = ResetCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("no session exists"));
    }
}
