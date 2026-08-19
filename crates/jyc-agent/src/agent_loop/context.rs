//! Raw-context rendering and history compaction helpers.
//!
//! Extracted from the monolithic `agent_loop.rs`.

use std::borrow::Cow;

use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};

use crate::provider::Provider;
use crate::types::{ContentBlock, Message, Role};

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

/// Render `raw_context` as a plain-text conversation: user and assistant
/// text only, tool calls / tool results omitted. Provider-format agnostic:
/// accepts both OpenAI (`content: "string"`) and Anthropic
/// (`content: [{type:"text", text:"..."}, ...]`) shapes. Used by the
/// sliding-window strategy to give the model the full prior history as
/// extra context without re-emitting tool noise.
fn render_conversation_text(raw_context: &[serde_json::Value]) -> String {
    let mut out = String::with_capacity(raw_context.len() * 256);
    for msg in raw_context {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "user" | "assistant" => {
                let text = crate::session::extract_message_text(msg);
                if !text.is_empty() {
                    let label = if role == "user" { "USER" } else { "ASSISTANT" };
                    out.push_str(label);
                    out.push_str(": ");
                    out.push_str(&text);
                    out.push_str("\n\n");
                }
            }
            _ => {}
        }
    }
    out
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
    let formatted = if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
        provider.build_raw_assistant_message(&text, "", &[])
    } else {
        provider.format_user_message(&[ContentBlock::Text { text }])
    };
    Some(formatted)
}

/// Build the context to send to the LLM for the next request.
///
/// The full `raw_context` is always persisted to `.jyc/agent-context.json`
/// untouched; this function only shapes what is sent to the LLM.
///
/// * `Full` — borrow the full context (no copy).
/// * `SlidingWindow` — three parts, in order:
///   1. A synthetic user message containing the full prior conversation
///      rendered as plain user/assistant text (tool calls / tool results
///      omitted). Recovers history that the window would otherwise drop.
///   2. The last `strategy.window` user+assistant text pairs from the prior
///      context, reformatted in provider wire format (string content →
///      provider-correct content shape).
///   3. The full current turn (`raw_context[prior_len..]`) verbatim, so
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

            let transcript = render_conversation_text(prior);
            let mut out = Vec::new();
            if !transcript.is_empty() {
                out.push(provider.format_user_message(&[ContentBlock::Text {
                    text: format!(
                        "<jyc-conversation-history>\nFull prior conversation (user and \
                         assistant text only; tool calls and results omitted):\n\n{}\n\
                         </jyc-conversation-history>",
                        transcript
                    ),
                }]));
            }

            let windowed = crate::session::extract_user_assistant_pairs(prior, strategy.window);
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
            tool_use_id: &str,
            content: &str,
            _is_error: bool,
        ) -> serde_json::Value {
            json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": tool_use_id, "content": content}],
            })
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
    fn render_conversation_text_skips_tool_messages() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "out"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ];
        let rendered = render_conversation_text(&ctx);
        assert!(rendered.contains("USER: u1"));
        assert!(rendered.contains("ASSISTANT: a1"));
        assert!(rendered.contains("USER: u2"));
        assert!(rendered.contains("ASSISTANT: a2"));
        assert!(!rendered.contains("bash"));
        assert!(!rendered.contains("out"));
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
        let sent = build_send_context(&prov(), &ctx, prior_len, &cfg);
        assert!(matches!(sent, Cow::Borrowed(_)));
        assert_eq!(sent.len(), ctx.len());
    }

    #[test]
    fn sliding_window_emits_transcript_windowed_and_current() {
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
        };
        let sent = build_send_context(&prov(), &ctx, prior_len, &cfg);
        let sent = sent.into_owned();

        // 1. Transcript message: full prior text (no tools), wrapped.
        assert_eq!(sent[0]["role"], "user");
        let transcript_text = sent[0]["content"].as_str().unwrap();
        assert!(transcript_text.starts_with("<jyc-conversation-history>"));
        assert!(transcript_text.contains("USER: u1"));
        assert!(transcript_text.contains("ASSISTANT: a3"));
        assert!(!transcript_text.contains("[tool_call"));
        assert!(!transcript_text.contains("TOOL_RESULT"));

        // 2. Windowed recent N pairs from prior (reformatted by provider).
        assert_eq!(sent[1], json!({"role": "user", "content": "u2"}));
        assert_eq!(sent[2], json!({"role": "assistant", "content": "a2"}));
        assert_eq!(sent[3], json!({"role": "user", "content": "u3"}));
        assert_eq!(sent[4], json!({"role": "assistant", "content": "a3"}));

        // 3. Current turn verbatim.
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
        };
        let sent = build_send_context(&prov(), &ctx, 99, &cfg).into_owned();
        // boundary clamps to raw_context.len(), so the whole ctx is
        // treated as prior. Expected: 1 transcript + 1 complete pair
        // (u1, a1) = 3 messages.
        assert_eq!(sent.len(), 3);
        assert!(
            sent[0]["content"]
                .as_str()
                .unwrap()
                .contains("<jyc-conversation-history>")
        );
        assert_eq!(sent[1], json!({"role": "user", "content": "u1"}));
        assert_eq!(sent[2], json!({"role": "assistant", "content": "a1"}));
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
        let sent = build_send_context(&prov(), &ctx, 0, &cfg).into_owned();
        // No prior → no transcript, no windowed. Just current turn verbatim.
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

        // The windowed pairs (u1,a1)(u2,a2) should be present; the
        // tool_result user-role wrapper must be excluded.
        assert!(
            sent.iter()
                .any(|m| m["role"] == "user" && m["content"][0]["text"].as_str() == Some("u1"))
        );
        assert!(sent.iter().any(|m| m["role"] == "assistant"
            && m["content"][0]["text"].as_str() == Some("a1")));
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
