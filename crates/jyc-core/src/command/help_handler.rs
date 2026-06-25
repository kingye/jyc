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

    async fn execute(&self, _context: CommandContext) -> Result<CommandResult> {
        let help = "\
Available commands:\n\
  /model <name>  — switch AI model\n\
  /plan   — switch to plan mode (read-only)\n\
  /build  — switch to build mode (full execution)\n\
  /reset  — reset session, keep chat history\n\
  /new    — reset session and clear chat history\n\
  /close  — close and delete this thread\n\
  /template [update] — apply or re-apply thread template\n\
  /?      — show this help";

        Ok(CommandResult {
            success: true,
            message: help.into(),
            error: None,
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
            template_dir: PathBuf::from("/tmp/test/templates"),
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
}
