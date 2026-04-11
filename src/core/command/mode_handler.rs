use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// /plan command — switch to plan mode (read-only, enforced by OpenCode).
pub struct PlanCommandHandler;

#[async_trait]
impl CommandHandler for PlanCommandHandler {
    fn name(&self) -> &str {
        "/plan"
    }

    fn description(&self) -> &str {
        "Switch to plan mode (read-only)"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let jyc_dir = context.thread_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await?;

        let override_path = jyc_dir.join("mode-override");
        tokio::fs::write(&override_path, "plan").await?;

        // Mode is passed per-prompt (PromptRequest.agent), not per-session.
        // Session is preserved — AI keeps conversation memory.

        Ok(CommandResult {
            success: true,
            message: "/plan: switched to plan mode (read-only)".into(),
            error: None,
            requires_restart: false,
        })
    }
}

/// /build command — switch to build mode (full execution, default).
pub struct BuildCommandHandler;

#[async_trait]
impl CommandHandler for BuildCommandHandler {
    fn name(&self) -> &str {
        "/build"
    }

    fn description(&self) -> &str {
        "Switch to build mode (full execution)"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let jyc_dir = context.thread_path.join(".jyc");
        let override_path = jyc_dir.join("mode-override");

        if override_path.exists() {
            tokio::fs::remove_file(&override_path).await?;
        }

        // Mode is passed per-prompt (PromptRequest.agent), not per-session.
        // Session is preserved — AI keeps conversation memory.

        Ok(CommandResult {
            success: true,
            message: "/build: switched to build mode (full execution)".into(),
            error: None,
            requires_restart: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
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
    async fn test_plan_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = PlanCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!result.requires_restart);

        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/mode-override"))
            .await
            .unwrap();
        assert_eq!(content, "plan");
    }

    #[tokio::test]
    async fn test_build_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("mode-override"), "plan")
            .await
            .unwrap();

        let handler = BuildCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!jyc_dir.join("mode-override").exists());
    }

    #[tokio::test]
    async fn test_plan_preserves_session() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("opencode-session.json"), r#"{"sessionId":"test"}"#)
            .await
            .unwrap();

        let handler = PlanCommandHandler;
        let ctx = test_context(tmp.path());
        handler.execute(ctx).await.unwrap();

        // Session file should still exist
        assert!(jyc_dir.join("opencode-session.json").exists());
    }
}
