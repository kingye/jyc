use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::session_state;

/// Switch the topic to `mode` (`"plan"` or `"build"`).
///
/// Plan mode writes `.jyc/mode-override`; build mode removes it (build is the
/// default). Also refreshes `max_input_tokens` so the dashboard and the
/// pre-loop context check see the new model's window immediately.
///
/// Shared by `/plan`, `/build`, and user-defined commands that declare a mode.
pub async fn set_mode(context: &CommandContext, mode: &str) -> Result<()> {
    let jyc_dir = context.topic_path.join(".jyc");
    let override_path = jyc_dir.join("mode-override");

    if mode == "plan" {
        tokio::fs::create_dir_all(&jyc_dir).await?;
        tokio::fs::write(&override_path, "plan").await?;
    } else if override_path.exists() {
        tokio::fs::remove_file(&override_path).await?;
    }

    // Update `max_input_tokens` in agent-session.json so the dashboard
    // reflects the active model's window immediately (and the pre-loop
    // pre-check has the right threshold to compare against on the
    // next turn).
    refresh_max_input_tokens(context).await;

    // Mode is passed per-prompt (PromptRequest.agent), not per-session.
    // Session is preserved — AI keeps conversation memory.
    Ok(())
}

/// /plan command — switch to plan mode (read-only).
pub struct PlanCommandHandler;

#[async_trait]
impl CommandHandler for PlanCommandHandler {
    fn name(&self) -> &str {
        "/plan"
    }

    fn description(&self) -> &str {
        "Switch to plan mode (read-only)"
    }

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
    fn name(&self) -> &str {
        "/build"
    }

    fn description(&self) -> &str {
        "Switch to build mode (full execution)"
    }

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

/// Resolve the active model's `max_input_tokens` and write it to
/// `agent-session.json`. Best-effort: silently no-ops if resolution fails
/// (the post-loop `update_tokens` will set it on the next turn).
async fn refresh_max_input_tokens(context: &CommandContext) {
    let new_max = session_state::resolve_active_context_window(
        &context.topic_path,
        &context.config,
        &context.channel,
        context.config.ai.auto_reset_threshold,
    )
    .await;
    if let Some(new_max) = new_max {
        session_state::write_max_input_tokens(&context.topic_path, new_max).await;
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
    async fn test_plan_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = PlanCommandHandler;
        let ctx = test_context(tmp.path());

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
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("mode-override"), "plan")
            .await
            .unwrap();

        let handler = BuildCommandHandler;
        let ctx = test_context(tmp.path());

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!jyc_dir.join("mode-override").exists());
    }

    #[tokio::test]
    async fn test_plan_preserves_session() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("agent-session.json"), r#"{"created_at":"2026-01-01","context_input_tokens":0,"total_output_tokens":0,"max_input_tokens":0}"#)
            .await
            .unwrap();

        let handler = PlanCommandHandler;
        let ctx = test_context(tmp.path());
        handler.execute(ctx).await.unwrap();

        // Session file should still exist
        assert!(jyc_dir.join("agent-session.json").exists());
    }

    #[tokio::test]
    async fn test_plan_writes_max_input_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = test_context(tmp.path());
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
auto_reset_threshold = 0.95

[agent.providers.p]
type = "openai-compatible"
base_url = "https://x"
api_key_env = "X"

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

        // max_input_tokens in agent-session.json should reflect plan-1m (95% of 1M = 950k)
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["max_input_tokens"], 950000);
    }

    #[tokio::test]
    async fn test_build_writes_max_input_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        // Start in plan mode (so /build transitions away from it)
        tokio::fs::write(jyc_dir.join("mode-override"), "plan")
            .await
            .unwrap();
        // Pre-existing session with old max_input_tokens from plan
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"max_input_tokens":950000}"#,
        )
        .await
        .unwrap();

        let mut ctx = test_context(tmp.path());
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
auto_reset_threshold = 0.95

[agent.providers.p]
type = "openai-compatible"
base_url = "https://x"
api_key_env = "X"

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
        assert!(!jyc_dir.join("mode-override").exists());
        // max_input_tokens now reflects build-256k (95% of 256k = 243200)
        let content = tokio::fs::read_to_string(jyc_dir.join("agent-session.json"))
            .await
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["max_input_tokens"], 243200);
    }
}
