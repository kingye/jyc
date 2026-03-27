use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// /model command — switch AI model for this thread.
///
/// Usage:
///   /model <model-id>    Switch to a specific model
///   /model reset          Reset to default model from config
///   /model                List available (not yet implemented)
pub struct ModelCommandHandler;

#[async_trait]
impl CommandHandler for ModelCommandHandler {
    fn name(&self) -> &str {
        "/model"
    }

    fn description(&self) -> &str {
        "Switch AI model for this thread"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let jyc_dir = context.thread_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await?;

        let override_path = jyc_dir.join("model-override");
        let session_path = jyc_dir.join("opencode-session.json");

        if context.args.is_empty() {
            // /model with no args — list models (not yet implemented)
            return Ok(CommandResult {
                success: true,
                message: "/model: listing models not yet implemented. Use /model <model-id> to switch.".into(),
                error: None,
                requires_restart: false,
            });
        }

        let arg = context.args.join(" ");

        if arg.to_lowercase() == "reset" {
            // /model reset — remove override, revert to config default
            if override_path.exists() {
                tokio::fs::remove_file(&override_path).await?;
            }
            // Delete session to force new one with default model
            if session_path.exists() {
                tokio::fs::remove_file(&session_path).await?;
            }

            return Ok(CommandResult {
                success: true,
                message: "/model: reset to default model from config".into(),
                error: None,
                requires_restart: true,
            });
        }

        // /model <model-id> — write override
        tokio::fs::write(&override_path, arg.trim()).await?;

        // Delete session to force new one with new model
        if session_path.exists() {
            tokio::fs::remove_file(&session_path).await?;
        }

        Ok(CommandResult {
            success: true,
            message: format!("/model: switched to {}", arg.trim()),
            error: None,
            requires_restart: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
    }

    #[tokio::test]
    async fn test_model_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ModelCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["SomeProvider/SomeModel".into()];

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.requires_restart);

        let override_content =
            tokio::fs::read_to_string(tmp.path().join(".jyc/model-override"))
                .await
                .unwrap();
        assert_eq!(override_content, "SomeProvider/SomeModel");
    }

    #[tokio::test]
    async fn test_model_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("model-override"), "old-model")
            .await
            .unwrap();

        let handler = ModelCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["reset".into()];

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!jyc_dir.join("model-override").exists());
    }
}
