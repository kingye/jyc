use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// /? command — show available commands and their descriptions.
///
/// Usage:
///   /?    List all available commands with brief descriptions
pub struct HelpCommandHandler;

#[async_trait]
impl CommandHandler for HelpCommandHandler {
    fn name(&self) -> &str {
        "/?"
    }

    fn description(&self) -> &str {
        "Show available commands"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // Generated from all_commands_with() so built-ins and user-defined
        // `[[commands]]` stay in sync with the dashboard command popup.
        let commands = super::all_commands_with(&context.config.commands);
        let width = commands.iter().map(|c| c.name.len()).max().unwrap_or(0);

        let mut help = String::from("Available commands:\n");
        for cmd in &commands {
            help.push_str(&format!(
                "  {:width$}  — {}\n",
                cmd.name,
                cmd.description,
                width = width
            ));
        }

        Ok(CommandResult {
            success: true,
            message: help.trim_end().to_string(),
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

    #[tokio::test]
    async fn test_help_contains_commands() {
        let ctx = CommandContext {
            args: vec![],
            thread_path: PathBuf::from("/tmp/test"),
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
        };

        let handler = HelpCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        // Verify key commands are listed
        for cmd in &[
            "/model",
            "/plan",
            "/build",
            "/reset",
            "/new",
            "/close",
            "/template",
            "/?",
        ] {
            assert!(result.message.contains(cmd), "help should mention {cmd}");
        }
    }

    #[tokio::test]
    async fn test_help_lists_custom_commands() {
        let config = jyc_types::load_config_from_str(
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

[[commands]]
name = "review"
description = "Review the PR"
user_prompt = "Review it."
"#,
        )
        .unwrap();

        let ctx = CommandContext {
            args: vec![],
            thread_path: PathBuf::from("/tmp/test"),
            config: Arc::new(config),
            channel: "test".into(),
            agent: None,
            template_dirs: PathBuf::from("/tmp/test/templates").into(),
            channel_type: "websocket".to_string(),
            config_path: None,
        };

        let result = HelpCommandHandler.execute(ctx).await.unwrap();
        assert!(result.message.contains("/review"));
        assert!(result.message.contains("Review the PR"));
        // built-ins are still listed
        assert!(result.message.contains("/plan"));
    }
}
