//! Raw-context rendering and history compaction helpers.
//!
//! Extracted from the monolithic `agent_loop.rs`.

use std::borrow::Cow;

use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};

use crate::provider::Provider;
use crate::types::{ContentBlock, Message, Role};

/// Render the raw context as a plain-text transcript for one-shot
/// summarization (cycle-boundary progress updates). Thin wrapper over
/// `session::render_raw_context_as_text` — single implementation shared
/// with the sliding-window view.
pub(crate) fn render_raw_context_as_text(raw_context: &[serde_json::Value]) -> String {
    crate::session::render_raw_context_as_text(raw_context)
}

pub(crate) fn compact_raw_context_heuristic(
    raw_context: &[serde_json::Value],
    keep_pairs: usize,
) -> Vec<serde_json::Value> {
    // Compaction keeps notes on every pair (`None`); `note_window` only
    // shapes the sliding-window wire payload.
    crate::session::extract_user_assistant_pairs(raw_context, keep_pairs, None)
}

/// Heuristic compaction of internal history: keep only the last N user+assistant
/// text pairs. Synced with `compact_raw_context_heuristic`.
///
/// INTENTIONALLY divergent from the raw-context path: this view keeps plain
/// text only (no folded tool-call annotations) and drops tool-call-only
/// turns entirely, because internal history is used for reply detection and
/// text extraction — not for model grounding. Do not "fix" the difference
/// by porting annotations here; the raw path (`extract_pairs`) owns the
/// rich view.
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

/// Reformat a cleaned message (string content from `extract_user_assistant_pairs`)
/// into the active provider's wire format via the Provider trait. This is
/// what makes sliding-window output valid for Anthropic, which requires
/// user/assistant `content` to be an array of blocks. Returns `None` when
/// the message has no extractable text, so callers must never emit empty
/// text blocks (Anthropic rejects them).
fn format_cleaned_message(
    provider: &dyn Provider,
    msg: &serde_json::Value,
) -> Option<serde_json::Value> {
    let text = crate::session::extract_message_text(msg);
    if text.is_empty() {
        return None;
    }
    Some(
        if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            provider.build_raw_assistant_message(&text, "", &[])
        } else {
            provider.format_user_message(&[ContentBlock::Text { text }])
        },
    )
}

/// Build the context to send to the LLM for the next request.
///
/// The full `raw_context` is always persisted to `.jyc/agent-context.json`
/// untouched; this function only shapes what is sent to the LLM.
///
/// * `Full` — borrow the full context (no copy).
/// * `SlidingWindow` — two parts, in order:
///   1. The last `strategy.window` user+assistant text pairs from the prior
///      context, reformatted in provider wire format (string content →
///      provider-correct content shape).
///   2. The full current turn (`raw_context[prior_len..]`) verbatim, so
///      tool calls / results stay coherent mid-loop.
pub(crate) fn build_send_context<'a>(
    provider: &dyn Provider,
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

            let mut out = Vec::new();
            let windowed = crate::session::extract_user_assistant_pairs(
                prior,
                strategy.window,
                strategy.note_window,
            );
            for msg in &windowed {
                // Defensive: skip messages with no extractable text so the
                // wire payload never contains empty text blocks (which
                // Anthropic rejects, especially under cache_control).
                if let Some(formatted) = format_cleaned_message(provider, msg) {
                    out.push(formatted);
                }
            }

            out.extend_from_slice(current);
            Cow::Owned(out)
        }
    }
}

#[cfg(test)]
mod render_raw_context_tests {
    use super::*;
    use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};
    use serde_json::json;

    /// Minimal provider that mimics the OpenAI-compatible wire format:
    /// string `content` for both user and assistant. Mirrors the actual
    /// `OpenAICompatProvider` output closely enough for these helpers.
    struct OpenAiCompatProvider;
    #[async_trait::async_trait]
    impl Provider for OpenAiCompatProvider {
        fn name(&self) -> &str {
            "openai-compat"
        }
        fn model(&self) -> &str {
            "test"
        }
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[crate::types::ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<crate::provider::EventStream> {
            unimplemented!()
        }
        async fn complete_raw(
            &self,
            _raw_messages: &[serde_json::Value],
            _tools: &[crate::types::ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<crate::provider::EventStream> {
            unimplemented!()
        }
        fn format_user_message(&self, blocks: &[ContentBlock]) -> serde_json::Value {
            let text: String = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            json!({"role": "user", "content": text})
        }
        fn format_tool_result(
            &self,
            tool_call_id: &str,
            content: &str,
            _is_error: bool,
        ) -> serde_json::Value {
            json!({"role": "tool", "tool_call_id": tool_call_id, "content": content})
        }
        fn build_raw_assistant_message(
            &self,
            text: &str,
            _reasoning: &str,
            _tool_calls: &[(String, String, String)],
        ) -> serde_json::Value {
            json!({"role": "assistant", "content": text})
        }
    }

    /// Mirrors `AnthropicProvider` wire format: `content` is an array of
    /// blocks for both user and assistant. Used to assert that
    /// `build_send_context` never emits empty text blocks — which Anthropic
    /// rejects with 400 `cache_control cannot be set for empty text blocks`
    /// when `apply_cache_breakpoints` lands on a `n-3`/`n-2` message.
    struct AnthropicProvider;
    #[async_trait::async_trait]
    impl Provider for AnthropicProvider {
        fn name(&self) -> &str {
            "anthropic"
        }
        fn model(&self) -> &str {
            "test"
        }
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[crate::types::ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<crate::provider::EventStream> {
            unimplemented!()
        }
        async fn complete_raw(
            &self,
            _raw_messages: &[serde_json::Value],
            _tools: &[crate::types::ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<crate::provider::EventStream> {
            unimplemented!()
        }
        fn format_user_message(&self, blocks: &[ContentBlock]) -> serde_json::Value {
            let content: Vec<serde_json::Value> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
                    _ => None,
                })
                .collect();
            json!({"role": "user", "content": content})
        }
        fn format_tool_result(
            &self,
            _tool_use_id: &str,
            _content: &str,
            _is_error: bool,
        ) -> serde_json::Value {
            unimplemented!()
        }
        fn build_raw_assistant_message(
            &self,
            text: &str,
            _reasoning: &str,
            _tool_calls: &[(String, String, String)],
        ) -> serde_json::Value {
            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(json!({"type": "text", "text": text}));
            }
            json!({"role": "assistant", "content": content})
        }
    }

    fn prov() -> OpenAiCompatProvider {
        OpenAiCompatProvider
    }

    fn anthropic() -> AnthropicProvider {
        AnthropicProvider
    }

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
        // One turn → one merged ASSISTANT block: first step's text, the
        // folded tool-call annotation with its result, then the reply.
        assert!(rendered.contains("ASSISTANT: I'll start"));
        assert!(rendered.contains("bash() → output"));
        assert!(rendered.contains("Done."));
    }

    #[test]
    fn truncates_long_tool_results() {
        let long = "x".repeat(2000);
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{
                "id": "1", "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }]}),
            json!({"role": "tool", "tool_call_id": "1", "content": long}),
        ];
        let rendered = render_raw_context_as_text(&ctx);
        // Result cap is 500 + "…", inside the annotation, plus the
        // header and USER/ASSISTANT framing.
        assert!(rendered.len() < 700);
        assert!(rendered.contains("…"));
    }

    #[test]
    fn skips_unknown_roles_and_empty_content() {
        let ctx = vec![
            json!({"role": "system", "content": "ignored"}),
            json!({"role": "user", "content": ""}),
            json!({"role": "user", "content": "real"}),
            json!({"role": "assistant", "content": "reply"}),
        ];
        let rendered = render_raw_context_as_text(&ctx);
        assert!(!rendered.contains("ignored"));
        assert!(rendered.contains("USER: real"));
        assert!(rendered.contains("ASSISTANT: reply"));
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
            note_window: None,
        };
        let sent = build_send_context(&prov(), &ctx, prior_len, &cfg);
        assert!(matches!(sent, Cow::Borrowed(_)));
        assert_eq!(sent.len(), ctx.len());
    }

    #[test]
    fn sliding_window_emits_windowed_and_current() {
        // Prior: 3 user+assistant turns, plus a tool turn in the middle.
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
            note_window: None,
        };
        let sent = build_send_context(&prov(), &ctx, prior_len, &cfg);
        let sent = sent.into_owned();

        // 1. Windowed recent N pairs from prior (reformatted by provider).
        // a2 issued a tool call: its text stays pure, and the call summary
        // (with the matching result "out" after the arrow) precedes as a
        // separate user-role history note.
        assert_eq!(sent[0], json!({"role": "user", "content": "u2"}));
        assert_eq!(
            sent[1],
            json!({"role": "user",
                "content": "[History note] assistant tool calls: bash() → out"})
        );
        assert_eq!(sent[2], json!({"role": "assistant", "content": "a2"}));
        assert_eq!(sent[3], json!({"role": "user", "content": "u3"}));
        assert_eq!(sent[4], json!({"role": "assistant", "content": "a3"}));

        // 2. Current turn verbatim.
        assert_eq!(sent[5], current[0]);
        assert_eq!(sent[6], current[1]);
        assert_eq!(sent[7], current[2]);
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
            note_window: None,
        };
        let sent = build_send_context(&prov(), &ctx, 99, &cfg).into_owned();
        // boundary clamps to raw_context.len(), so the whole ctx is
        // treated as prior. Expected: 1 complete pair (u1, a1) = 2 messages.
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], json!({"role": "user", "content": "u1"}));
        assert_eq!(sent[1], json!({"role": "assistant", "content": "a1"}));
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
            note_window: None,
        };
        let sent = build_send_context(&prov(), &ctx, 0, &cfg).into_owned();
        // No prior → no windowed. Just current turn verbatim.
        assert_eq!(sent, ctx);
    }

    /// Regression: with Anthropic-shaped prior context, sliding-window must
    /// not produce empty text blocks (Anthropic rejects them, especially
    /// under cache_control on `messages[n-3]`/`[n-2]`). Also verifies that
    /// assistant text in array form is recognized by pairing, and tool_result
    /// user-role wrappers are excluded from the windowed pairs.
    #[test]
    fn sliding_window_anthropic_shaped_prior_emits_no_empty_blocks() {
        let prior = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "u1"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "a1"}]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "calling bash"},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {}},
            ]}),
            // Anthropic tool result: role "user" with tool_result block, no text.
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "u2"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "a2"}]}),
        ];
        let current = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "u3"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "a3"}]}),
        ];
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.extend(current.clone());

        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 4,
            note_window: None,
        };
        let sent = build_send_context(&anthropic(), &ctx, prior_len, &cfg).into_owned();

        // Walk every emitted message: assert no text block has empty text.
        for (i, msg) in sent.iter().enumerate() {
            let role = msg["role"].as_str().unwrap_or("");
            if let Some(blocks) = msg["content"].as_array() {
                for b in blocks {
                    if b["type"].as_str() == Some("text") {
                        let text = b["text"].as_str().unwrap_or("");
                        assert!(
                            !text.is_empty(),
                            "empty text block at messages.{i} (role={role})",
                        );
                    }
                }
            }
        }

        // The windowed pairs should be present; the tool_result user-role
        // wrapper must be excluded. Turn-based pairing merges u1's two
        // assistant steps ("a1" and "calling bash") into one pure-text
        // entry, with the tool_use summarized in a preceding history note;
        // (u2, a2) stays a plain pair.
        assert!(
            sent.iter()
                .any(|m| m["role"] == "user" && m["content"][0]["text"].as_str() == Some("u1"))
        );
        assert!(sent.iter().any(|m| m["role"] == "assistant"
            && m["content"][0]["text"].as_str() == Some("a1\ncalling bash")));
        assert!(sent.iter().any(|m| m["role"] == "user"
            && m["content"][0]["text"].as_str()
                == Some("[History note] assistant tool calls: bash() → ok")));
        assert!(
            sent.iter()
                .any(|m| m["role"] == "user" && m["content"][0]["text"].as_str() == Some("u2"))
        );
        assert!(sent.iter().any(|m| m["role"] == "assistant"
            && m["content"][0]["text"].as_str() == Some("a2")));
        // No standalone tool_result user message (its text would be empty).
        assert!(
            !sent
                .iter()
                .any(|m| m["content"][0]["type"].as_str() == Some("tool_result")),
            "tool_result wrapper leaked into windowed output"
        );

        // Current turn preserved verbatim at the tail.
        let tail_start = sent.len() - current.len();
        assert_eq!(&sent[tail_start..], current.as_slice());
    }
}
