use anyhow::Result;
use async_trait::async_trait;
use jyc_types::state_dir::resolve_state_dir;

use super::flags::{CONFIRM_FLAGS, ConfirmFlag};
use super::handler::{CommandContext, CommandHandler, CommandResult};
use super::spec::{self, ArgError, Parsed};

/// /reset command — reset agent session for this topic.
///
/// Usage:
///   /reset --force    Clear the agent session (chat history is kept)
///
/// Without `--force` the command only warns, so the session cannot be
/// reset by an accidental send (mirrors `/close`).
pub struct ResetCommandHandler;

impl ResetCommandHandler {
    /// Read this command's line through the shape `/reset`'s row declares: no
    /// arguments, and the guard flag the row names.
    fn parse(args: &[String]) -> Result<Parsed<ConfirmFlag>, ArgError> {
        spec::parse_command("/reset", CONFIRM_FLAGS, args)
    }
}

#[async_trait]
impl CommandHandler for ResetCommandHandler {
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let parsed = match Self::parse(&context.args) {
            Ok(parsed) => parsed,
            Err(error) => return Ok(CommandResult::refusal(error.to_string())),
        };

        // Require --force before touching anything — including the
        // exchange token rotation below, so a plain /reset cannot kill
        // previously published links by accident.
        if !parsed.has(ConfirmFlag::Force) {
            return Ok(CommandResult {
                success: true,
                message: "⚠️  /reset will clear this topic's agent session (chat history is \
                         kept).\n\
                         \n\
                         To proceed, send: /reset --force"
                    .into(),
                error: None,
                append_body: None,
            });
        }

        context.session_end_hook("reset").await;

        // Clear agent-published files and the exchange-access token: /reset must
        // kill previously shared links (token rotation forces regeneration on
        // the next publish). Done for both the agent and fallback branches.
        let state_dir = resolve_state_dir(&context.topic_name, &context.topic_path);
        tokio::fs::remove_dir_all(state_dir.join(crate::EXCHANGE_DIR_NAME))
            .await
            .ok();
        tokio::fs::remove_file(state_dir.join(crate::EXCHANGE_TOKEN_FILENAME))
            .await
            .ok();

        // The task list is part of the session being reset — chat history is
        // kept, the agent's plan for the current piece of work is not.
        crate::session_state::clear_tasks_at(&state_dir).await;

        if let Some(ref agent) = context.agent {
            // Identity is the topic name, not the workspace dir — a shared child
            // works in its parent's dir, so the path would name the parent.
            let topic_name = context.topic_name.as_str();
            agent.reset_session(&context.topic_path, topic_name).await?;
            context.session_start_hook("reset").await;
            Ok(CommandResult {
                success: true,
                message: "/reset: session reset successfully".into(),
                error: None,
                append_body: None,
            })
        } else {
            // No agent service available — fallback to direct file deletion
            tokio::fs::remove_file(state_dir.join("agent-session.json"))
                .await
                .ok();
            crate::session_state::remove_raw_context_files(&state_dir).await;
            context.session_start_hook("reset").await;
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
    use crate::agent::{AgentResult, AgentService};
    use std::path::Path;
    use std::sync::Arc;

    /// `-f` is the same guard `--force` is, off the declaration `/close` and
    /// `/new` read.
    #[test]
    fn is_forced_reads_the_short_guard_spelling() {
        let forced = |items: &[&str]| {
            ResetCommandHandler::parse(&items.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .unwrap()
                .has(ConfirmFlag::Force)
        };
        assert!(forced(&["-f"]));
        assert!(forced(&["--force"]));
        assert!(!forced(&[]));
    }

    /// A word where `/reset` takes none is refused rather than ignored.
    #[test]
    fn a_stray_argument_is_refused() {
        assert!(
            ResetCommandHandler::parse(&["joe".to_string()])
                .unwrap_err()
                .to_string()
                .starts_with("too many arguments")
        );
    }

    /// Records the topic names it is asked to reset.
    #[derive(Default)]
    struct RecordingAgent {
        reset_names: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl AgentService for RecordingAgent {
        async fn base_url(&self) -> Result<String> {
            Ok(String::new())
        }
        async fn process(
            &self,
            _message: &jyc_types::InboundMessage,
            _topic_name: &str,
            _topic_path: &Path,
            _message_dir: &str,
            _pending_rx: &mut tokio::sync::mpsc::Receiver<jyc_types::QueueItem>,
            _topic_cancel: tokio_util::sync::CancellationToken,
        ) -> Result<AgentResult> {
            Ok(AgentResult {
                reply_delivered: false,
                reply_text: None,
            })
        }
        async fn reset_session(&self, _topic_path: &Path, topic_name: &str) -> Result<()> {
            self.reset_names
                .lock()
                .unwrap()
                .push(topic_name.to_string());
            Ok(())
        }
    }

    /// Fork shape: the topic name differs from the shared workspace dir —
    /// the agent must be reset by name, not by dir name.
    #[tokio::test]
    async fn test_reset_uses_topic_name_not_workspace_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_reset_uses_topic_name";
        let workspace = tmp.path().join("parent-repo");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        let agent = Arc::new(RecordingAgent::default());
        let handler = ResetCommandHandler;
        let mut ctx = test_context(topic, &workspace);
        ctx.agent = Some(agent.clone());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let names = agent.reset_names.lock().unwrap();
        assert_eq!(
            names.as_slice(),
            [topic.to_string()],
            "reset must target the topic name, not the workspace dir name"
        );
    }

    fn test_context(topic_name: &str, topic_path: &Path) -> CommandContext {
        jyc_types::state_dir::register(topic_name, &topic_path.join(".jyc"));
        let mut context = crate::test_support::test_context(topic_name, topic_path);
        // Pass --force by default so the destructive-action tests exercise the
        // real path; guard tests override `args`.
        context.args = vec!["--force".to_string()];
        context
    }

    #[tokio::test]
    async fn test_reset_without_force_warns_and_keeps_session() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_reset_without_force_warns_and_keeps_session";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        let session = state_dir.join("agent-session.json");
        tokio::fs::write(
            &session,
            r#"{"created_at":"2026-01-01","context_input_tokens":100,"total_output_tokens":50,"max_input_tokens":1000}"#,
        )
        .await
        .unwrap();

        let handler = ResetCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![];

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            result.message.contains("/reset --force"),
            "warning should tell the user how to proceed, got: {}",
            result.message
        );
        assert!(session.exists(), "plain /reset must not touch the session");
    }

    #[tokio::test]
    async fn test_reset_existing_session() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_reset_existing_session";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01","context_input_tokens":100,"total_output_tokens":50,"max_input_tokens":1000}"#,
        )
        .await
        .unwrap();

        crate::session_state::write_tasks_at(
            topic,
            &state_dir,
            &jyc_types::task::TaskList {
                items: vec![jyc_types::task::TaskItem {
                    id: 1,
                    text: "step".into(),
                    status: jyc_types::task::TaskStatus::InProgress,
                }],
            },
        )
        .await
        .unwrap();

        let handler = ResetCommandHandler;
        let ctx = test_context(topic, tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!state_dir.join("agent-session.json").exists());
        assert!(
            crate::session_state::read_tasks_at(&state_dir)
                .await
                .is_none(),
            "/reset must clear the task list"
        );
        assert!(
            result.message.contains("session deleted")
                || result.message.contains("session reset")
                || result.message.contains("no agent service")
        );
    }

    #[tokio::test]
    async fn test_reset_clears_exchange_files_and_token() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_reset_clears_exchange_files_and_token";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        let exchange_dir = state_dir.join(crate::EXCHANGE_DIR_NAME);
        tokio::fs::create_dir_all(&exchange_dir).await.unwrap();
        tokio::fs::write(exchange_dir.join("report.pdf"), b"pdf")
            .await
            .unwrap();
        tokio::fs::write(state_dir.join(crate::EXCHANGE_TOKEN_FILENAME), "abc123")
            .await
            .unwrap();

        let handler = ResetCommandHandler;
        let ctx = test_context(topic, tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!exchange_dir.exists());
        assert!(!state_dir.join(crate::EXCHANGE_TOKEN_FILENAME).exists());
    }

    #[tokio::test]
    async fn test_reset_no_existing_session() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_reset_no_existing_session";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));

        let handler = ResetCommandHandler;
        let ctx = test_context(topic, tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            result.message.contains("no session")
                || result.message.contains("session reset")
                || result.message.contains("no agent service")
        );
    }
}
