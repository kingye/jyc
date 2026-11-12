use anyhow::Result;
use async_trait::async_trait;
use jyc_types::state_dir::resolve_state_dir;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use super::spec;

/// `/thinking` command - control the reasoning-effort tier sent with each
/// LLM request.
///
/// Usage:
/// - `/thinking low|medium|high` - set the tier, persisted in
///   `.jyc/thinking-effort`; it overrides `[ai.reasoning_effort]` for this
///   topic until changed again
/// - `/thinking` - report the current tier (unset tier reported as unset)
pub struct ThinkingCommandHandler;

const TIERS: [&str; 3] = ["low", "medium", "high"];

#[async_trait]
impl CommandHandler for ThinkingCommandHandler {
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let parsed = match spec::parse_plain("/thinking", &context.args) {
            Ok(parsed) => parsed,
            Err(error) => return Ok(CommandResult::refusal(error.to_string())),
        };
        let arg = parsed
            .positionals
            .first()
            .map(|s| s.to_lowercase())
            .unwrap_or_default();

        // Defence in depth: the dispatcher already refuses commands for a
        // topic closed earlier in the same command block (`/close` +
        // `/thinking high`), but the create below would rebuild the state dir
        // the close just deleted if this is ever reached directly.
        if jyc_types::state_dir::is_closed(&context.topic_name) {
            return Ok(CommandResult::refusal(
                "/thinking: this topic was just closed; nothing to change".to_string(),
            ));
        }

        let state_dir = resolve_state_dir(&context.topic_name, &context.topic_path);

        if arg.is_empty() {
            // Bare `/thinking`: report the tier that would actually apply —
            // the same chain `resolve_effort_tier` walks for every request
            // (runtime file > mode tier > default), not just the file.
            let source;
            let tier;
            match tokio::fs::read_to_string(state_dir.join("thinking-effort")).await {
                Ok(raw) => {
                    tier = raw.trim().to_string();
                    source = "runtime override";
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tier = crate::session_state::resolve_effort_tier(
                        &context.topic_name,
                        &context.topic_path,
                        context.config.as_ref(),
                        &context.channel,
                    )
                    .await
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "none".to_string());
                    source = "config";
                }
                Err(error) => return Err(error.into()),
            }
            let message = format!("/thinking: current tier is '{tier}' ({source})");
            return Ok(CommandResult {
                success: true,
                message,
                error: None,
                append_body: None,
            });
        }

        if !TIERS.contains(&arg.as_str()) {
            return Ok(CommandResult {
                success: false,
                message: format!(
                    "/thinking: unknown tier '{arg}'. Use '/thinking low|medium|high'"
                ),
                error: None,
                append_body: None,
            });
        }

        tokio::fs::create_dir_all(&state_dir).await?;
        tokio::fs::write(state_dir.join("thinking-effort"), &arg).await?;

        Ok(CommandResult {
            success: true,
            message: format!("/thinking: reasoning effort set to '{arg}'"),
            error: None,
            append_body: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context(topic_name: &str, topic_path: &std::path::Path) -> CommandContext {
        jyc_types::state_dir::register(topic_name, &topic_path.join(".jyc"));
        crate::test_support::test_context(topic_name, topic_path)
    }

    #[tokio::test]
    async fn test_thinking_sets_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_thinking_sets_tier";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ThinkingCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["high".to_string()];

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/thinking-effort"))
            .await
            .unwrap();
        assert_eq!(content, "high");
    }

    #[tokio::test]
    async fn test_thinking_bare_reports_config_tier_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_thinking_bare_reports_config_tier";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ThinkingCommandHandler;
        let ctx = test_context(topic, tmp.path());

        // The standard fixture has no [ai.reasoning_effort], so the chain
        // ends at "none".
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            result.message.contains("current tier is 'none' (config)"),
            "{:?}",
            result.message
        );
        jyc_types::state_dir::unregister("test_thinking_bare_reports_config_tier");
        jyc_types::state_dir::clear_closed("test_thinking_bare_reports_config_tier");
    }

    #[tokio::test]
    async fn test_thinking_bare_reports_runtime_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_thinking_bare_reports_runtime_tier";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ThinkingCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["medium".to_string()];
        handler.execute(ctx).await.unwrap();

        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            result
                .message
                .contains("current tier is 'medium' (runtime override)"),
            "{:?}",
            result.message
        );
        jyc_types::state_dir::unregister("test_thinking_bare_reports_runtime");
        jyc_types::state_dir::clear_closed("test_thinking_bare_reports_runtime");
    }

    #[tokio::test]
    async fn test_thinking_invalid_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_thinking_invalid_tier";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ThinkingCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["banana".to_string()];

        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("unknown tier"));
    }
}
