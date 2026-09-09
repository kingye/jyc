use anyhow::Result;
use async_trait::async_trait;
use jyc_types::state_dir::jyc_dir;

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// /new command — reset session, clear chat history, and clear
/// exchange-published files (+ token) for this topic.
///
/// Usage:
///   /new --force    Delete session state files and chat history; next AI prompt will start completely fresh
///
/// Without `--force` the command only warns, so the destructive action
/// cannot be triggered by an accidental send (mirrors `/close`).
/// Delete all files matching a glob pattern. Returns count of deleted files.
async fn delete_glob_files(pattern: &std::path::Path) -> u64 {
    let pattern_str = pattern.to_string_lossy().to_string();
    let mut count = 0u64;
    match glob::glob(&pattern_str) {
        Ok(paths) => {
            for entry in paths {
                match entry {
                    Ok(path) => {
                        if tokio::fs::remove_file(&path).await.is_ok() {
                            count += 1;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            pattern = %pattern.display(),
                            error = %e,
                            "Failed to read path during /new glob"
                        );
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                pattern = %pattern.display(),
                error = %e,
                "Failed to glob files during /new"
            );
        }
    }
    count
}

pub struct NewCommandHandler;

impl NewCommandHandler {
    /// Returns `true` if the args contain the explicit `--force` flag.
    fn is_forced(args: &[String]) -> bool {
        args.iter().any(|a| a == "--force")
    }
}

#[async_trait]
impl CommandHandler for NewCommandHandler {
    fn name(&self) -> &str {
        "/new"
    }

    fn description(&self) -> &str {
        "Reset session and clear chat history (requires --force)"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // Require --force to prevent accidentally wiping the session and
        // chat history. Plain /new returns a warning instead.
        if !Self::is_forced(&context.args) {
            return Ok(CommandResult {
                success: true,
                message: "⚠️  /new will PERMANENTLY delete this topic's session and chat \
                         history. This cannot be undone.\n\
                         \n\
                         To proceed, send: /new --force"
                    .into(),
                error: None,
                append_body: None,
            });
        }

        let state = jyc_types::state_dir::jyc_dir(&context.topic_path);
        let agent_path = state.join("agent-session.json");
        let context_path = state.join("agent-context.json");
        let activity_path = state.join("activity.jsonl");

        let mut deleted_session = false;
        if agent_path.exists() {
            tokio::fs::remove_file(&agent_path).await?;
            deleted_session = true;
        }
        if context_path.exists() {
            tokio::fs::remove_file(&context_path).await?;
            deleted_session = true;
        }
        if activity_path.exists() {
            tokio::fs::remove_file(&activity_path).await?;
            deleted_session = true;
        }

        // Clear exchange-published files and the exchange token, mirroring
        // /reset: /new starts fresh, so previously shared links must die.
        let jyc_dir = jyc_dir(&context.topic_path);
        tokio::fs::remove_dir_all(jyc_dir.join(crate::EXCHANGE_DIR_NAME))
            .await
            .ok();
        tokio::fs::remove_file(jyc_dir.join(crate::EXCHANGE_TOKEN_FILENAME))
            .await
            .ok();

        // Delete all chat_history_*.jsonl files in the topic directory (both locations)
        let mut deleted_history = 0u64;

        // New location: .jyc/
        let new_pattern =
            jyc_types::state_dir::jyc_dir(&context.topic_path).join("chat_history_*.jsonl");
        deleted_history += delete_glob_files(&new_pattern).await;

        // Legacy location: topic root
        let root_pattern = context.topic_path.join("chat_history_*.jsonl");
        deleted_history += delete_glob_files(&root_pattern).await;

        let msg = if deleted_session || deleted_history > 0 {
            format!(
                "/new: session deleted ({} chat history files removed). Fresh start on next AI prompt.",
                deleted_history
            )
        } else {
            "/new: no session or chat history exists. Fresh start on next AI prompt.".into()
        };

        tracing::info!(
            topic = %context.topic_path.display(),
            deleted_session,
            deleted_history,
            "Topic refreshed via /new command"
        );

        Ok(CommandResult {
            success: true,
            message: msg,
            error: None,
            append_body: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn test_context(topic_path: &Path) -> CommandContext {
        CommandContext {
            // Pass --force by default so the destructive-action tests
            // exercise the real path; guard tests override `args`.
            args: vec!["--force".to_string()],
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
            per_agent_commands: vec![],
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
    async fn test_new_without_force_warns_and_keeps_files() {
        let tmp = tempfile::tempdir().unwrap();
        setup_session(&tmp).await;
        setup_chat_history(&tmp).await;

        let handler = NewCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec![];

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            result.message.contains("/new --force"),
            "warning should tell the user how to proceed, got: {}",
            result.message
        );
        assert!(tmp.path().join(".jyc/agent-session.json").exists());
        assert!(tmp.path().join("chat_history_2026-06-25.jsonl").exists());
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

    #[tokio::test]
    async fn test_new_deletes_activity_log() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("activity.jsonl"),
            r#"{"text":"test","timestamp":"2026-06-25T10:00:00Z","severity":"info"}"#,
        )
        .await
        .unwrap();

        let handler = NewCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            !tmp.path().join(".jyc/activity.jsonl").exists(),
            "activity.jsonl should be deleted by /new"
        );
    }

    #[tokio::test]
    async fn test_new_clears_exchange_files_and_token() {
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

        let handler = NewCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!exchange_dir.exists());
        assert!(!jyc_dir.join(crate::EXCHANGE_TOKEN_FILENAME).exists());
    }
}
