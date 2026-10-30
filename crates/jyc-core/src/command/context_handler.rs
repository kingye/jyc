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
///   /context sliding [N] [EXT] [TURNCAP] [MAXHIST] [CHECKPOINT] [GROWTH]
///     Switch to sliding window. Parameters are positional and all
///     optional:
///
///       slot        type    accepted range           default
///       N           int     1..=200                  10
///       EXT         float   0.0..=10.0               0.5
///       TURNCAP     int     0, or 130..=1048576      0 (off)
///       MAXHIST     int     0..=4000000              0 (off)
///       CHECKPOINT  int     0 (off), else any        32
///       GROWTH      int     0 (fixed), else any      8
///
///     N is the window's floor in turns: it keeps N turns after a slide.
///     EXT is the extension rate: the window grows from N up to
///     N + round(EXT × N) turns before it slides back to N (0 makes N a
///     hard window that slides every turn).
///     TURNCAP is the per-tool-result byte cap on the current turn; an
///     oversized result keeps its head and tail with the middle replaced
///     by a paging marker. A non-zero value below `MIN_EDGE_KEEPING_CAP`
///     cannot hold the marker and both ends of an ASCII result — a
///     configured value is lifted to the floor, a command-line value is
///     refused here.
///     MAXHIST is the token budget for the windowed history: when the
///     window has reached its turn bound and estimates over it, the window
///     slides back to N turns — the same slide the turn bound performs.
///     CHECKPOINT is how many completed assistant messages the current
///     turn may reach before its older steps fold into a text-only
///     checkpoint. It counts assistant messages — steps that produced
///     output, tool call or reply — not turns or tool calls, and 0
///     disables in-turn folding so the whole current turn rides verbatim.
///     Raise it when a long read-heavy turn keeps re-reading what it just
///     read; lower it when a long turn's verbatim replay costs too much.
///     GROWTH is how slowly that interval widens as the turn folds: the
///     first fold still fires at CHECKPOINT, then each fold adds
///     CHECKPOINT / GROWTH to the next interval (32 and 8 give 32, 36,
///     40 …), so a long turn keeps a growing verbatim tail. 0 keeps the
///     interval fixed. A small non-zero value widens fast — 1 adds a full
///     interval per fold (32, 64, 96 …).
///
///     The slots moved when `note_window` was removed (it was the old 4th
///     slot, between EXT and TURNCAP), so a command carrying more
///     positionals than the table above is refused rather than silently
///     reinterpreted.
///
///     Per-slot semantics: an absent slot falls back to the *configured*
///     value (the config.toml chain) — not to whatever an earlier
///     `/context` call left in the runtime override, so changing one slot
///     does not preserve the others you set before it.
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

/// Upper bound on the extension rate accepted from the user via
/// `/context sliding N EXT`. Past 10× the window, a slide is so rare that
/// the window is effectively unbounded — that is `full` mode wearing a
/// sliding-window label, and it should be chosen as such.
const MAX_EXTEND_RATE: f64 = 10.0;
/// Upper bound for the current-turn cap. Past 1 MiB the result is not the
/// problem, so the command refuses larger values.
const MAX_TURN_TOOL_RESULT_CAP: usize = 1024 * 1024;
/// Lower bound for a *non-zero* current-turn cap. Below this,
/// `truncate_text_keeping_edges` cannot keep both ends and degrades to
/// head-only, so the cap would gut the turn instead of trimming it — and a
/// value like `3` is almost always a slot-shifted leftover command. 0 stays
/// the explicit "off" sentinel.
///
/// The floor itself is [`jyc_types::channel::MIN_EDGE_KEEPING_CAP`], shared
/// with the wire builder that lifts a configured value up to the same bound
/// (a config file bypasses this command, so the rule has to be enforced on
/// both paths from one definition).
const MIN_TURN_TOOL_RESULT_CAP: usize = jyc_types::channel::MIN_EDGE_KEEPING_CAP;
/// Upper bound for the history token budget accepted from the user via
/// `/context sliding N EXT TURNCAP MAXHIST`. Past 4M tokens a history
/// window is not what is costing money — the per-result caps are the lever
/// there — so a larger value would only disguise a misconfiguration.
const MAX_HISTORY_TOKENS: usize = 4_000_000;

#[async_trait]
impl CommandHandler for ContextCommandHandler {
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // Defence in depth: the dispatcher refuses commands for a topic closed
        // earlier in the same command block, but both the dump flag and the
        // strategy override create the state dir the close just deleted.
        if jyc_types::state_dir::is_closed(&context.topic_name) {
            return Ok(CommandResult::refusal(
                "/context: this topic was just closed; nothing to change".to_string(),
            ));
        }
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
                        // Preserve the configured window/caps so
                        // toggling full → sliding later still has sensible
                        // defaults.
                        window: configured.window,
                        extend_rate: configured.extend_rate,
                        turn_tool_result_cap: configured.turn_tool_result_cap,
                        max_history_tokens: configured.max_history_tokens,
                        checkpoint_interval: configured.checkpoint_interval,
                        checkpoint_growth: configured.checkpoint_growth,
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
                // The slots moved when `note_window` was removed (the old
                // 4th slot). A command copied from an older build can
                // therefore carry one trailing argument too many; refuse it
                // rather than silently dropping it.
                if let Some(extra) = context.args.get(7) {
                    return Ok(CommandResult {
                        success: false,
                        message: format!(
                            "/context: unexpected argument '{extra}' (usage: /context sliding [N] [EXT] [TURNCAP] [MAXHIST] [CHECKPOINT] [GROWTH])"
                        ),
                        error: None,
                        append_body: None,
                    });
                }
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
                // Optional extension rate EXT (args[2]): the window grows
                // from N up to N + round(EXT × N) turns before it slides
                // back to N. 0 is valid (a hard window that slides every
                // turn); absent = keep the configured value.
                let extend_rate = if let Some(arg) = context.args.get(2) {
                    match arg.parse::<f64>() {
                        Ok(rate) if (0.0..=MAX_EXTEND_RATE).contains(&rate) => Some(rate),
                        Ok(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: extend rate must be between 0 and {MAX_EXTEND_RATE}"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                        Err(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: invalid extend rate '{arg}', expected a non-negative number (e.g. 0.5)"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                    }
                } else {
                    None
                };
                let extend_rate = extend_rate.unwrap_or(configured.extend_rate);
                // Optional current-turn tool-result byte cap TURNCAP
                // (args[3]): bounds the results the model is still
                // reasoning over, which are uncapped by default. 0 is the
                // explicit "off" sentinel; absent = keep the configured
                // value, which for a topic that never set it is None.
                let turn_tool_result_cap = if let Some(arg) = context.args.get(3) {
                    match arg.parse::<usize>() {
                        Ok(c)
                            if c == 0
                                || (MIN_TURN_TOOL_RESULT_CAP..=MAX_TURN_TOOL_RESULT_CAP)
                                    .contains(&c) =>
                        {
                            Some(Some(c))
                        }
                        Ok(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: turn_tool_result_cap must be 0 (off) or between {MIN_TURN_TOOL_RESULT_CAP} and {MAX_TURN_TOOL_RESULT_CAP} bytes"
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
                // Optional history token budget MAXHIST (args[4]): bounds
                // the windowed history by weight rather than by turn count, dropping
                // the oldest history first. 0 is the explicit "off"
                // sentinel (same convention as CAP); absent = keep the
                // configured value, which for a topic that never set one is
                // None.
                let max_history_tokens = if let Some(arg) = context.args.get(4) {
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
                // Optional current-turn checkpoint interval CHECKPOINT
                // (args[5]): completed assistant messages before the turn's
                // older steps fold to a text-only checkpoint. 0 is the
                // explicit "off" sentinel (same convention as CAP and
                // MAXHIST) and leaves the whole turn verbatim; absent = keep
                // the configured value, which defaults to
                // `DEFAULT_CHECKPOINT_INTERVAL`.
                let checkpoint_interval = if let Some(arg) = context.args.get(5) {
                    match arg.parse::<usize>() {
                        Ok(n) => Some(n),
                        Err(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: invalid checkpoint_interval '{arg}', expected a non-negative integer"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                    }
                } else {
                    None
                };
                let checkpoint_interval =
                    checkpoint_interval.unwrap_or(configured.checkpoint_interval);
                // Optional checkpoint widening GROWTH (args[6]): folds per
                // widening step, so the next interval is CHECKPOINT /
                // GROWTH wider than this one. 0 is the explicit "fixed"
                // sentinel — the behaviour before the slot existed — and
                // absent = keep the configured value, which defaults to
                // `DEFAULT_CHECKPOINT_GROWTH`.
                let checkpoint_growth = if let Some(arg) = context.args.get(6) {
                    match arg.parse::<usize>() {
                        Ok(g) => Some(g),
                        Err(_) => {
                            return Ok(CommandResult {
                                success: false,
                                message: format!(
                                    "/context: invalid checkpoint_growth '{arg}', expected a non-negative integer"
                                ),
                                error: None,
                                append_body: None,
                            });
                        }
                    }
                } else {
                    None
                };
                let checkpoint_growth = checkpoint_growth.unwrap_or(configured.checkpoint_growth);
                let cfg = ContextStrategyConfig {
                    mode: ContextStrategy::SlidingWindow,
                    window,
                    extend_rate,
                    turn_tool_result_cap,
                    max_history_tokens,
                    checkpoint_interval,
                    checkpoint_growth,
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
                    "/context: unknown argument '{other}'. Use: /context sliding [N] [EXT] [TURNCAP] [MAXHIST] [CHECKPOINT] [GROWTH] | full | reset | dump [on|off]"
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
            s.push_str(&format!(
                ", extend_rate={} (max_window={})",
                cfg.extend_rate,
                cfg.max_window()
            ));
            if let Some(c) = cfg.turn_tool_result_cap {
                // Show what is actually applied: a configured value below
                // the edge-keeping floor is lifted by the wire builder, so
                // echoing the raw number would mislead about the turn the
                // model receives. The rule (0 = off, non-zero lifted) comes
                // from `jyc_types`, the same function the builder calls.
                let effective = jyc_types::channel::effective_turn_cap(Some(c));
                if effective == c {
                    s.push_str(&format!(", turn_tool_result_cap={c}"));
                } else {
                    s.push_str(&format!(
                        ", turn_tool_result_cap={effective} (lifted from {c})"
                    ));
                }
            }
            if let Some(t) = cfg.max_history_tokens {
                s.push_str(&format!(", max_history_tokens={t}"));
            }
            // Always echoed: unlike the two caps above it has a non-off
            // default, so "absent from the echo" would be ambiguous.
            if cfg.checkpoint_interval == 0 {
                s.push_str(", checkpoint_interval=0 (off)");
            } else {
                s.push_str(&format!(
                    ", checkpoint_interval={}",
                    cfg.checkpoint_interval
                ));
                // Only meaningful while folding is on, so it is echoed only
                // then: `growth=8` next to `interval=0 (off)` would suggest a
                // widening that never happens.
                if cfg.checkpoint_growth == 0 {
                    s.push_str(", checkpoint_growth=0 (fixed)");
                } else {
                    s.push_str(&format!(", checkpoint_growth={}", cfg.checkpoint_growth));
                }
            }
            s.push(')');
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::channel::{DEFAULT_CHECKPOINT_GROWTH, DEFAULT_CHECKPOINT_INTERVAL};
    use std::path::Path;

    fn test_context(topic_name: &str, topic_path: &Path) -> CommandContext {
        jyc_types::state_dir::register(topic_name, &topic_path.join(".jyc"));
        let mut context = crate::test_support::test_context(topic_name, topic_path);
        context.channel_type = "email".into();
        context
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
        ctx.args = vec!["sliding".into(), "10".into(), "0.5".into(), "32768".into()];
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
        assert_eq!(v["extend_rate"], 0.5);
        assert_eq!(v["turn_tool_result_cap"], 32768);
    }

    #[tokio::test]
    async fn test_sliding_rejects_extra_positional() {
        // The slots moved when `note_window` was removed; a command copied
        // from an older build carries one argument too many.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_rejects_extra_positional";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "0.5".into(),
            "8192".into(),
            "60000".into(),
            "3".into(),
            "7".into(),
            "9".into(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("unexpected argument '9'"));
        assert!(!tmp.path().join(".jyc/context-strategy.json").exists());
    }

    #[tokio::test]
    async fn test_sliding_turn_cap_below_minimum_rejected() {
        // `sliding 10 0.5 3` is a slot-shifted leftover; a 3-byte cap would
        // gut every current-turn tool result, so it is refused.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_turn_cap_below_minimum";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "10".into(), "0.5".into(), "3".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        // Built from the constant: asserting a literal 130 here is what let
        // the message and the bound drift apart when the floor moved.
        assert!(
            result.message.contains(&format!(
                "must be 0 (off) or between {MIN_TURN_TOOL_RESULT_CAP}"
            )),
            "message: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn test_sliding_turn_cap_zero_is_explicit_off() {
        // `/context sliding 10 0.5 0` — the user's way of switching
        // current-turn truncation off. Must persist as 0, and re-reading
        // must keep it at 0 rather than snapping back to the default.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_turn_cap_zero_is_explicit_off";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "10".into(), "0.5".into(), "0".into()];
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
        // `test_full_keeps_the_configured_sliding_settings`).
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
             turn_tool_result_cap = 16384\n",
        )
        .await
        .unwrap();

        let handler = ContextCommandHandler;
        // First call writes an override whose turn cap differs from the
        // configured one, so the fallback below cannot pass by accident.
        let mut set = test_context(topic, tmp.path());
        set.args = vec!["sliding".into(), "10".into(), "0.5".into(), "4096".into()];
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
            ctx.args = vec!["sliding".into(), "10".into(), "0.5".into(), bad.into()];
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
            "0.5".into(),
            (MAX_TURN_TOOL_RESULT_CAP + 1).to_string(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_sliding_with_extend_rate() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_with_extend_rate";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "10".into(), "1.0".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        assert!(
            result.message.contains("extend_rate=1"),
            "message: {}",
            result.message
        );
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["window"], 10);
        assert_eq!(v["extend_rate"], 1.0);
    }

    #[tokio::test]
    async fn test_sliding_extend_rate_zero_is_a_hard_window() {
        // 0 is a valid rate (slide every turn), not an "off" sentinel.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_extend_rate_zero";
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
        assert_eq!(v["extend_rate"], 0.0);
        assert_eq!(v["window"], 10);
    }

    #[tokio::test]
    async fn test_sliding_extend_rate_above_max_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_extend_rate_above_max_rejected";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            (MAX_EXTEND_RATE + 1.0).to_string(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("extend rate must be between"));
    }

    #[tokio::test]
    async fn test_sliding_extend_rate_invalid_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_extend_rate_invalid_arg";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "10".into(), "abc".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("invalid extend rate"));
    }

    #[tokio::test]
    async fn test_sliding_omitted_slots_use_configured_defaults() {
        // No EXT slot -> falls back to the configured defaults
        // (`extend_rate = 0.5`).
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_omitted_slots_use_configured_defaults";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "10".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["window"], 10);
        assert_eq!(v["extend_rate"], 0.5);
    }

    #[tokio::test]
    async fn test_full_keeps_the_configured_sliding_settings() {
        // The `full` arm reads the sliding knobs from the configured
        // (config.toml) source, NOT from the prior override. So
        // pre-setting an override with an extend_rate does not "leak" into
        // the new full-mode override.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_full_keeps_the_configured_sliding_settings";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        tokio::fs::write(
            state_dir.join("context-strategy.json"),
            r#"{"mode":"sliding_window","window":10,"extend_rate":3.0}"#,
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
        // `/context full` preserves the configured extend_rate (0.5 by
        // default), not the override's 3.0.
        assert_eq!(v["extend_rate"], 0.5);
    }

    #[tokio::test]
    async fn test_describe_strategy_includes_extend_rate() {
        // describe_strategy is also used by /context (no args) - make sure
        // it renders the rate and the bound it derives.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            extend_rate: 1.0,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let s = describe_strategy(&cfg);
        assert!(s.contains("window=10"));
        assert!(s.contains("extend_rate=1"), "{s}");
        assert!(s.contains("max_window=20"), "{s}");
    }

    #[tokio::test]
    async fn test_sliding_with_checkpoint_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_with_checkpoint_interval";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "0.5".into(),
            "0".into(),
            "0".into(),
            "16".into(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success, "message: {}", result.message);
        assert!(
            result.message.contains("checkpoint_interval=16"),
            "message: {}",
            result.message
        );
        let v: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["checkpoint_interval"], 16);
    }

    #[tokio::test]
    async fn test_sliding_checkpoint_interval_off_and_default() {
        // 0 is the explicit "off" sentinel; omitting the slot keeps the
        // configured value, which defaults to `DEFAULT_CHECKPOINT_INTERVAL`.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_checkpoint_interval_off_and_default";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "0.5".into(),
            "0".into(),
            "0".into(),
            "0".into(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success, "message: {}", result.message);
        assert!(
            result.message.contains("checkpoint_interval=0 (off)"),
            "message: {}",
            result.message
        );

        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "10".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success, "message: {}", result.message);
        assert!(
            result.message.contains(&format!(
                "checkpoint_interval={DEFAULT_CHECKPOINT_INTERVAL}"
            )),
            "message: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn test_sliding_checkpoint_interval_invalid_arg() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_checkpoint_interval_invalid_arg";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "0.5".into(),
            "0".into(),
            "0".into(),
            "abc".into(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(
            result.message.contains("invalid checkpoint_interval 'abc'"),
            "message: {}",
            result.message
        );
        assert!(!tmp.path().join(".jyc/context-strategy.json").exists());
    }

    #[tokio::test]
    async fn test_sliding_checkpoint_growth_slot() {
        // GROWTH is the sixth positional: 0 keeps the interval fixed, an
        // absent slot keeps the configured value, and a non-zero value widens
        // each checkpoint by CHECKPOINT / GROWTH.
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_checkpoint_growth_slot";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;

        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "0.5".into(),
            "0".into(),
            "0".into(),
            "32".into(),
            "0".into(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success, "message: {}", result.message);
        assert!(
            result.message.contains("checkpoint_growth=0 (fixed)"),
            "message: {}",
            result.message
        );
        let v: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["checkpoint_growth"], 0);

        // Absent slot → the configured value, which defaults to
        // `DEFAULT_CHECKPOINT_GROWTH`.
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec!["sliding".into(), "10".into()];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success, "message: {}", result.message);
        assert!(
            result
                .message
                .contains(&format!("checkpoint_growth={DEFAULT_CHECKPOINT_GROWTH}")),
            "message: {}",
            result.message
        );
        let v: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(tmp.path().join(".jyc/context-strategy.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["checkpoint_growth"], DEFAULT_CHECKPOINT_GROWTH);
    }

    #[tokio::test]
    async fn test_sliding_checkpoint_growth_invalid_and_hidden_when_off() {
        let tmp = tempfile::tempdir().unwrap();
        let topic = "test_sliding_checkpoint_growth_invalid_and_hidden_when_off";
        jyc_types::state_dir::register(topic, &tmp.path().join(".jyc"));
        let handler = ContextCommandHandler;

        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "0.5".into(),
            "0".into(),
            "0".into(),
            "32".into(),
            "abc".into(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(
            result.message.contains("invalid checkpoint_growth 'abc'"),
            "message: {}",
            result.message
        );
        assert!(!tmp.path().join(".jyc/context-strategy.json").exists());

        // With folding off the widening never happens, so the echo must not
        // imply that it does.
        let mut ctx = test_context(topic, tmp.path());
        ctx.args = vec![
            "sliding".into(),
            "10".into(),
            "0.5".into(),
            "0".into(),
            "0".into(),
            "0".into(),
        ];
        let result = handler.execute(ctx).await.unwrap();
        assert!(result.success, "message: {}", result.message);
        assert!(
            result.message.contains("checkpoint_interval=0 (off)"),
            "message: {}",
            result.message
        );
        assert!(
            !result.message.contains("checkpoint_growth"),
            "message: {}",
            result.message
        );
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
            "0.5".into(),
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
            "0.5".into(),
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
            "0.5".into(),
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
            "0.5".into(),
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
                "0.5".into(),
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
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: Some(30000),
            ..ContextStrategyConfig::default()
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

    /// The echo reports the cap actually applied and names the configured
    /// value it came from — a config file never passes through the command,
    /// so its `turn_tool_result_cap = 3` is lifted by the wire builder, and
    /// echoing `3` would describe a turn the model never receives.
    #[test]
    fn describe_strategy_shows_the_lifted_turn_cap() {
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            extend_rate: 0.5,
            turn_tool_result_cap: Some(3),
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        assert!(
            describe_strategy(&cfg).contains(&format!(
                "turn_tool_result_cap={} (lifted from 3)",
                jyc_types::channel::MIN_EDGE_KEEPING_CAP
            )),
            "{}",
            describe_strategy(&cfg)
        );

        // 0 is the off sentinel, not something to lift.
        let off = ContextStrategyConfig {
            turn_tool_result_cap: Some(0),
            ..cfg.clone()
        };
        assert!(describe_strategy(&off).contains("turn_tool_result_cap=0"));
        assert!(!describe_strategy(&off).contains("lifted"));

        // A value at or above the floor is echoed as configured.
        let big = ContextStrategyConfig {
            turn_tool_result_cap: Some(32768),
            ..cfg.clone()
        };
        assert!(describe_strategy(&big).contains("turn_tool_result_cap=32768"));
        assert!(!describe_strategy(&big).contains("lifted"));
    }

    /// The checkpoint interval is always echoed, unlike the two caps above it:
    /// it has a non-off default, so its absence from the echo would read as
    /// "not configured" when it is in fact 32. `0` is the off sentinel and
    /// says so.
    #[test]
    fn describe_strategy_always_shows_the_checkpoint_interval() {
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        assert!(
            describe_strategy(&cfg).contains(&format!(
                "checkpoint_interval={DEFAULT_CHECKPOINT_INTERVAL}"
            )),
            "{}",
            describe_strategy(&cfg)
        );

        let off = ContextStrategyConfig {
            checkpoint_interval: 0,
            ..cfg.clone()
        };
        assert!(
            describe_strategy(&off).contains("checkpoint_interval=0 (off)"),
            "{}",
            describe_strategy(&off)
        );

        let custom = ContextStrategyConfig {
            checkpoint_interval: 64,
            ..cfg
        };
        assert!(
            describe_strategy(&custom).contains("checkpoint_interval=64"),
            "{}",
            describe_strategy(&custom)
        );
    }
}
