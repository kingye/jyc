use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// /reset command — reset agent session for this topic.
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
        "Reset agent session for this topic"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // Clear agent-published files and the exchange-access token: /reset must
        // kill previously shared links (token rotation forces regeneration on
        // the next publish). Done for both the agent and fallback branches.
        let jyc_dir = context.topic_path.join(".jyc");
        tokio::fs::remove_dir_all(jyc_dir.join(crate::EXCHANGE_DIR_NAME))
            .await
            .ok();
        tokio::fs::remove_file(jyc_dir.join(crate::EXCHANGE_TOKEN_FILENAME))
            .await
            .ok();

        // Resolve ResetCompressionConfig. Best signal we have at command time:
        // read the matched pattern from disk (written by the message router
        // when the topic was created). Falls back to first pattern if the
        // pattern file is missing, then to global [agent].reset_compression,
        // then to the default (Heuristic).
        let matched_pattern = crate::session_state::read_pattern(&context.topic_path).await;
        let reset_config = crate::session_state::resolve_reset_compression(
            &context.config,
            &context.channel,
            matched_pattern.as_deref(),
        );

        if let Some(ref agent) = context.agent {
            let topic_name = context
                .topic_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            agent
                .reset_session(&context.topic_path, topic_name, &reset_config)
                .await?;
            Ok(CommandResult {
                success: true,
                message: "/reset: session reset successfully".into(),
                error: None,
                append_body: None,
            })
        } else {
            // No agent service available — fallback to direct file deletion
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
                append_body: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn test_context(topic_path: &Path) -> CommandContext {
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
    async fn test_reset_existing_session() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01","context_input_tokens":100,"total_output_tokens":50,"max_input_tokens":1000}"#,
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
    async fn test_reset_clears_exchange_files_and_token() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        let exchange_dir = jyc_dir.join(crate::EXCHANGE_DIR_NAME);
        tokio::fs::create_dir_all(&exchange_dir).await.unwrap();
        tokio::fs::write(exchange_dir.join("report.pdf"), b"pdf")
            .await
            .unwrap();
        tokio::fs::write(jyc_dir.join(crate::EXCHANGE_TOKEN_FILENAME), "abc123")
            .await
            .unwrap();

        let handler = ResetCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!exchange_dir.exists());
        assert!(!jyc_dir.join(crate::EXCHANGE_TOKEN_FILENAME).exists());
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
