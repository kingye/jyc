use anyhow::Result;
use async_trait::async_trait;
use jyc_types::state_dir::resolve_state_dir;

use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// `/context` command — view or change the context management strategy for
/// this topic. The on-disk `.jyc/agent-context.jsonl` always stores the
/// full raw context; this command only changes what is sent to the LLM.
///
/// Usage:
///   /context              Show current strategy
///   /context full         Switch to full context (send everything)
///   /context sliding [N] [M] [CAP] [TURNCAP] [MAXHIST]
///     Switch to sliding window (default N = 10 turns). M is the optional
///     note window: only the most recent M windowed turns carry tool-call
///     history notes (default: 5). CAP is the optional per-tool-result
///     byte cap on the verbatim prior region (region ②; default: 2048).
///     TURNCAP is the optional per-tool-result byte cap on the current
///     turn (region ③), which CAP deliberately leaves uncapped; an
///     oversized result keeps its head and tail with the middle replaced
///     by a paging marker. Unset by default. MAXHIST is the optional token
///     budget for the two history regions (① + ②): when they estimate over
///     it, the oldest history is dropped first. Unset by default.
///
///     Per-slot semantics: an absent slot falls back to the *configured*
///     value (the config.toml chain) — not to whatever an earlier
///     `/context` call left in the runtime override, so changing one slot
///     does not preserve the others you set before it — and `0`
///     explicitly disables a cap. (M is an ordinary value — 0 there means
///     "text-only window"; MAXHIST follows CAP, so 0 there means "off".)
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
/// Upper bound for the current-turn cap. Same 1 MiB ceiling as
/// `MAX_TOOL_RESULT_CAP` — past that the result is not the problem.
const MAX_TURN_TOOL_RESULT_CAP: usize = 1024 * 1024;
/// Upper bound for the history token budget accepted from the user via
/// `/context sliding N M CAP TURNCAP MAXHIST`. Past 4M tokens a history
/// window is not what is costing money — the per-result caps are the lever
/// there — so a larger value would only disguise a misconfiguration.
const MAX_HISTORY_TOKENS: usize = 4_000_000;

#[async_trait]
impl CommandHandler for ContextCommandHandler {
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let state_dir = resolve_state_dir(&context.topic_name, &context.topic_path);
        let override_path = state_dir.join(crate::session_state::CONTEXT_STRATEGY_FILE);

        let matched_pattern =
            crate::session_state::read_pattern(&context.topic_name, &context.topic_path).await;
        // The topic's own config is a layer of the chain (below the runtime
        // override, above the pattern): parse it once here, the way the turn
        // path hands its parsed table in.
        let topic_cfg = jyc_types::load_topic_config(&context.topic_name, &context.topic_path);
        let configured = crate::session_state::resolve_context_strategy(
            &context.config,
            &context.channel,
            matched_pattern.as_deref(),
            topic_cfg.as_ref().and_then(|c| c.context_strategy.as_ref()),
        );

        // No args → show current strategy (runtime override takes priority).
        if context.args.is_empty() {
            let runtime = crate::session_state::read_context_strategy_override(
                &context.topic_name,
                &context.topic_path,
            )
            .await;
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
                    &state_dir,
                    &override_path,
                    ContextStrategyConfig {
                        mode: ContextStrategy::Full,
                        // Preserve the configured window/note_window/caps so
                        // toggling full → sliding later still has sensible
                        // defaults.
                        window: configured.window,
                        note_window: configured.note_window,
                        tool_result_cap: configured.tool_result_cap,
                        turn_tool_result_cap: configured.turn_tool_result_cap,
                        max_history_tokens: configured.max_history_tokens,
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
                // Optional current-turn tool-result byte cap TURNCAP
                // (args[4]): bounds the results the model is still
                // reasoning over, which CAP leaves alone because it is
                // uncapped by default. 0 is the explicit "off" sentinel
                // (same convention as CAP); absent = keep configured
                // value, which for a topic that never set it is None.
                let turn_tool_result_cap = if let Some(arg) = context.args.get(4) {
                    match arg.parse::<usize>() {
                        Ok(c) if c <= MAX_TURN_TOOL_RESULT_CAP => Some(Some(c)),
                        Ok(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: turn_tool_result_cap must be between 0 and {MAX_TURN_TOOL_RESULT_CAP} bytes"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                        Err(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: invalid turn_tool_result_cap '{arg}', expected a non-negative integer"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                    }
                } else {
                    None
                };
                let turn_tool_result_cap =
                    turn_tool_result_cap.unwrap_or(configured.turn_tool_result_cap);
                // Optional history token budget MAXHIST (args[5]): bounds
                // regions ①+② by weight rather than by turn count, dropping
                // the oldest history first. 0 is the explicit "off"
                // sentinel (same convention as CAP); absent = keep the
                // configured value, which for a topic that never set one is
                // None.
                let max_history_tokens = if let Some(arg) = context.args.get(5) {
                    match arg.parse::<usize>() {
                        Ok(t) if t <= MAX_HISTORY_TOKENS => Some(Some(t)),
                        Ok(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: max_history_tokens must be between 0 and {MAX_HISTORY_TOKENS} tokens"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                        Err(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: invalid max_history_tokens '{arg}', expected a non-negative integer"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                    }
                } else {
                    None
                };
                let max_history_tokens =
                    max_history_tokens.unwrap_or(configured.max_history_tokens);
                let cfg = ContextStrategyConfig {
                    mode: ContextStrategy::SlidingWindow,
                    window,
                    note_window,
                    tool_result_cap,
                    turn_tool_result_cap,
                    max_history_tokens,
                };
                write_override(&state_dir, &override_path, cfg.clone()).await?;
                Ok(CommandResult {
                    success: true,
                    message: format!("/context: switched to {}", describe_strategy(&cfg)),
                    error: None,
                    append_body: None,
                })
            }
            "dump" => handle_dump(&state_dir, context.args.get(1).map(|s| s.as_str())).await,
            other => Ok(CommandResult {
                success: false,
                message: format!(
                    "/context: unknown argument '{other}'. Use: /context full | sliding [N] [M] [CAP] [TURNCAP] [MAXHIST] | reset | dump [on|off]"
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
async fn handle_dump(state_dir: &std::path::Path, arg: Option<&str>) -> Result<CommandResult> {
    let flag_path = state_dir.join(crate::session_state::WIRE_PAYLOAD_DUMP_FLAG_FILE);
    let dump_path = state_dir.join(crate::session_state::WIRE_PAYLOAD_DUMP_FILE);
    let enabled = tokio::fs::read(&flag_path)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("enabled").and_then(|e| e.as_bool()))
        .unwrap_or(false);

    match arg.map(|s| s.to_lowercase()).as_deref() {
        Some("on") => {
            tokio::fs::create_dir_all(state_dir).await?;
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
    state_dir: &std::path::Path,
    path: &std::path::Path,
    cfg: ContextStrategyConfig,
) -> Result<()> {
    tokio::fs::create_dir_all(state_dir).await?;
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
            if let Some(c) = cfg.turn_tool_result_cap {
                s.push_str(&format!(", turn_tool_result_cap={c}"));
            }
            if let Some(t) = cfg.max_history_tokens {
                s.push_str(&format!(", max_history_tokens={t}"));
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

    fn test_context(topic_name: &str, topic_path: &Path) -> CommandContext {
        jyc_types::state_dir::register(topic_name, &topic_path.join(".jyc"));
        CommandContext {
            topic_name: topic_name.to_string(),
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
            per_agent_commands: vec![],
            hooks: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_show_default() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_show_default";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let ctx = test_context(topic, tmp.path());
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        // Default strategy is now sliding_window (per #656).
        assert!(result.message.contains("sliding_window"));
        assert!(result.message.contains("default"));
    }

    #[tokio::test]
    async fn test_full_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_full_switch";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["full".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["mode"], "full");
        // full preserves the configured window (default 10) so toggling
        // full → sliding later still has a sensible default.
        assert_eq!(v["window"], 10);
    }

    #[tokio::test]
    async fn test_sliding_with_window() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_with_window";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
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
        let topic = "test_sliding_with_note_window";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
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
        let topic = "test_sliding_note_window_zero_allowed";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
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
        let topic = "test_sliding_note_window_invalid_arg";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "10".into(), "abc".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("invalid note window"));
    }

    #[tokio::test]
    async fn test_sliding_without_note_window_uses_default_5() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_without_note_window_uses_default_5";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "7".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        // No explicit note_window arg → falls back to the configured
        // default (`Some(3)`, per #656). The field is now always
        // present in the override file unless the user explicitly
        // sets it to null.
        assert_eq!(v["note_window"], 3);
    }

    #[tokio::test]
    async fn test_sliding_default_window() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_default_window";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding_window".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["mode"], "sliding_window");
        // No explicit window → falls back to the configured default (10).
        assert_eq!(v["window"], 10);
    }

    #[tokio::test]
    async fn test_sliding_window_rejects_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_window_rejects_zero";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "0".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("window must be between"));
    }

    #[tokio::test]
    async fn test_sliding_window_rejects_above_max() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_window_rejects_above_max";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "9999".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("window must be between"));
    }

    #[tokio::test]
    async fn test_sliding_window_invalid_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_window_invalid_arg";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "abc".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("invalid window"));
    }

    #[tokio::test]
    async fn test_reset_clears_override() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_reset_clears_override";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("context-strategy.json"),
            r#"{"mode":"sliding_window","window":3}"#,
        )
        .await
        .unwrap();

        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["reset".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(!state_dir.join("context-strategy.json").exists());
    }

    #[tokio::test]
    async fn test_show_after_override() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_show_after_override";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("context-strategy.json"),
            r#"{"mode":"sliding_window","window":5}"#,
        )
        .await
        .unwrap();

        let handler = ContextCommandHandler;
        let ctx = test_context(topic, tmp.path());
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("sliding_window"));
        assert!(result.message.contains("window=5"));
        assert!(result.message.contains("override"));
    }

    #[tokio::test]
    async fn test_unknown_subcommand() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_unknown_subcommand";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["bogus".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("unknown argument"));
    }

    #[tokio::test]
    async fn test_dump_off_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_dump_off_by_default";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["dump".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("off"));
        assert!(result.message.contains("wire-payload.jsonl"));
    }

    #[tokio::test]
    async fn test_dump_on_off_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_dump_on_off_roundtrip";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let flag = tmp.path().join(".jyc").join("wire-payload-dump.json");

        // /context dump on → flag file written, message mentions enabled
        let mut ctx = test_context(topic, tmp.path());
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
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["dump".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("on"));

        // /context dump off → flag file removed
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["dump".into(), "off".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("disabled"));
        assert!(!flag.exists());

        // /context dump (no args) → reports off
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["dump".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("off"));
    }

    #[tokio::test]
    async fn test_sliding_with_turn_tool_result_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_with_turn_tool_result_cap";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "3".into(),
            "2048".into(),
            "32768".into(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            result.message.contains("turn_tool_result_cap=32768"),
            "message: {}",
            result.message
        );
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["window"], 10);
        assert_eq!(v["note_window"], 3);
        assert_eq!(v["tool_result_cap"], 2048);
        assert_eq!(v["turn_tool_result_cap"], 32768);
    }

    #[tokio::test]
    async fn test_sliding_turn_cap_zero_is_explicit_off() {
        // `/context sliding 10 5 2048 0` — the user's way of switching
        // current-turn truncation off. Must persist as 0, and re-reading
        // must keep it at 0 rather than snapping back to the default.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_turn_cap_zero_is_explicit_off";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "3".into(),
            "2048".into(),
            "0".into(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["turn_tool_result_cap"], 0);

        // Reading it back shows the off state.
        let mut show = test_context(topic, tmp.path());
        show.args = vec![];
        let shown = handler.execute(show).await.unwrap();
        assert!(
            shown.message.contains("turn_tool_result_cap=0"),
            "{}",
            shown.message
        );
    }

    #[tokio::test]
    async fn test_sliding_turn_cap_omitted_falls_back_to_configured_not_override() {
        // An absent 4th slot falls back to the *configured* value, not to
        // whatever an earlier `/context` call left in the runtime override —
        // the convention `full` already follows (see
        // `test_full_uses_configured_tool_result_cap_not_prior_override`).
        // So a topic configured with 16384 keeps 16384 even while an
        // override holds 4096, and that is the behaviour worth pinning: it
        // is the one a reader would otherwise assume to go the other way.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_turn_cap_omitted_falls_back_to_configured_not_override";
        let state_dir = tmp.path().join(".jyc");
        jyc_types::state_dir::register(topic, &state_dir);
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("config.toml"),
            "[context_strategy]\n\
             mode = \"sliding_window\"\n\
             window = 7\n\
             note_window = 3\n\
             tool_result_cap = 2048\n\
             turn_tool_result_cap = 16384\n",
        )
        .await
        .unwrap();

        let handler = ContextCommandHandler;
        // First call writes an override whose turn cap differs from the
        // configured one, so the fallback below cannot pass by accident.
        let mut set = test_context(topic, tmp.path());
        set.args = vec![
            "sliding".into(),
            "10".into(),
            "3".into(),
            "2048".into(),
            "4096".into(),
        ];
        assert!(handler.execute(set).await.unwrap().success);

        let mut again = test_context(topic, tmp.path());
        again.args = vec!["sliding".into(), "12".into()];
        assert!(handler.execute(again).await.unwrap().success);
        let v: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(state_dir.join("context-strategy.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["window"], 12, "the given slot still applies");
        assert_eq!(
            v["note_window"], 3,
            "note_window is configured, not overridden"
        );
        assert_eq!(
            v["turn_tool_result_cap"], 16384,
            "omitted slot falls back to the configured value, not the prior override"
        );
    }

    #[tokio::test]
    async fn test_sliding_turn_cap_invalid_rejected() {
        for bad in ["abc", "-5"] {
            let tmp = tempfile::tempdir().unwrap();
            let topic = format!("test_sliding_turn_cap_invalid_rejected_{bad}");
            jyc_types::state_dir::register(&topic, &tmp.path().join(".jyc"));
            let handler = ContextCommandHandler;
            let mut ctx = test_context(&topic, tmp.path());
            ctx.args = vec![
                "sliding".into(),
                "10".into(),
                "3".into(),
                "2048".into(),
                bad.into(),
            ];
            let result = handler.execute(ctx).await.unwrap();
            assert!(!result.success, "{bad} must be rejected");
            assert!(
                result.message.contains("turn_tool_result_cap"),
                "error must name the slot: {}",
                result.message
            );
            // A rejected slot must leave no override behind.
            assert!(
                !tmp.path().join(".jyc/context-strategy.json").exists(),
                "rejected input must not write an override"
            );
        }
    }

    #[tokio::test]
    async fn test_sliding_turn_cap_above_max_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_turn_cap_above_max_rejected";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "3".into(),
            "2048".into(),
            (MAX_TURN_TOOL_RESULT_CAP + 1).to_string(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_sliding_with_tool_result_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_with_tool_result_cap";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
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
        let topic = "test_sliding_cap_zero_is_explicit_off";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
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
        let topic = "test_sliding_cap_above_max_rejected";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
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
        let topic = "test_sliding_cap_invalid_arg";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
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
        let topic = "test_sliding_without_tool_result_cap_uses_default_2048";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "10".into(), "3".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["window"], 10);
        assert_eq!(v["note_window"], 3);
        assert_eq!(v["tool_result_cap"], 2048);
    }

    #[tokio::test]
    async fn test_full_uses_configured_tool_result_cap_not_prior_override() {
        // The `full` arm reads tool_result_cap from the configured (config.toml)
        // source, NOT from the prior override. So pre-setting an override with
        // a cap does not "leak" into the new full-mode override.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_full_uses_configured_tool_result_cap_not_prior_override";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("context-strategy.json"),
            r#"{"mode":"sliding_window","window":10,"tool_result_cap":8192}"#,
        )
        .await
        .unwrap();

        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["full".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(&state_dir.join("context-strategy.json"))
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
            turn_tool_result_cap: None,
            max_history_tokens: None,
        };
        let s = describe_strategy(&cfg);
        assert!(s.contains("window=10"));
        assert!(s.contains("note_window=3"));
        assert!(s.contains("tool_result_cap=5000"));
    }

    #[tokio::test]
    async fn test_sliding_with_max_history_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_with_max_history_tokens";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "3".into(),
            "2048".into(),
            "32768".into(),
            "30000".into(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            result.message.contains("max_history_tokens=30000"),
            "message: {}",
            result.message
        );
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["window"], 10);
        assert_eq!(v["turn_tool_result_cap"], 32768);
        assert_eq!(v["max_history_tokens"], 30000);
    }

    #[tokio::test]
    async fn test_sliding_max_history_tokens_zero_is_explicit_off() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_max_history_tokens_zero_is_explicit_off";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "3".into(),
            "2048".into(),
            "32768".into(),
            "0".into(),
        ];
        assert!(handler.execute(ctx).await.unwrap().success);
        let v: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["max_history_tokens"], 0);

        let mut show = test_context(topic, tmp.path());
        show.args = vec![];
        let shown = handler.execute(show).await.unwrap();
        assert!(
            shown.message.contains("max_history_tokens=0"),
            "{}",
            shown.message
        );
    }

    /// An absent 5th slot falls back to the *configured* value, not to
    /// whatever an earlier `/context` call left in the runtime override —
    /// the same convention the other slots follow (#936 pinned it for the
    /// 4th).
    #[tokio::test]
    async fn test_sliding_max_history_tokens_omitted_falls_back_to_configured_not_override() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_max_hist_omitted_falls_back_to_configured";
        let state_dir = tmp.path().join(".jyc");
        jyc_types::state_dir::register(topic, &state_dir);
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("config.toml"),
            "[context_strategy]\n\
             mode = \"sliding_window\"\n\
             window = 7\n\
             max_history_tokens = 50000\n",
        )
        .await
        .unwrap();

        let handler = ContextCommandHandler;
        let mut set = test_context(topic, tmp.path());
        set.args = vec![
            "sliding".into(),
            "10".into(),
            "3".into(),
            "2048".into(),
            "32768".into(),
            "1000".into(),
        ];
        assert!(handler.execute(set).await.unwrap().success);

        // Second call omits the slot: it must come back as the configured
        // 50000, not the 1000 the previous call left behind.
        let mut again = test_context(topic, tmp.path());
        again.args = vec!["sliding".into(), "12".into()];
        assert!(handler.execute(again).await.unwrap().success);
        let v: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(state_dir.join("context-strategy.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["window"], 12, "the given slot still applies");
        assert_eq!(
            v["max_history_tokens"], 50000,
            "omitted slot falls back to the configured value, not the prior override"
        );
    }

    #[tokio::test]
    async fn test_sliding_max_history_tokens_above_max_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_max_history_tokens_above_max_rejected";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "3".into(),
            "2048".into(),
            "32768".into(),
            (MAX_HISTORY_TOKENS + 1).to_string(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(
            result.message.contains("max_history_tokens"),
            "error must name the slot: {}",
            result.message
        );
        assert!(
            !tmp.path().join(".jyc/context-strategy.json").exists(),
            "a rejected slot must leave no override behind"
        );
    }

    #[tokio::test]
    async fn test_sliding_max_history_tokens_invalid_rejected() {
        for bad in ["abc", "-5"] {
            let tmp = tempfile::tempdir().unwrap();
            let topic = format!("test_sliding_max_history_tokens_invalid_{bad}");
            jyc_types::state_dir::register(&topic, &tmp.path().join(".jyc"));
            let handler = ContextCommandHandler;
            let mut ctx = test_context(&topic, tmp.path());
            ctx.args = vec![
                "sliding".into(),
                "10".into(),
                "3".into(),
                "2048".into(),
                "32768".into(),
                bad.into(),
            ];
            let result = handler.execute(ctx).await.unwrap();
            assert!(!result.success, "{bad} must be rejected");
            assert!(
                result.message.contains("max_history_tokens"),
                "error must name the slot: {}",
                result.message
            );
            assert!(
                !tmp.path().join(".jyc/context-strategy.json").exists(),
                "a rejected slot must leave no override behind"
            );
        }
    }

    /// `/context full` keeps the configured budget, so switching back to
    /// sliding later does not silently lose it.
    #[tokio::test]
    async fn test_full_preserves_configured_max_history_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_full_preserves_configured_max_history_tokens";
        let state_dir = tmp.path().join(".jyc");
        jyc_types::state_dir::register(topic, &state_dir);
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("config.toml"),
            "[context_strategy]\nmax_history_tokens = 42000\n",
        )
        .await
        .unwrap();

        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["full".into()];
        assert!(handler.execute(ctx).await.unwrap().success);

        let v: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(state_dir.join("context-strategy.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["mode"], "full");
        assert_eq!(v["max_history_tokens"], 42000);
    }

    #[test]
    fn describe_strategy_shows_max_history_tokens() {
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            note_window: Some(3),
            tool_result_cap: Some(2048),
            turn_tool_result_cap: None,
            max_history_tokens: Some(30000),
        };
        assert!(describe_strategy(&cfg).contains("max_history_tokens=30000"));

        // Unset reads as "off" by absence, exactly like turn_tool_result_cap.
        let unset = ContextStrategyConfig {
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..cfg.clone()
        };
        assert!(!describe_strategy(&unset).contains("max_history_tokens"));
    }
}
