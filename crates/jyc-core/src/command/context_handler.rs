use anyhow::Result;
use async_trait::async_trait;

use crate::template_dirs::TemplateDirs;

use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};
use serde::{Deserialize, Serialize};

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// `/context` command — view or change the context management strategy for
/// this topic. The on-disk `.jyc/agent-context.json` always stores the
/// full raw context; this command only changes what is sent to the LLM.
///
/// Usage:
///   /context            Show current strategy
///   /context full       Switch to full context (send everything)
///   /context sliding [N]  Switch to sliding window (default N = 10 turns)
///   /context reset      Remove runtime override (revert to configured default)
pub struct ContextCommandHandler;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContextStrategyFile {
    mode: ContextStrategy,
    #[serde(default = "default_window")]
    window: usize,
}

fn default_window() -> usize {
    10
}

#[async_trait]
impl CommandHandler for ContextCommandHandler {
    fn name(&self) -> &str {
        "/context"
    }

    fn description(&self) -> &str {
        "View or change the context management strategy for this topic"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let jyc_dir = context.topic_path.join(".jyc");
        let override_path = jyc_dir.join(crate::session_state::CONTEXT_STRATEGY_FILE);

        let matched_pattern = crate::session_state::read_pattern(&context.topic_path).await;
        let configured = crate::session_state::resolve_context_strategy(
            &context.config,
            &context.channel,
            matched_pattern.as_deref(),
        );

        // No args → show current strategy (runtime override takes priority).
        if context.args.is_empty() {
            let runtime =
                crate::session_state::read_context_strategy_override(&context.topic_path).await;
            let (label, source) = match &runtime {
                Some(cs) => (
                    format!("{} (window={})", describe(cs), cs.window),
                    "override",
                ),
                None => (
                    format!("{} (window={})", describe(&configured), configured.window),
                    "default",
                ),
            };
            return Ok(CommandResult {
                success: true,
                message: format!("/context: current strategy is {label} ({source})"),
                error: None,
                append_body: None,
            });
        }

        let cmd = context.args[0].to_lowercase();
        match cmd.as_str() {
            "reset" => {
                tokio::fs::remove_file(&override_path).await.ok();
                Ok(CommandResult {
                    success: true,
                    message: format!(
                        "/context: cleared override, now using {} (window={})",
                        describe(&configured),
                        configured.window,
                    ),
                    error: None,
                    append_body: None,
                })
            }
            "full" => {
                write_override(&jyc_dir, &override_path, "full", None).await?;
                Ok(CommandResult {
                    success: true,
                    message: "/context: switched to full (all history sent)".into(),
                    error: None,
                    append_body: None,
                })
            }
            "sliding" | "sliding_window" => {
                let window = if let Some(arg) = context.args.get(1) {
                    match arg.parse::<usize>() {
                        Ok(n) if n > 0 => Some(n),
                        Ok(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: "/context: window must be > 0".into(),
                                error: None,
                                append_body: None,
                            });
                        }
                        Err(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: invalid window '{arg}', expected a positive integer"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                    }
                } else {
                    None
                };
                write_override(&jyc_dir, &override_path, "sliding_window", window).await?;
                let cfg = ContextStrategyConfig {
                    mode: ContextStrategy::SlidingWindow,
                    window: window.unwrap_or_else(default_window),
                };
                Ok(CommandResult {
                    success: true,
                    message: format!(
                        "/context: switched to sliding_window (window={})",
                        cfg.window,
                    ),
                    error: None,
                    append_body: None,
                })
            }
            other => Ok(CommandResult {
                success: false,
                message: format!(
                    "/context: unknown argument '{other}'. Use: /context full | sliding [N] | reset"
                ),
                error: None,
                append_body: None,
            }),
        }
    }
}

async fn write_override(
    jyc_dir: &std::path::Path,
    path: &std::path::Path,
    mode: &str,
    window: Option<usize>,
) -> Result<()> {
    tokio::fs::create_dir_all(jyc_dir).await?;
    let payload = ContextStrategyFile {
        mode: serde_json::from_str(&format!("\"{mode}\"")).expect("valid mode"),
        window: window.unwrap_or_else(default_window),
    };
    let json = serde_json::to_string(&payload)?;
    tokio::fs::write(path, json).await?;
    Ok(())
}

fn describe(cfg: &ContextStrategyConfig) -> &'static str {
    match cfg.mode {
        ContextStrategy::Full => "full",
        ContextStrategy::SlidingWindow => "sliding_window",
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
            template_dirs: TemplateDirs::single(PathBuf::from("/tmp/test/templates")),
            channel_type: "email".into(),
            config_path: None,
        }
    }

    #[tokio::test]
    async fn test_show_default() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let ctx = test_context(tmp.path());
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("full"));
        assert!(result.message.contains("default"));
    }

    #[tokio::test]
    async fn test_full_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["full".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["mode"], "full");
    }

    #[tokio::test]
    async fn test_sliding_with_window() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "7".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("window=7"));
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["mode"], "sliding_window");
        assert_eq!(v["window"], 7);
    }

    #[tokio::test]
    async fn test_sliding_default_window() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding_window".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["mode"], "sliding_window");
        assert_eq!(v["window"], 10);
    }

    #[tokio::test]
    async fn test_sliding_window_must_be_positive() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "0".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("window must be"));
    }

    #[tokio::test]
    async fn test_sliding_window_invalid_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "abc".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("invalid window"));
    }

    #[tokio::test]
    async fn test_reset_clears_override() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("context-strategy.json"),
            r#"{"mode":"sliding_window","window":3}"#,
        )
        .await
        .unwrap();

        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["reset".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!jyc_dir.join("context-strategy.json").exists());
    }

    #[tokio::test]
    async fn test_show_after_override() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("context-strategy.json"),
            r#"{"mode":"sliding_window","window":5}"#,
        )
        .await
        .unwrap();

        let handler = ContextCommandHandler;
        let ctx = test_context(tmp.path());
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("sliding_window"));
        assert!(result.message.contains("window=5"));
        assert!(result.message.contains("override"));
    }

    #[tokio::test]
    async fn test_unknown_subcommand() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["bogus".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("unknown argument"));
    }
}
