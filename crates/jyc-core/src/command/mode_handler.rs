use anyhow::Result;
use async_trait::async_trait;
use jyc_types::state_dir::resolve_state_dir;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::session_state;

/// Switch the topic to `mode` (`"plan"` or `"build"`).
///
/// Plan mode writes `.jyc/mode-override`; build mode removes it (build is the
/// default). Also refreshes `context_window` so the dashboard sees the new
/// model's window immediately.
///
/// Shared by `/plan`, `/build`, and user-defined commands that declare a mode.
pub async fn set_mode(context: &CommandContext, mode: &str) -> Result<()> {
    // A `/plan` (or a mode-declaring custom command) stacked behind a `/close`
    // in the same message still runs; changing the mode of a topic that is
    // gone is a no-op, and the `create_dir_all` below would bring back the
    // state dir the close just deleted.
    if jyc_types::state_dir::is_closed(&context.topic_name) {
        return Ok(());
    }
    let state_dir = resolve_state_dir(&context.topic_name, &context.topic_path);
    let override_path = state_dir.join("mode-override");

    if mode == "plan" {
        tokio::fs::create_dir_all(&state_dir).await?;
        tokio::fs::write(&override_path, "plan").await?;
    } else if override_path.exists() {
        tokio::fs::remove_file(&override_path).await?;
    }

    // Update `context_window` in agent-session.json so the dashboard
    // reflects the active model's window immediately.
    refresh_context_window(context).await;

    // Mode is passed per-prompt (PromptRequest.agent), not per-session.
    // Session is preserved — AI keeps conversation memory.
    Ok(())
}

/// /plan command — switch to plan mode (read-only).
pub struct PlanCommandHandler;

#[async_trait]
impl CommandHandler for PlanCommandHandler {
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        set_mode(&context, "plan").await?;

        Ok(CommandResult {
            success: true,
            message: "/plan: switched to plan mode (read-only)".into(),
            error: None,
            append_body: None,
        })
    }
}

/// /build command — switch to build mode (full execution, default).
pub struct BuildCommandHandler;

#[async_trait]
impl CommandHandler for BuildCommandHandler {
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        set_mode(&context, "build").await?;

        Ok(CommandResult {
            success: true,
            message: "/build: switched to build mode (full execution)".into(),
            error: None,
            append_body: None,
        })
    }
}

/// Resolve the active model's context window and write it to
/// `agent-session.json`. Best-effort: silently no-ops if resolution fails
/// (the agent loop writes it on the next turn).
async fn refresh_context_window(context: &CommandContext) {
    let new_window = session_state::resolve_active_context_window(
        &context.topic_name,
        &context.topic_path,
        &context.config,
        &context.channel,
    )
    .await;
    if let Some(new_window) = new_window {
        session_state::write_context_window(&context.topic_name, &context.topic_path, new_window)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_context(topic_name: &str, topic_path: &std::path::Path) -> CommandContext {
        jyc_types::state_dir::register(topic_name, &topic_path.join(".jyc"));
        crate::test_support::test_context(topic_name, topic_path)
    }

    #[tokio::test]
    async fn test_plan_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_plan_mode";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = PlanCommandHandler;
        let ctx = test_context(topic, tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/mode-override"))
            .await
            .unwrap();
        assert_eq!(content, "plan");
    }

    #[tokio::test]
    async fn test_build_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_build_mode";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(state_dir.join("mode-override"), "plan")
            .await
            .unwrap();

        let handler = BuildCommandHandler;
        let ctx = test_context(topic, tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!state_dir.join("mode-override").exists());
    }

    #[tokio::test]
    async fn test_plan_preserves_session() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_plan_preserves_session";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(state_dir.join("agent-session.json"), r#"{"created_at":"2026-01-01","context_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#)
            .await
            .unwrap();

        let handler = PlanCommandHandler;
        let ctx = test_context(topic, tmp.path());
        handler.execute(ctx).await.unwrap();

        // Session file should still exist
        assert!(state_dir.join("agent-session.json").exists());
    }

    #[tokio::test]
    async fn test_plan_writes_context_window() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_plan_writes_context_window";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let mut ctx = test_context(topic, tmp.path());
        // Add a plan_model with a known context_window
        ctx.config = Arc::new(
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
plan_model = "p/plan-1m"
build_model = "p/build-256k"

[agent.providers.p]
type = "openai-compatible"
base_url = "https://x"
api_key = "test-key"

[agent.providers.p.models.plan-1m]
context_window = 1000000

[agent.providers.p.models.build-256k]
context_window = 256000
"#,
            )
            .unwrap(),
        );
        let handler = PlanCommandHandler;
        handler.execute(ctx).await.unwrap();

        // `context_window` in agent-session.json reflects the plan model's
        // full window (1M) — the auto-reset cap is gone, so there is no
        // 0.95 factor any more.
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_window"], 1000000);
        assert!(state.get("max_input_tokens").is_none());
    }

    #[tokio::test]
    async fn test_build_writes_context_window() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_build_writes_context_window";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        // Start in plan mode (so /build transitions away from it)
        tokio::fs::write(state_dir.join("mode-override"), "plan")
            .await
            .unwrap();
        // Pre-existing session with old max_input_tokens from plan
        tokio::fs::write(
            state_dir.join("agent-session.json"),
            r#"{"max_input_tokens":950000}"#,
        )
        .await
        .unwrap();

        let mut ctx = test_context(topic, tmp.path());
        ctx.config = Arc::new(
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
plan_model = "p/plan-1m"
build_model = "p/build-256k"

[agent.providers.p]
type = "openai-compatible"
base_url = "https://x"
api_key = "test-key"

[agent.providers.p.models.plan-1m]
context_window = 1000000

[agent.providers.p.models.build-256k]
context_window = 256000
"#,
            )
            .unwrap(),
        );
        let handler = BuildCommandHandler;
        handler.execute(ctx).await.unwrap();

        // mode-override removed
        assert!(!state_dir.join("mode-override").exists());
        // `context_window` now reflects build-256k's full window (256k);
        // the legacy `max_input_tokens` key written by the plan mode is
        // dropped in the same write.
        let content = tokio::fs::read_to_string(state_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["context_window"], 256000);
        assert!(state.get("max_input_tokens").is_none());
    }
}
