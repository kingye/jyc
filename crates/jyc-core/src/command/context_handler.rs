use anyhow::Result;
use async_trait::async_trait;

use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// `/context` command — view or change the context management strategy for
/// this topic. The on-disk `.jyc/agent-context.json` always stores the
/// full raw context; this command only changes what is sent to the LLM.
///
/// Usage:
///   /context              Show current strategy
///   /context full         Switch to full context (send everything)
///   /context sliding [N] [M] [CAP]
///     Switch to sliding window (default N = 15 turns). M is the optional
///     note window: only the most recent M windowed turns carry tool-call
///     history notes (default: 5). CAP is the optional per-tool-result
///     byte cap on the verbatim prior region (region ②; current turn is never
///     truncated) (default: 2048). 0 in any slot keeps the configured value.
///   /context reset        Remove runtime override (revert to configured default)
///   /context dump [on|off]
///     Toggle wire-payload debug dump. When on, every LLM call appends
///     one JSON line to `<topic>/.jyc/wire-payload.jsonl` (capped at 50).
///     No args shows current state.
pub struct ContextCommandHandler;

/// Upper bound on the `window` accepted from the user via `/context`. A
/// larger value degenerates into "keep all prior pairs" (full mode minus
/// tools) and risks unbounded memory growth when paired with very long
/// histories, so we cap it here at the command boundary.
const MAX_WINDOW: usize = 200;

/// Upper bound on the per-tool-result byte cap accepted from the user via
/// `/context sliding N M CAP`. Larger caps defeat the purpose of capping
/// and exceed the size of typical LLM context windows, so we cap at 1 MB
/// here. Users needing larger caps can edit `.jyc/context-strategy.json`
/// directly.
const MAX_TOOL_RESULT_CAP: usize = 1024 * 1024;

#[async_trait]
impl CommandHandler for ContextCommandHandler {
    fn name(&self) -> &str {
        "/context"
    }

    fn description(&self) -> &str {
        "View or change the context strategy / debug-dump wire payload"
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
                Some(cs) => (describe_strategy(cs), "override"),
                None => (describe_strategy(&configured), "default"),
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
                        "/context: cleared override, now using {}",
                        describe_strategy(&configured),
                    ),
                    error: None,
                    append_body: None,
                })
            }
            "full" => {
                write_override(
                    &jyc_dir,
                    &override_path,
                    ContextStrategyConfig {
                        mode: ContextStrategy::Full,
                        // Preserve the configured window/note_window/tool_result_cap
                        // so toggling full → sliding later still has sensible
                        // defaults.
                        window: configured.window,
                        note_window: configured.note_window,
                        tool_result_cap: configured.tool_result_cap,
                    },
                )
                .await?;
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
                        Ok(n) if n > 0 && n <= MAX_WINDOW => Some(n),
                        Ok(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: window must be between 1 and {MAX_WINDOW}"
                                ),
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
                let window = window.unwrap_or(configured.window);
                // Optional note window M (args[2]): only the most recent M
                // windowed turns carry tool-call history notes. 0 is valid
                // (text-only window); absent = keep configured default.
                let note_window = if let Some(arg) = context.args.get(2) {
                    match arg.parse::<usize>() {
                        Ok(m) if m <= MAX_WINDOW => Some(Some(m)),
                        Ok(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: note window must be between 0 and {MAX_WINDOW}"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                        Err(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: invalid note window '{arg}', expected a non-negative integer"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                    }
                } else {
                    None
                };
                let note_window = note_window.unwrap_or(configured.note_window);
                // Optional per-tool-result byte cap CAP (args[3]): truncates
                // each tool result in the verbatim region to at most CAP
                // bytes. 0 is the explicit "off" sentinel; absent = keep
                // configured default.
                let tool_result_cap = if let Some(arg) = context.args.get(3) {
                    match arg.parse::<usize>() {
                        Ok(c) if c <= MAX_TOOL_RESULT_CAP => Some(Some(c)),
                        Ok(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: tool_result_cap must be between 0 and {MAX_TOOL_RESULT_CAP} bytes"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                        Err(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: invalid tool_result_cap '{arg}', expected a non-negative integer"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                    }
                } else {
                    None
                };
                let tool_result_cap = tool_result_cap.unwrap_or(configured.tool_result_cap);
                let cfg = ContextStrategyConfig {
                    mode: ContextStrategy::SlidingWindow,
                    window,
                    note_window,
                    tool_result_cap,
                };
                write_override(&jyc_dir, &override_path, cfg.clone()).await?;
                Ok(CommandResult {
                    success: true,
                    message: format!("/context: switched to {}", describe_strategy(&cfg)),
                    error: None,
                    append_body: None,
                })
            }
            "dump" => handle_dump(&jyc_dir, context.args.get(1).map(|s| s.as_str())).await,
            other => Ok(CommandResult {
                success: false,
                message: format!(
                    "/context: unknown argument '{other}'. Use: /context full | sliding [N] [M] [CAP] | reset | dump [on|off]"
                ),
                error: None,
                append_body: None,
            }),
        }
    }
}

/// Handle `/context dump [on|off]` — toggle the wire-payload debug dump.
///
/// When enabled, every LLM call in this topic appends one JSON line to
/// `<topic>/.jyc/wire-payload.jsonl` (capped at 50 lines, oldest dropped).
/// No args shows the current state and the dump file path.
async fn handle_dump(jyc_dir: &std::path::Path, arg: Option<&str>) -> Result<CommandResult> {
    let flag_path = jyc_dir.join(crate::session_state::WIRE_PAYLOAD_DUMP_FLAG_FILE);
    let dump_path = jyc_dir.join(crate::session_state::WIRE_PAYLOAD_DUMP_FILE);
    let enabled = tokio::fs::read(&flag_path)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("enabled").and_then(|e| e.as_bool()))
        .unwrap_or(false);

    match arg.map(|s| s.to_lowercase()).as_deref() {
        Some("on") => {
            tokio::fs::create_dir_all(jyc_dir).await?;
            tokio::fs::write(&flag_path, r#"{"enabled":true}"#).await?;
            Ok(CommandResult {
                success: true,
                message: format!(
                    "/context: wire-payload dump enabled — writing to {}",
                    dump_path.display()
                ),
                error: None,
                append_body: None,
            })
        }
        Some("off") => {
            tokio::fs::remove_file(&flag_path).await.ok();
            Ok(CommandResult {
                success: true,
                message: "/context: wire-payload dump disabled".into(),
                error: None,
                append_body: None,
            })
        }
        _ => Ok(CommandResult {
            success: true,
            message: format!(
                "/context: wire-payload dump is {} (file: {})",
                if enabled { "on" } else { "off" },
                dump_path.display()
            ),
            error: None,
            append_body: None,
        }),
    }
}

async fn write_override(
    jyc_dir: &std::path::Path,
    path: &std::path::Path,
    cfg: ContextStrategyConfig,
) -> Result<()> {
    tokio::fs::create_dir_all(jyc_dir).await?;
    let json = serde_json::to_string(&cfg)?;
    tokio::fs::write(path, json).await?;
    Ok(())
}

fn describe_strategy(cfg: &ContextStrategyConfig) -> String {
    match cfg.mode {
        ContextStrategy::Full => "full".to_string(),
        ContextStrategy::SlidingWindow => {
            let mut s = format!("sliding_window (window={}", cfg.window);
            if let Some(m) = cfg.note_window {
                s.push_str(&format!(", note_window={m}"));
            }
            if let Some(c) = cfg.tool_result_cap {
                s.push_str(&format!(", tool_result_cap={c}"));
            }
            s.push(')');
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template_dirs::TemplateDirs;
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
        // Default strategy is now sliding_window (per #656).
        assert!(result.message.contains("sliding_window"));
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
        // full preserves the configured window (default 15) so toggling
        // full → sliding later still has a sensible default.
        assert_eq!(v["window"], 15);
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
    async fn test_sliding_with_note_window() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "10".into(), "3".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("window=10"));
        assert!(result.message.contains("note_window=3"));
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["window"], 10);
        assert_eq!(v["note_window"], 3);
    }

    #[tokio::test]
    async fn test_sliding_note_window_zero_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "10".into(), "0".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["note_window"], 0);
    }

    #[tokio::test]
    async fn test_sliding_note_window_invalid_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "10".into(), "abc".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("invalid note window"));
    }

    #[tokio::test]
    async fn test_sliding_without_note_window_uses_default_5() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "7".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        // No explicit note_window arg → falls back to the configured
        // default (`Some(5)`, per #656). The field is now always
        // present in the override file unless the user explicitly
        // sets it to null.
        assert_eq!(v["note_window"], 5);
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
        // No explicit window → falls back to the configured default (15).
        assert_eq!(v["window"], 15);
    }

    #[tokio::test]
    async fn test_sliding_window_rejects_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "0".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("window must be between"));
    }

    #[tokio::test]
    async fn test_sliding_window_rejects_above_max() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "9999".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("window must be between"));
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

    #[tokio::test]
    async fn test_dump_off_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["dump".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("off"));
        assert!(result.message.contains("wire-payload.jsonl"));
    }

    #[tokio::test]
    async fn test_dump_on_off_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let flag = tmp.path().join(".jyc").join("wire-payload-dump.json");

        // /context dump on → flag file written, message mentions enabled
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["dump".into(), "on".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("enabled"));
        assert!(flag.exists());
        assert_eq!(
            tokio::fs::read_to_string(&flag).await.unwrap(),
            r#"{"enabled":true}"#
        );

        // /context dump (no args) → reports on
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["dump".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("on"));

        // /context dump off → flag file removed
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["dump".into(), "off".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("disabled"));
        assert!(!flag.exists());

        // /context dump (no args) → reports off
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["dump".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("off"));
    }

    #[tokio::test]
    async fn test_sliding_with_tool_result_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "10".into(), "3".into(), "5000".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("tool_result_cap=5000"));
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["window"], 10);
        assert_eq!(v["note_window"], 3);
        assert_eq!(v["tool_result_cap"], 5000);
    }

    #[tokio::test]
    async fn test_sliding_cap_zero_is_explicit_off() {
        // Some(0) is the "explicit off" sentinel — must round-trip as 0,
        // not be confused with the configured default (None).
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "10".into(), "3".into(), "0".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["tool_result_cap"], 0);
    }

    #[tokio::test]
    async fn test_sliding_cap_above_max_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "3".into(),
            (MAX_TOOL_RESULT_CAP + 1).to_string(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("tool_result_cap must be between"));
    }

    #[tokio::test]
    async fn test_sliding_cap_invalid_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "10".into(), "3".into(), "abc".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("invalid tool_result_cap"));
    }

    #[tokio::test]
    async fn test_sliding_without_tool_result_cap_uses_default_2048() {
        // No CAP arg → falls back to the configured default
        // (`Some(2048)`, per #656).
        let tmp = tempfile::tempdir().unwrap();
        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["sliding".into(), "15".into(), "5".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["window"], 15);
        assert_eq!(v["note_window"], 5);
        assert_eq!(v["tool_result_cap"], 2048);
    }

    #[tokio::test]
    async fn test_full_uses_configured_tool_result_cap_not_prior_override() {
        // The `full` arm reads tool_result_cap from the configured (config.toml)
        // source, NOT from the prior override. So pre-setting an override with
        // a cap does not "leak" into the new full-mode override.
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("context-strategy.json"),
            r#"{"mode":"sliding_window","window":10,"tool_result_cap":8192}"#,
        )
        .await
        .unwrap();

        let handler = ContextCommandHandler;
        let mut ctx = test_context(tmp.path());
        ctx.args = vec!["full".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(&jyc_dir.join("context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["mode"], "full");
        // `/context full` preserves the configured tool_result_cap
        // (default `Some(2048)`, per #656).
        assert_eq!(v["tool_result_cap"], 2048);
    }

    #[tokio::test]
    async fn test_describe_strategy_includes_tool_result_cap() {
        // describe_strategy is also used by /context (no args) — make sure
        // it renders the cap when set.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            note_window: Some(3),
            tool_result_cap: Some(5000),
        };
        let s = describe_strategy(&cfg);
        assert!(s.contains("window=10"));
        assert!(s.contains("note_window=3"));
        assert!(s.contains("tool_result_cap=5000"));
    }
}
