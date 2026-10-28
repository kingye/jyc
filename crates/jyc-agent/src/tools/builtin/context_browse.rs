//! `context_browse` — page through the agent's conversation transcript
//! (user/assistant text pairs), including turns that fell out of the
//! model's sliding context window.
//!
//! The transcript lives in the topic's `agent-context.jsonl` journal. The
//! prior turns are read back from there — the reader parses only the tail
//! the page needs — while the running turn is still in memory.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolOutput};

use crate::context_reader::ContextReader;
use crate::session::TurnPair;
use crate::types::PriorDepth;

/// Hard cap on pairs returned per call. The result is appended to the
/// transcript and resent on the next request, so an unbounded page would
/// blow the window; 50 pairs is plenty for any "what happened earlier"
/// question.
const MAX_PAGE: usize = 50;

/// Byte bound on one turn's rendered `TOOLS:` line. [`MAX_PAGE`] bounds the
/// page by *count*, but each turn can carry up to `TOOL_CALLS_PER_TURN_MAX`
/// calls of up to `TOOL_ARG_MAX` bytes each — a page of call-heavy turns
/// would otherwise be hundreds of KB, and this text is a tool result that
/// rides every remaining call of the current turn.
const TOOLS_LINE_MAX: usize = 4000;

/// Byte bound on one message's text in a rendered pair. [`MAX_PAGE`] bounds
/// the page by pair count, so fifty long assistant replies would otherwise
/// still make a megabyte-sized tool result.
///
/// This is the *content* budget: [`crate::session::truncate_text`] appends
/// its own `… [truncated N bytes]` note on top, so a truncated message sits
/// a couple of dozen bytes above this value.
const MESSAGE_TEXT_MAX: usize = 2000;

/// Byte bound on the whole page. [`TOOLS_LINE_MAX`] and [`MESSAGE_TEXT_MAX`]
/// bound one pair (~8 KB worst case), which [`MAX_PAGE`] then multiplies by
/// fifty; this caps the sum so a maximal page stays in the low tens of KB —
/// it is a tool result, resent on every remaining call of the turn.
const PAGE_TEXT_MAX: usize = 32_000;

/// Page through the conversation transcript.
pub struct ContextBrowseTool;

#[async_trait]
impl Tool for ContextBrowseTool {
    fn name(&self) -> &str {
        "context_browse"
    }

    fn description(&self) -> &str {
        "Browse the conversation transcript — user/assistant text pairs, including \
         turns outside your sliding context window. Pair numbers count from the oldest \
         (1 = first). Returns oldest→newest."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "offset": {
                    "type": "integer",
                    "description": "Pairs to skip from the newest end. 0 = most recent pairs.",
                    "default": 0
                },
                "limit": {
                    "type": "integer",
                    "description": "Max pairs to return (capped at 50).",
                    "default": 10
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let offset = input
            .get("offset")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .max(0) as usize;
        let limit = input
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(10)
            .clamp(1, MAX_PAGE as i64) as usize;

        let (journal_pairs, journal_total) = journal_view(ctx, offset + limit).await;
        // The running turn is still in memory — the journal does not have
        // it yet.
        let current_pairs = crate::session::extract_pairs(&ctx.raw_context);
        let total = journal_total + current_pairs.len();

        // Page [start..end) in oldest→newest order, with `offset` counted
        // from the newest end. Saturating math clamps out-of-range offsets
        // to an empty page instead of panicking.
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(limit);

        if start >= end {
            return Ok(ToolOutput::success(format!(
                "No pairs in range: transcript has {total} pairs, offset {offset}."
            )));
        }

        // The journal's pairs hold the numbers [journal_first .. journal_total]
        // (counting from the oldest), the running turn's the ones after
        // them. The page is [start, end) in the same numbering.
        let journal_first = journal_total.saturating_sub(journal_pairs.len());
        let (journal_from, journal_to) = (start.max(journal_first), end.min(journal_total));
        let (current_from, current_to) = (start.max(journal_total), end);

        let mut out = format!(
            "=== Transcript pairs {}-{} of {} (oldest→newest, offset {offset}) ===\n",
            start + 1,
            end,
            total
        );
        let mut shown = 0usize;
        let journal_slice =
            &journal_pairs[journal_from - journal_first..journal_to - journal_first];
        let current_slice =
            &current_pairs[current_from - journal_total..current_to - journal_total];
        let page = journal_slice.iter().chain(current_slice.iter());
        for (n, pair) in (start..).zip(page) {
            let rendered = format_pair(n + 1, pair);
            // Per-pair and per-line bounds cap what one pair can cost; this
            // caps the page as a whole, so a long page of maximal pairs
            // cannot turn the tool result into a megabyte that rides every
            // remaining call of the turn.
            if shown > 0 && out.len() + rendered.len() > PAGE_TEXT_MAX {
                break;
            }
            out.push_str(&rendered);
            shown += 1;
        }
        let omitted = end - start - shown;
        if omitted > 0 {
            out.push_str(&format!(
                "\n…({omitted} more pairs in this range omitted to stay within \
                 {PAGE_TEXT_MAX} bytes — page with `offset` to read them)\n"
            ));
        }
        Ok(ToolOutput::success(out))
    }
}

/// One pair as the tool renders it.
fn format_pair(n: usize, pair: &TurnPair) -> String {
    let mut out = format!("\n[{n}] USER: {}\n", bounded_message_text(&pair.user),);
    if let Some(assistant) = &pair.assistant {
        out.push_str(&format!(
            "[{n}] ASSISTANT: {}\n",
            bounded_message_text(assistant),
        ));
    }
    if let Some(calls) = crate::session::render_turn_tool_calls_bounded(pair, TOOLS_LINE_MAX) {
        // The list is bounded twice upstream: at most `TOOL_CALLS_PER_TURN_MAX`
        // calls per turn, and here by bytes. Both bounds drop whole calls and
        // keep the overflow marker, so a call-heavy page cannot become the
        // expensive part of the payload — and the reader can still tell how
        // many calls it is not seeing.
        out.push_str(&format!("[{n}] TOOLS: {calls}\n"));
    }
    out
}

/// A message's text, cut to [`MESSAGE_TEXT_MAX`] bytes with a marker when it
/// is longer.
fn bounded_message_text(message: &serde_json::Value) -> String {
    crate::session::truncate_text(
        &crate::session::extract_message_text(message),
        MESSAGE_TEXT_MAX,
    )
}

/// The journal's tail pairs and the journal's total pair count.
///
/// The reader parses only the tail the page needs; the total comes from
/// the reader's counter, which the writer keeps up to date. A topic
/// without a journal (a fresh one, or a test) has neither.
async fn journal_view(ctx: &ToolContext<'_>, want: usize) -> (Vec<TurnPair>, usize) {
    let Some(topic) = ctx.current_topic.as_deref() else {
        return (Vec::new(), 0);
    };
    let Some(state_dir) = jyc_types::state_dir::registered_state(topic) else {
        return (Vec::new(), 0);
    };
    let reader = ContextReader::new(topic, &state_dir);
    let slice = reader
        .read_prior(PriorDepth::Window {
            pairs: want,
            openers: want,
        })
        .await;
    let pairs = crate::session::extract_pairs(&slice.messages);
    let total = reader.total_pairs().await;
    (pairs, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    /// Build a transcript of `n` user+assistant pairs (u1/a1 … un/an).
    fn pairs(n: usize) -> Vec<Value> {
        let mut v = Vec::new();
        for i in 1..=n {
            v.push(json!({"role": "user", "content": format!("u{i}")}));
            v.push(json!({"role": "assistant", "content": format!("a{i}")}));
        }
        v
    }

    /// A ToolContext with the given raw_context snapshot, borrowing the
    /// test's tempdir as the working dir (dropped when the test ends, so
    /// nothing leaks). `context_browse` never touches the working dir.
    fn tool_ctx(working_dir: &Path, raw: Vec<Value>) -> ToolContext<'_> {
        let mut ctx = ToolContext::new(working_dir);
        ctx.raw_context = raw;
        ctx
    }

    #[tokio::test]
    async fn offset_zero_shows_newest_page() {
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(
                json!({"offset": 0, "limit": 10}),
                &tool_ctx(dir.path(), pairs(25)),
            )
            .await
            .unwrap();
        let text = out.content;
        assert!(text.contains("pairs 16-25 of 25"), "header: {text}");
        assert!(text.contains("[16] USER: u16"), "first: {text}");
        assert!(text.contains("[25] ASSISTANT: a25"), "last: {text}");
        assert!(!text.contains("u15"), "older pair leaked: {text}");
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn offset_steps_toward_older_pairs() {
        // 25 pairs, offset 10 → skip newest 10 (16–25) → show 6–15.
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(json!({"offset": 10}), &tool_ctx(dir.path(), pairs(25)))
            .await
            .unwrap();
        let text = out.content;
        assert!(text.contains("pairs 6-15 of 25"), "header: {text}");
        assert!(text.contains("[6] USER: u6"), "text: {text}");
        assert!(text.contains("[15] ASSISTANT: a15"), "text: {text}");
    }

    #[tokio::test]
    async fn offset_beyond_total_returns_empty_page() {
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(json!({"offset": 30}), &tool_ctx(dir.path(), pairs(25)))
            .await
            .unwrap();
        assert!(
            out.content
                .contains("No pairs in range: transcript has 25 pairs, offset 30."),
            "{}",
            out.content
        );
        assert!(!out.is_error);
    }

    /// The older turns come from the journal — the tool reads them back
    /// there, and the running turn from memory.
    #[tokio::test]
    async fn the_journal_supplies_the_older_turns() {
        let dir = tempfile::tempdir().expect("create working dir");
        let state_dir = dir.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        let topic = "context_browse_journal";
        jyc_types::state_dir::register(topic, &state_dir);
        let mut body = String::new();
        for i in 1..=6 {
            body.push_str(&format!(
                "{}\n",
                json!({"role": "user", "content": format!("u{i}")})
            ));
            body.push_str(&format!(
                "{}\n",
                json!({"role": "assistant", "content": format!("a{i}")})
            ));
        }
        tokio::fs::write(state_dir.join("agent-context.jsonl"), body)
            .await
            .unwrap();

        let tool = ContextBrowseTool;
        let mut ctx = ToolContext::new(dir.path());
        ctx.current_topic = Some(topic.to_string());
        // The running turn: no assistant reply yet, so it is no pair.
        ctx.raw_context = vec![json!({"role": "user", "content": "current"})];
        let out = tool
            .execute(json!({"offset": 0, "limit": 3}), &ctx)
            .await
            .unwrap();
        let text = out.content;
        assert!(text.contains("pairs 4-6 of 6"), "header: {text}");
        assert!(text.contains("[4] USER: u4"), "first: {text}");
        assert!(text.contains("[6] ASSISTANT: a6"), "last: {text}");
        assert!(!text.contains("u3"), "older pair leaked: {text}");
    }

    #[tokio::test]
    async fn empty_transcript_returns_empty_page() {
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(json!({}), &tool_ctx(dir.path(), Vec::new()))
            .await
            .unwrap();
        assert!(out.content.contains("0 pairs"), "{}", out.content);
    }

    /// Default limit is 10; an oversized limit is capped (50) rather than
    /// echoing an unbounded page back into the transcript.
    #[tokio::test]
    async fn limit_defaults_and_clamps() {
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        // Defaults: limit 10 → newest 10 of 25 = pairs 16–25.
        let out = tool
            .execute(json!({}), &tool_ctx(dir.path(), pairs(25)))
            .await
            .unwrap();
        assert!(out.content.contains("pairs 16-25 of 25"), "{}", out.content);
        // Huge limit is capped at 50, which covers all 25.
        let out = tool
            .execute(
                json!({"offset": 0, "limit": 999}),
                &tool_ctx(dir.path(), pairs(25)),
            )
            .await
            .unwrap();
        assert!(out.content.contains("pairs 1-25 of 25"), "{}", out.content);
    }

    /// Bare tool calls an assistant issued render on the pair's own TOOLS
    /// line.
    #[tokio::test]
    async fn renders_tool_calls() {
        let raw = vec![
            json!({"role": "user", "content": "u"}),
            json!({"role": "assistant", "content": "running", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": r#"{"command": "ls"}"#}}
            ]}),
        ];
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(json!({}), &tool_ctx(dir.path(), raw))
            .await
            .unwrap();
        assert!(
            out.content.contains("[1] ASSISTANT: running\n"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains(r#"[1] TOOLS: bash(command="ls")"#),
            "{}",
            out.content
        );
    }

    /// A page is bounded by pair count alone, so each message's text is
    /// capped too: fifty long replies must not make a megabyte result.
    #[tokio::test]
    async fn long_message_text_is_bounded() {
        let long = "x".repeat(MESSAGE_TEXT_MAX * 2);
        let raw = vec![
            json!({"role": "user", "content": long.clone()}),
            json!({"role": "assistant", "content": long}),
        ];
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(json!({}), &tool_ctx(dir.path(), raw))
            .await
            .unwrap();
        // Two messages, each capped, plus the headers and the marker.
        assert!(
            out.content.len() < MESSAGE_TEXT_MAX * 2 + 500,
            "page not bounded: {}",
            out.content.len()
        );
        assert!(
            out.content.contains("truncated"),
            "a cut message must say so: {}",
            &out.content[..200.min(out.content.len())]
        );
    }

    /// A page of maximal pairs is bounded as a whole, not only pair by
    /// pair: [`MAX_PAGE`] pairs of capped messages would still sum past a
    /// hundred KB, and the remainder is dropped with a count instead of
    /// being sent every call of the turn.
    #[tokio::test]
    async fn whole_page_is_bounded() {
        let long = "x".repeat(MESSAGE_TEXT_MAX * 2);
        let mut raw = Vec::new();
        for _ in 0..20 {
            raw.push(json!({"role": "user", "content": long.clone()}));
            raw.push(json!({"role": "assistant", "content": long.clone()}));
        }
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(json!({"limit": 20}), &tool_ctx(dir.path(), raw))
            .await
            .unwrap();
        let text = out.content;
        assert!(
            text.len() < PAGE_TEXT_MAX + 2 * MESSAGE_TEXT_MAX,
            "page not bounded: {}",
            text.len()
        );
        assert!(
            text.contains("more pairs in this range omitted"),
            "the dropped tail must be counted: {text}"
        );
    }

    /// The built-in registry exposes context_browse.
    #[test]
    fn registered_in_builtin_registry() {
        let registry = crate::tools::builtin::create_builtin_registry();
        assert!(registry.has_tool("context_browse"));
    }
}
