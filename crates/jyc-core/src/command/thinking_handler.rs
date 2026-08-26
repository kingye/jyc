use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// `/thinking` command - control whether AI thinking/reasoning content is
/// sent to the dashboard.
///
/// Usage:
/// - `/thinking show` - enable thinking content display (default)
/// - `/thinking hide` - suppress thinking content; dashboard shows only "Thinking..."
///
/// State is persisted in `.jyc/thinking-state` and read by the agent loop
/// before each processing cycle.
pub struct ThinkingCommandHandler;

#[async_trait]
impl CommandHandler for ThinkingCommandHandler {
    fn name(&self) -> &str {
        "/thinking"
    }

    fn description(&self) -> &str {
        "Show or hide AI thinking/reasoning content"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let arg = context
            .args
            .first()
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        let enabled = match arg.as_str() {
            "show" => true,
            "hide" => false,
            "" => true, // default to show when no arg
            other => {
                return Ok(CommandResult {
                    success: false,
                    message: format!(
                        "/thinking: unknown argument '{other}'. Use '/thinking show' or '/thinking hide'"
                    ),
                    error: None,
                    append_body: None,
                });
            }
        };

        let jyc_dir = context.topic_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await?;

        let state_path = jyc_dir.join("thinking-state");
        let content = if enabled { "show" } else { "hide" };
        tokio::fs::write(&state_path, content).await?;

        let message = if enabled {
            "/thinking: thinking content will be shown"
        } else {
            "/thinking: thinking content hidden, only \"Thinking...\" will be displayed"
        };

        Ok(CommandResult {
            success: true,
            message: message.into(),
            error: None,
            append_body: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

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
            template_dirs: PathBuf::from("/tmp/test/templates").into(),
            channel_type: "websocket".to_string(),
            config_path: None,
            topic: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn test_thinking_hide() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ThinkingCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["hide".to_string()];

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/thinking-state"))
            .await
            .unwrap();
        assert_eq!(content, "hide");
    }

    #[tokio::test]
    async fn test_thinking_show() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ThinkingCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["show".to_string()];

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/thinking-state"))
            .await
            .unwrap();
        assert_eq!(content, "show");
    }

    #[tokio::test]
    async fn test_thinking_default_is_show() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ThinkingCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/thinking-state"))
            .await
            .unwrap();
        assert_eq!(content, "show");
    }

    #[tokio::test]
    async fn test_thinking_invalid_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ThinkingCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["banana".to_string()];

        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("unknown argument"));
    }
}
