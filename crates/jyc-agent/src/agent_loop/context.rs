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
/// * `SlidingWindow` — three parts, in order:
///   1. The last `window - recent` user+assistant text pairs from the
///      older prior, reformatted in provider wire format (string content →
///      provider-correct content shape).
///   2. The most recent `note_window` prior turns **verbatim** (structured
///      `tool_calls` + tool results intact, like the current turn), so the
///      model keeps seeing tool calls in the wire format it is supposed to
///      emit instead of user-role text notes.
///   3. The full current turn (`raw_context[prior_len..]`) verbatim, so
///      tool calls / results stay coherent mid-loop.
///
/// Thin wrapper kept for the existing test surface; production callers
/// use [`build_send_context_with_regions`] directly.
#[allow(dead_code)]
pub(crate) fn build_send_context<'a>(
    provider: &dyn Provider,
    raw_context: &'a [serde_json::Value],
    prior_len: usize,
    strategy: &ContextStrategyConfig,
) -> Cow<'a, [serde_json::Value]> {
    build_send_context_with_regions(provider, raw_context, prior_len, strategy).0
}

/// Same as [`build_send_context`] but also returns a per-message region
/// label (parallel to the wire payload), for callers that need to
/// distinguish regions (debug dump, tests).
///
/// Region labels:
/// - `1` = first region — compacted history (`SlidingWindow`) or the
///   whole context (`Full` mode, no region split).
/// - `2` = second region — verbatim prior turns (`SlidingWindow` only).
/// - `3` = third region — current turn verbatim (`SlidingWindow` only).
pub(crate) fn build_send_context_with_regions<'a>(
    provider: &dyn Provider,
    raw_context: &'a [serde_json::Value],
    prior_len: usize,
    strategy: &ContextStrategyConfig,
) -> (Cow<'a, [serde_json::Value]>, Vec<u8>) {
    match strategy.mode {
        ContextStrategy::Full => (Cow::Borrowed(raw_context), vec![1; raw_context.len()]),
        ContextStrategy::SlidingWindow => {
            // Mid-loop compression can shorten `raw_context` below `prior_len`;
            // clamp so the slice math never underflows.
            let boundary = prior_len.min(raw_context.len());
            let prior = &raw_context[..boundary];
            let current = &raw_context[boundary..];

            let mut out = Vec::new();
            let mut regions = Vec::new();

            // The most recent `note_window` prior turns are sent VERBATIM
            // (structured tool_calls + results) instead of being folded into
            // user-role text notes. Text notes taught thinking models
            // (MiniMax, DeepSeek) to write their process — tool calls and
            // action announcements — as content instead of using the
            // structured channel, which leaked "thinking" into delivered
            // replies. Keeping the structured wire format in the recent
            // window restores the format the model should mimic.
            let note_window = strategy.note_window.unwrap_or(usize::MAX);
            let verbatim_start = crate::session::verbatim_start_index(prior, note_window);
            let (compact_prior, verbatim_prior) = prior.split_at(verbatim_start);

            // Older pairs compact to text-only user/assistant. No history
            // notes here — the recent pairs are already verbatim, so notes
            // would be redundant (and are the leak trigger).
            let verbatim_pairs = crate::session::extract_pairs(verbatim_prior).len();
            let compact_keep = strategy.window.saturating_sub(verbatim_pairs);
            let compacted =
                crate::session::extract_user_assistant_pairs(compact_prior, compact_keep, Some(0));
            for msg in &compacted {
                // Defensive: skip messages with no extractable text so the
                // wire payload never contains empty text blocks (which
                // Anthropic rejects, especially under cache_control).
                if let Some(formatted) = format_cleaned_message(provider, msg) {
                    out.push(formatted);
                    regions.push(1);
                }
            }

            // Recent turns verbatim — skip round-tripped history notes
            // (metadata, not conversation; re-sending them as user text is
            // the pattern that triggered the leak).
            for msg in verbatim_prior {
                if crate::session::is_history_note(msg) {
                    continue;
                }
                out.push(msg.clone());
                regions.push(2);
            }

            out.extend_from_slice(current);
            regions.extend(std::iter::repeat_n(3, current.len()));
            (Cow::Owned(out), regions)
        }
    }
}

/// Debug dump of what `build_send_context` would send to the LLM — a
/// human-readable, region-labeled view of the wire payload. Caller
/// chooses what to do with the returned string (`println!`, write to
/// file, log via `tracing::debug!`, etc.). Currently used by the test
/// suite — kept as a public(crate) API for future in-process debug
/// tooling (e.g., a `/dump` rendering path).
#[allow(dead_code)]
pub(crate) fn dump_send_context(
    provider: &dyn Provider,
    raw_context: &[serde_json::Value],
    prior_len: usize,
    strategy: &ContextStrategyConfig,
) -> String {
    let (sent, regions) =
        build_send_context_with_regions(provider, raw_context, prior_len, strategy);
    let mut s = format!(
        "=== context sent to LLM (strategy={:?} window={} note_window={:?}) ===\n\
         {} msgs, ~{} bytes\n\n",
        strategy.mode,
        strategy.window,
        strategy.note_window,
        sent.len(),
        serde_json::to_string(sent.as_ref())
            .map(|s| s.len())
            .unwrap_or(0),
    );
    append_region_summary(
        &mut s,
        sent.as_ref(),
        &regions,
        matches!(strategy.mode, ContextStrategy::Full),
    );
    s.push_str("\n--- wire payload ---\n");
    match serde_json::to_string_pretty(sent.as_ref()) {
        Ok(p) => {
            s.push_str(&p);
            s.push('\n');
        }
        Err(e) => {
            s.push_str(&format!("<serialize failed: {e}>"));
        }
    }
    s
}

fn append_region_summary(
    s: &mut String,
    sent: &[serde_json::Value],
    regions: &[u8],
    is_full: bool,
) {
    if is_full {
        s.push_str("(Full mode — single region, no windowing)\n");
        for (i, msg) in sent.iter().enumerate() {
            s.push_str(&format!("  [{i}] {}\n", msg_one_line(msg)));
        }
        return;
    }

    // SlidingWindow: walk through msgs, group by region, emit ASCII box per region.
    let region_headers = [
        (1u8, "① Compacted history (text-only)"),
        (2u8, "② Verbatim (structured tool calls kept)"),
        (3u8, "③ Current turn (verbatim)"),
    ];

    let mut first = true;
    for (region, header) in region_headers {
        let msgs_in_region: Vec<(usize, &serde_json::Value)> = sent
            .iter()
            .zip(regions.iter())
            .enumerate()
            .filter(|(_, (_, r))| **r == region)
            .map(|(i, (m, _))| (i, m))
            .collect();
        if msgs_in_region.is_empty() {
            continue;
        }
        if !first {
            s.push_str("├─\n");
        }
        s.push_str(&format!("┌─ {}\n", header));
        for (i, m) in &msgs_in_region {
            s.push_str(&format!("│  [{i}] {}\n", msg_one_line(m)));
        }
        first = false;
    }
    if sent.is_empty() {
        s.push_str("(empty wire payload)\n");
    } else {
        s.push_str("└─\n");
    }
}

fn msg_one_line(msg: &serde_json::Value) -> String {
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("?");
    let mut out = role.to_string();
    let text = crate::session::extract_message_text(msg);
    if !text.is_empty() {
        let trimmed = text.replace('\n', " ");
        let preview: String = trimmed.chars().take(60).collect();
        out.push_str(&format!(" \"{preview}\""));
    }
    if let Some(calls) = msg.get("tool_calls").and_then(|v| v.as_array())
        && !calls.is_empty()
    {
        out.push_str(&format!(" ({} tool_calls)", calls.len()));
    }
    out
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
    fn sliding_window_emits_recent_verbatim_and_older_compacted() {
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

        // window=2, note_window=1: the last prior turn (u3, a3) is verbatim,
        // the older turn (u2 with its tool call) compacts to text-only. No
        // history note anywhere — notes taught the model to write tool calls
        // as text (the thinking-leak trigger).
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 2,
            note_window: Some(1),
        };
        let sent = build_send_context(&prov(), &ctx, prior_len, &cfg);
        let sent = sent.into_owned();

        // 1. Older pair compacted to pure text (no note, tool_calls dropped).
        assert_eq!(sent[0], json!({"role": "user", "content": "u2"}));
        assert_eq!(sent[1], json!({"role": "assistant", "content": "a2"}));

        // 2. Recent pair verbatim.
        assert_eq!(sent[2], json!({"role": "user", "content": "u3"}));
        assert_eq!(sent[3], json!({"role": "assistant", "content": "a3"}));

        // 3. Current turn verbatim (tool_calls preserved).
        assert_eq!(sent[4], current[0]);
        assert_eq!(sent[5], current[1]);
        assert_eq!(sent[6], current[2]);
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
    /// under cache_control on `messages[n-3]`/`[n-2]`). The compacted part
    /// goes through provider reformatting; the recent verbatim part is
    /// passed through untouched (with tool_use/tool_result kept paired).
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

        // note_window=1: only the last prior turn (u2, a2) is verbatim; the
        // older u1 turn (with its tool_use + tool_result) compacts to text.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 4,
            note_window: Some(1),
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

        // Older u1 turn compacted: text merged ("a1\ncalling bash"), no
        // history note (notes are the thinking-leak trigger), no tool blocks.
        assert!(
            sent.iter()
                .any(|m| m["role"] == "user" && m["content"][0]["text"].as_str() == Some("u1"))
        );
        assert!(sent.iter().any(|m| m["role"] == "assistant"
            && m["content"][0]["text"].as_str() == Some("a1\ncalling bash")));
        assert!(
            !sent.iter().any(|m| m["content"]
                .as_str()
                .is_some_and(|t| t.starts_with("[History note]"))),
            "no history note may reach the wire"
        );

        // Recent u2 turn verbatim (passed through untouched).
        assert!(
            sent.iter()
                .any(|m| m["role"] == "user" && m["content"][0]["text"].as_str() == Some("u2"))
        );
        assert!(sent.iter().any(|m| m["role"] == "assistant"
            && m["content"][0]["text"].as_str() == Some("a2")));

        // Current turn preserved verbatim at the tail.
        let tail_start = sent.len() - current.len();
        assert_eq!(&sent[tail_start..], current.as_slice());
    }

    /// The most recent `note_window` prior turns are sent VERBATIM: their
    /// structured tool_calls and tool results are preserved (not stripped
    /// into text notes), so the model keeps seeing the wire format it is
    /// supposed to emit.
    #[test]
    fn sliding_window_recent_turn_keeps_structured_tool_calls() {
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "calling bash", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "file.txt"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let current = vec![json!({"role": "user", "content": "u3"})];
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.extend(current.clone());

        // note_window=1: the last prior turn (u2 → bash → done) is verbatim.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            note_window: Some(1),
        };
        let sent = build_send_context(&prov(), &ctx, prior_len, &cfg).into_owned();

        // Older turn compacted text-only.
        assert_eq!(sent[0], json!({"role": "user", "content": "u1"}));
        assert_eq!(sent[1], json!({"role": "assistant", "content": "a1"}));
        // Recent turn VERBATIM: user, structured tool-call assistant, result.
        assert_eq!(sent[2], json!({"role": "user", "content": "u2"}));
        assert_eq!(sent[3], prior[3]);
        assert_eq!(sent[4], prior[4]);
        assert_eq!(sent[5], json!({"role": "assistant", "content": "done"}));
        // Current turn verbatim.
        assert_eq!(sent[6], current[0]);
    }

    /// Round-tripped history notes in the prior (from heuristic compaction)
    /// must NOT be re-sent in the verbatim region — they are metadata, and
    /// re-injecting them as user text is the pattern that triggered the
    /// thinking-content leak.
    #[test]
    fn sliding_window_filters_round_tripped_notes_from_verbatim_region() {
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "user", "content": "[History note] assistant tool calls: bash(command=\"ls\") → ok"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ];
        let current = vec![json!({"role": "user", "content": "u3"})];
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.extend(current.clone());

        // note_window=5: every prior turn is in the verbatim region.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            note_window: Some(5),
        };
        let sent = build_send_context(&prov(), &ctx, prior_len, &cfg).into_owned();

        // The round-tripped note is filtered out of the verbatim pass-through.
        let texts: Vec<&str> = sent.iter().filter_map(|m| m["content"].as_str()).collect();
        assert!(
            !texts.iter().any(|t| t.starts_with("[History note]")),
            "history note must not reach the wire, got: {texts:?}"
        );
        assert_eq!(
            sent,
            vec![
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "a1"}),
                json!({"role": "user", "content": "u2"}),
                json!({"role": "assistant", "content": "a2"}),
                json!({"role": "user", "content": "u3"}),
            ]
        );
    }

    /// `with_regions` and `dump_send_context` — verify region partition is
    /// stable across `Full` and `SlidingWindow` strategies.
    #[test]
    fn with_regions_labels_full_mode_as_single_region() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
        ];
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::Full,
            window: 10,
            note_window: None,
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, 0, &cfg);
        assert_eq!(sent.len(), 3);
        assert_eq!(regions, vec![1, 1, 1]);
        let dump = dump_send_context(&prov(), &ctx, 0, &cfg);
        assert!(
            dump.contains("strategy=Full"),
            "missing strategy summary: {dump}"
        );
        assert!(
            dump.contains("single region"),
            "Full dump should say single region: {dump}"
        );
        assert!(dump.contains("\"u1\""), "missing msg preview: {dump}");
    }

    #[test]
    fn with_regions_labels_sliding_window_three_regions() {
        // ① compacted = (u1, a1), ② verbatim = (u2, a2), ③ current = (u3).
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ];
        let current = vec![json!({"role": "user", "content": "u3"})];
        let mut ctx = prior.clone();
        ctx.extend(current.clone());
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 2,
            note_window: Some(1),
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, prior.len(), &cfg);
        assert_eq!(sent.len(), 5);
        assert_eq!(regions, vec![1, 1, 2, 2, 3]);

        let dump = dump_send_context(&prov(), &ctx, prior.len(), &cfg);
        assert!(
            dump.contains("① Compacted history"),
            "missing region ①: {dump}"
        );
        assert!(dump.contains("② Verbatim"), "missing region ②: {dump}");
        assert!(dump.contains("③ Current turn"), "missing region ③: {dump}");
        assert!(
            dump.contains("--- wire payload ---"),
            "missing payload section: {dump}"
        );
    }
}
