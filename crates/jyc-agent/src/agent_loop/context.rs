//! Raw-context rendering and history compaction helpers.
//!
//! Extracted from the monolithic `agent_loop.rs`.

use std::borrow::Cow;

use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};

use crate::types::{Message, Role};

pub(crate) fn render_raw_context_as_text(raw_context: &[serde_json::Value]) -> String {
    let mut out = String::with_capacity(raw_context.len() * 256);
    out.push_str("=== Conversation transcript ===\n\n");
    for msg in raw_context {
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown");
        match role {
            "user" => {
                let text = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                if !text.is_empty() {
                    out.push_str("USER: ");
                    out.push_str(text);
                    out.push_str("\n\n");
                }
            }
            "assistant" => {
                out.push_str("ASSISTANT");
                // OpenAI: content as string
                if let Some(text) = msg.get("content").and_then(|c| c.as_str())
                    && !text.is_empty()
                {
                    out.push_str(": ");
                    out.push_str(text);
                }
                // Anthropic: content as array of blocks
                if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                    for block in blocks {
                        let t = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match t {
                            "text" => {
                                if let Some(s) = block.get("text").and_then(|x| x.as_str()) {
                                    out.push_str(": ");
                                    out.push_str(s);
                                }
                            }
                            "tool_use" => {
                                let name =
                                    block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                                out.push_str(&format!("\n  [tool_use: {}]", name));
                            }
                            _ => {}
                        }
                    }
                }
                // OpenAI: tool_calls array
                if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("?");
                        out.push_str(&format!("\n  [tool_call: {}]", name));
                    }
                }
                out.push_str("\n\n");
            }
            "tool" => {
                let text = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                let truncated = if text.len() > 500 {
                    format!("{}…", &text[..text.floor_char_boundary(500)])
                } else {
                    text.to_string()
                };
                out.push_str("TOOL_RESULT: ");
                out.push_str(&truncated);
                out.push_str("\n\n");
            }
            _ => {}
        }
    }
    out
}

pub(crate) fn compact_raw_context_heuristic(
    raw_context: &[serde_json::Value],
    keep_pairs: usize,
) -> Vec<serde_json::Value> {
    crate::session::extract_user_assistant_pairs(raw_context, keep_pairs)
}

/// Heuristic compaction of internal history: keep only the last N user+assistant
/// text pairs. Synced with `compact_raw_context_heuristic`.
pub(crate) fn compact_history_heuristic(history: &[Message], keep_pairs: usize) -> Vec<Message> {
    let mut pairs: Vec<(Message, Message)> = Vec::new();
    let mut last_user: Option<Message> = None;

    for msg in history {
        match msg.role {
            Role::User => {
                last_user = Some(msg.clone());
            }
            Role::Assistant => {
                let text = msg.text();
                if !text.is_empty()
                    && let Some(user_msg) = last_user.take()
                {
                    pairs.push((user_msg, Message::assistant(text)));
                }
            }
            _ => {}
        }
    }

    // Keep only the last N pairs
    pairs
        .into_iter()
        .rev()
        .take(keep_pairs)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .flat_map(|(user, assistant)| vec![user, assistant])
        .collect()
}

/// Build the context to send to the LLM for the next request.
///
/// The full `raw_context` is always persisted to `.jyc/agent-context.json`
/// untouched; this function only shapes what is sent to the LLM.
///
/// * `Full` — borrow the full context (no copy).
/// * `SlidingWindow` — keep the last `strategy.window` user+assistant turns
///   from the prior context (everything before `prior_len`), plus the full
///   current turn (`raw_context[prior_len..]`) so tool calls/results stay
///   coherent mid-loop.
pub(crate) fn build_send_context<'a>(
    raw_context: &'a [serde_json::Value],
    prior_len: usize,
    strategy: &ContextStrategyConfig,
) -> Cow<'a, [serde_json::Value]> {
    match strategy.mode {
        ContextStrategy::Full => Cow::Borrowed(raw_context),
        ContextStrategy::SlidingWindow => {
            // Mid-loop compression can shorten `raw_context` below `prior_len`;
            // clamp so the slice math never underflows.
            let boundary = prior_len.min(raw_context.len());
            let prior = &raw_context[..boundary];
            let current = &raw_context[boundary..];
            let mut windowed = crate::session::extract_user_assistant_pairs(prior, strategy.window);
            windowed.extend_from_slice(current);
            Cow::Owned(windowed)
        }
    }
}

#[cfg(test)]
mod render_raw_context_tests {
    use super::*;
    use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};
    use serde_json::json;

    #[test]
    fn renders_user_assistant_tool_sequence() {
        let ctx = vec![
            json!({"role": "user", "content": "fix bug"}),
            json!({
                "role": "assistant",
                "content": "I'll start",
                "reasoning_content": "thinking...",
                "tool_calls": [{
                    "id": "1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "1", "content": "output"}),
            json!({"role": "assistant", "content": "Done."}),
        ];
        let rendered = render_raw_context_as_text(&ctx);
        assert!(rendered.contains("USER: fix bug"));
        assert!(rendered.contains("ASSISTANT: I'll start"));
        assert!(rendered.contains("[tool_call: bash]"));
        assert!(rendered.contains("TOOL_RESULT: output"));
        assert!(rendered.contains("ASSISTANT: Done."));
    }

    #[test]
    fn truncates_long_tool_results() {
        let long = "x".repeat(2000);
        let ctx = vec![json!({"role": "tool", "tool_call_id": "1", "content": long})];
        let rendered = render_raw_context_as_text(&ctx);
        // Truncation cap is 500 + "…", plus the "TOOL_RESULT: " prefix and trailing newlines.
        assert!(rendered.len() < 700);
        assert!(rendered.contains("…"));
    }

    #[test]
    fn skips_unknown_roles_and_empty_content() {
        let ctx = vec![
            json!({"role": "system", "content": "ignored"}),
            json!({"role": "user", "content": ""}),
            json!({"role": "user", "content": "real"}),
        ];
        let rendered = render_raw_context_as_text(&ctx);
        assert!(!rendered.contains("ignored"));
        assert!(rendered.contains("USER: real"));
    }

    #[test]
    fn full_strategy_borrows_unchanged() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
        ];
        let prior_len = 2;
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::Full,
            window: 10,
        };
        let sent = build_send_context(&ctx, prior_len, &cfg);
        assert!(matches!(sent, Cow::Borrowed(_)));
        assert_eq!(sent.len(), ctx.len());
    }

    #[test]
    fn sliding_window_keeps_last_pairs_and_current_turn() {
        // Prior: 3 user+assistant turns, plus 2 tool messages.
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "out"}),
            json!({"role": "user", "content": "u3"}),
            json!({"role": "assistant", "content": "a3"}),
        ];
        let current = vec![
            json!({"role": "user", "content": "u4"}),
            json!({"role": "assistant", "content": "a4", "tool_calls": [{"id":"2","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "2", "content": "out2"}),
        ];
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.extend(current.clone());

        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 2,
        };
        let sent = build_send_context(&ctx, prior_len, &cfg);
        assert!(matches!(sent, Cow::Owned(_)));
        let sent = sent.into_owned();

        // Last 2 user+assistant text pairs from prior → {u2,a2_clean,u3,a3}.
        // Tool role from prior is dropped. Then current turn is appended
        // verbatim so tool calls/results remain coherent.
        let expected: Vec<serde_json::Value> = vec![
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
            json!({"role": "user", "content": "u3"}),
            json!({"role": "assistant", "content": "a3"}),
            current[0].clone(),
            current[1].clone(),
            current[2].clone(),
        ];
        assert_eq!(sent, expected);
    }

    #[test]
    fn sliding_window_prior_shorter_than_raw_context() {
        // After mid-loop compression, prior_len could exceed raw_context.len().
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
        ];
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
        };
        let sent = build_send_context(&ctx, 99, &cfg).into_owned();
        // Clamps boundary → whole ctx treated as prior, windowed.
        assert_eq!(sent.len(), 2);
    }

    #[test]
    fn sliding_window_empty_prior_returns_only_current() {
        let ctx = vec![
            json!({"role": "user", "content": "current"}),
            json!({"role": "assistant", "content": "reply"}),
        ];
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
        };
        let sent = build_send_context(&ctx, 0, &cfg).into_owned();
        assert_eq!(sent, ctx);
    }
}
