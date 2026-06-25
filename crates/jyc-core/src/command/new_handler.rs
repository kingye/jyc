use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// /new command — reset session and clear chat history for this thread.
///
/// Usage:
///   /new    Delete session state files and chat history; next AI prompt will start completely fresh
pub struct NewCommandHandler;

#[async_trait]
impl CommandHandler for NewCommandHandler {
    fn name(&self) -> &str {
        "/new"
    }

    fn description(&self) -> &str {
        "Reset session and clear chat history for this thread"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let agent_path = context.thread_path.join(".jyc/agent-session.json");
        let context_path = context.thread_path.join(".jyc/agent-context.json");

        let mut deleted_session = false;
        if agent_path.exists() {
            tokio::fs::remove_file(&agent_path).await?;
            deleted_session = true;
        }
        if context_path.exists() {
            tokio::fs::remove_file(&context_path).await?;
            deleted_session = true;
        }

        // Delete all chat_history_*.jsonl files in the thread directory
        let mut deleted_history = 0u64;
        let pattern = context.thread_path.join("chat_history_*.jsonl");
        let pattern_str = pattern.to_string_lossy().to_string();

        match glob::glob(&pattern_str) {
            Ok(paths) => {
                for entry in paths {
                    match entry {
                        Ok(path) => {
                            tokio::fs::remove_file(&path).await?;
                            deleted_history += 1;
                        }
                        Err(e) => {
                            tracing::warn!(
                                thread = %context.thread_path.display(),
                                error = %e,
                                "Failed to read chat history path during /new"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    thread = %context.thread_path.display(),
                    error = %e,
                    "Failed to glob chat history files during /new"
                );
            }
        }

        let msg = if deleted_session || deleted_history > 0 {
            format!(
                "/new: session deleted ({} chat history files removed). Fresh start on next AI prompt.",
                deleted_history
            )
        } else {
            "/new: no session or chat history exists. Fresh start on next AI prompt.".into()
        };

        tracing::info!(
            thread = %context.thread_path.display(),
            deleted_session,
            deleted_history,
            "Thread refreshed via /new command"
        );

        Ok(CommandResult {
            success: true,
            message: msg,
            error: None,
        })
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

    async fn setup_session(tmp: &tempfile::TempDir) {
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"sessionId":"test","createdAt":"2026-01-01","totalInputTokens":100,"maxInputTokens":1000}"#,
        )
        .await
        .unwrap();
    }

    async fn setup_chat_history(tmp: &tempfile::TempDir) {
        tokio::fs::write(
            tmp.path().join("chat_history_2026-06-25.jsonl"),
            r#"{"ts":"2026-06-25T10:00:00Z","type":"received","content":"test"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            tmp.path().join("chat_history_2026-06-24.jsonl"),
            r#"{"ts":"2026-06-24T10:00:00Z","type":"received","content":"test"}"#,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_new_with_session_and_history() {
        let tmp = tempfile::tempdir().unwrap();
        setup_session(&tmp).await;
        setup_chat_history(&tmp).await;

        let handler = NewCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("session deleted"));
        assert!(result.message.contains("2 chat history files removed"));
        assert!(!tmp.path().join(".jyc/agent-session.json").exists());
        assert!(!tmp.path().join("chat_history_2026-06-25.jsonl").exists());
        assert!(!tmp.path().join("chat_history_2026-06-24.jsonl").exists());
    }

    #[tokio::test]
    async fn test_new_with_no_session_or_history() {
        let tmp = tempfile::tempdir().unwrap();

        let handler = NewCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("no session or chat history exists"));
    }

    #[tokio::test]
    async fn test_new_with_session_only() {
        let tmp = tempfile::tempdir().unwrap();
        setup_session(&tmp).await;

        let handler = NewCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("session deleted"));
        assert!(result.message.contains("0 chat history files removed"));
        assert!(!tmp.path().join(".jyc/agent-session.json").exists());
    }

    #[tokio::test]
    async fn test_new_with_history_only() {
        let tmp = tempfile::tempdir().unwrap();
        setup_chat_history(&tmp).await;

        let handler = NewCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("session deleted"));
        assert!(result.message.contains("2 chat history files removed"));
        assert!(!tmp.path().join("chat_history_2026-06-25.jsonl").exists());
        assert!(!tmp.path().join("chat_history_2026-06-24.jsonl").exists());
    }
}
