use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// /reset command — reset agent session for this thread.
///
/// Usage:
///   /reset    Reset agent session with configurable compression
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
        // Resolve ResetCompressionConfig:
        // 1. Try to find a matched pattern from the channel config
        // 2. Fallback to global AgentConfig.reset_compression
        // 3. Default if neither is set
        let channel_config = context.config.channels.get(&context.channel);
        let pattern_reset = channel_config
            .and_then(|c| c.patterns.as_ref())
            .and_then(|patterns| {
                // Use the first pattern's reset_compression as fallback
                // (pattern matching is done at router level, not available here)
                patterns.first().and_then(|p| p.reset_compression.clone())
            });
        let global_reset = context.config.agent.reset_compression.clone();
        let reset_config = pattern_reset.or(global_reset).unwrap_or_default();

        if let Some(ref agent) = context.agent {
            let thread_name = context
                .thread_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            agent
                .reset_session(&context.thread_path, thread_name, &reset_config)
                .await?;
            Ok(CommandResult {
                success: true,
                message: "/reset: session reset successfully".into(),
                error: None,
            })
        } else {
            // No agent service available — fallback to direct file deletion
            let jyc_dir = context.thread_path.join(".jyc");
            tokio::fs::remove_file(jyc_dir.join("agent-session.json"))
                .await
                .ok();
            tokio::fs::remove_file(jyc_dir.join("agent-context.json"))
                .await
                .ok();
            Ok(CommandResult {
                success: true,
                message: "/reset: session deleted (no agent service)".into(),
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
            template_dirs: PathBuf::from("/tmp/test/templates").into(),
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
        assert!(
            result.message.contains("session deleted")
                || result.message.contains("session reset")
                || result.message.contains("no agent service")
        );
    }

    #[tokio::test]
    async fn test_reset_no_existing_session() {
        let tmp = tempfile::tempdir().unwrap();

        let handler = ResetCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            result.message.contains("no session")
                || result.message.contains("session reset")
                || result.message.contains("no agent service")
        );
    }
}
