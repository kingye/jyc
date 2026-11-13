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
/// - `/thinking auto` - clear the runtime override; the topic falls back to
///   the config tier, and with no config tier no reasoning parameter is sent
/// - `/thinking` - report the current tier (unset reported as `auto`)
pub struct ThinkingCommandHandler;

const TIERS: [&str; 4] = ["low", "medium", "high", "auto"];

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
            let (tier, source) =
                match tokio::fs::read_to_string(state_dir.join("thinking-effort")).await {
                    Ok(raw) => (raw.trim().to_string(), "runtime override"),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let tier = crate::session_state::resolve_effort_tier(
                            &context.topic_name,
                            &context.topic_path,
                            context.config.as_ref(),
                            &context.channel,
                        )
                        .await
                        .map(|t| t.to_string())
                        .unwrap_or_else(|| "auto".to_string());
                        (tier, "config")
                    }
                    Err(error) => return Err(error.into()),
                };
            let message = format!("/thinking: current effort is '{tier}' ({source})");
            return Ok(CommandResult {
                success: true,
                message,
                error: None,
                append_body: None,
            });
        }

        if arg == "auto" {
            // `auto` clears the runtime override; the topic falls back to
            // whatever the config chain resolves (possibly none — no
            // reasoning parameter sent).
            match tokio::fs::remove_file(state_dir.join("thinking-effort")).await {
                Ok(()) => {
                    return Ok(CommandResult {
                        success: true,
                        message: "/thinking: runtime override cleared; using config tier (auto)"
                            .to_string(),
                        error: None,
                        append_body: None,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(CommandResult {
                        success: true,
                        message: "/thinking: no runtime override; already using config tier (auto)"
                            .to_string(),
                        error: None,
                        append_body: None,
                    });
                }
                Err(error) => return Err(error.into()),
            }
        }

        if !TIERS.contains(&arg.as_str()) {
            return Ok(CommandResult {
                success: false,
                message: format!(
                    "/thinking: unknown tier '{arg}'. Use '/thinking low|medium|high|auto'"
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

    /// With no runtime file, the bare report falls through to the config
    /// tier and names the source.
    #[tokio::test]
    async fn test_thinking_bare_reports_config_tier_when_config_has_one() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_thinking_bare_reports_config_tier_when_config_has_one";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let ctx = crate::test_support::test_context_with_config(
            topic,
            tmp.path(),
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
[ai]
enabled = true
mode = "agent"
[ai.reasoning_effort]
default = "medium"
"#,
        );
        let handler = ThinkingCommandHandler;
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success, "{:?}", result.message);
        assert!(
            result
                .message
                .contains("current effort is 'medium' (config)"),
            "{:?}",
            result.message
        );
        jyc_types::state_dir::unregister(topic);
        jyc_types::state_dir::clear_closed(topic);
    }

    #[tokio::test]
    async fn test_thinking_bare_reports_config_tier_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_thinking_bare_reports_config_tier";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ThinkingCommandHandler;
        let ctx = test_context(topic, tmp.path());

        // The standard fixture has no [ai.reasoning_effort], so the chain
        // ends at "auto".
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            result.message.contains("current effort is 'auto' (config)"),
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
                .contains("current effort is 'medium' (runtime override)"),
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

    /// `/thinking auto` clears the runtime override; a subsequent bare
    /// report falls back to the config tier.
    #[tokio::test]
    async fn test_thinking_auto_clears_runtime_override() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_thinking_auto_clears_runtime_override";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ThinkingCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["high".to_string()];
        handler.execute(ctx).await.unwrap();

        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["auto".to_string()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success, "{:?}", result.message);
        assert!(
            !tmp.path().join(".jyc/thinking-effort").exists(),
            "override file should be removed"
        );

        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            result.message.contains("current effort is 'auto' (config)"),
            "{:?}",
            result.message
        );
        jyc_types::state_dir::unregister(topic);
        jyc_types::state_dir::clear_closed(topic);
    }

    /// `/thinking auto` with no override still succeeds.
    #[tokio::test]
    async fn test_thinking_auto_without_override() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_thinking_auto_without_override";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ThinkingCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["auto".to_string()];

        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success, "{:?}", result.message);
        assert!(result.message.contains("no runtime override"));
        jyc_types::state_dir::unregister(topic);
        jyc_types::state_dir::clear_closed(topic);
    }
}
