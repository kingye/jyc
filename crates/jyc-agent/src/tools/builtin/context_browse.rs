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

/// Byte bound on one tool result in a rendered pair, and on the whole
/// `RESULTS:` line of that pair. Only `results=true` pays these: the results
/// are the bulk of a transcript, and a page of result-heavy turns is a tool
/// result that then rides every remaining call of the turn.
const RESULT_TEXT_MAX: usize = 2000;
const RESULTS_LINE_MAX: usize = 4000;

/// One result plus its truncation and overflow markers fits a line with the
/// caps this module ships, so a pair never starts out by dropping the result
/// it was asked to show. The renderer does not *need* this to stay bounded —
/// its empty-body pass returns the marker — so this is a statement about the
/// shipped caps, not the renderer's precondition.
const _: () = assert!(RESULT_TEXT_MAX + 64 < RESULTS_LINE_MAX);

/// Byte bound on the whole page. [`TOOLS_LINE_MAX`] and [`MESSAGE_TEXT_MAX`]
/// bound one pair (~8 KB worst case), which [`MAX_PAGE`] then multiplies by
/// fifty; this caps the sum so a maximal page stays in the low tens of KB —
/// it is a tool result, resent on every remaining call of the turn.
///
/// Over the bound, whole pairs are dropped from the *old* end of the page
/// and counted (`…(N older pairs omitted — raise \`offset\` to read them)`),
/// so a page still ends at the newest pair it was asked for.
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
         (1 = first). Returns oldest→newest. `results=true` also replays each turn's \
         tool results, which folded history omits by default."
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
                },
                "results": {
                    "type": "boolean",
                    "description": "Also replay each turn's tool results (bounded).",
                    "default": false
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
        let with_results = input
            .get("results")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let (journal_pairs, journal_total) = journal_view(ctx, offset + limit, with_results).await;
        // The running turn is still in memory — the journal does not have
        // it yet.
        let current_pairs = if with_results {
            crate::session::extract_pairs_with_results(
                &ctx.raw_context,
                RESULT_TEXT_MAX,
                crate::session::truncate_text,
            )
        } else {
            crate::session::extract_pairs(&ctx.raw_context)
        };
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

        // Either half can be empty (a page entirely inside the journal, or
        // one entirely in the running turn), and `end` can sit below
        // `journal_total`, so both sides are guarded rather than sliced
        // unconditionally.
        let journal_slice: &[TurnPair] = if journal_to > journal_from {
            &journal_pairs[journal_from - journal_first..journal_to - journal_first]
        } else {
            &[]
        };
        let current_slice: &[TurnPair] = if current_to > current_from {
            &current_pairs[current_from - journal_total..current_to - journal_total]
        } else {
            &[]
        };

        // Render the page oldest→newest, then keep the *newest* pairs that
        // fit. The bound trims the old end on purpose: `offset` counts from
        // the newest pair, so the knob the note points at actually reaches
        // what was left out. Trimming the newest pairs would make the
        // omitted ones unreachable — raising `offset` would re-render the
        // same already-visible page.
        let rendered: Vec<String> = journal_slice
            .iter()
            .chain(current_slice.iter())
            .enumerate()
            .map(|(i, pair)| format_pair(start + 1 + i, pair, with_results))
            .collect();
        let mut kept = 0usize;
        let mut bytes = 0usize;
        for chunk in rendered.iter().rev() {
            // The newest pair is kept even on its own, so a single oversized
            // pair cannot produce an empty page.
            if kept > 0 && bytes + chunk.len() > PAGE_TEXT_MAX {
                break;
            }
            bytes += chunk.len();
            kept += 1;
        }
        let first = rendered.len() - kept;
        let mut out = format!(
            "=== Transcript pairs {}-{} of {} (oldest→newest, offset {offset}) ===\n",
            start + 1 + first,
            end,
            total
        );
        for chunk in &rendered[first..] {
            out.push_str(chunk);
        }
        let omitted = end - start - kept;
        if omitted > 0 {
            out.push_str(&format!(
                "\n…({omitted} older pairs omitted to stay within {PAGE_TEXT_MAX} bytes \
                 — raise `offset` to read them)\n"
            ));
        }
        Ok(ToolOutput::success(out))
    }
}

/// One pair as the tool renders it.
fn format_pair(n: usize, pair: &TurnPair, with_results: bool) -> String {
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
    if with_results && let Some(results) = render_turn_results_bounded(pair) {
        out.push_str(&format!("[{n}] RESULTS: {results}\n"));
    }
    out
}

/// The turn's tool-result texts, bounded by [`RESULTS_LINE_MAX`].
///
/// Each result already carries its own [`RESULT_TEXT_MAX`] truncation from
/// collection — see [`crate::session::extract_pairs_with_results`] — so this
/// only drops whole results from the end. The overflow count survives, so
/// the reader can always tell how many results it is not seeing.
fn render_turn_results_bounded(pair: &TurnPair) -> Option<String> {
    if pair.results.is_empty() {
        return None;
    }
    let total = pair.results.len();
    let marker = |dropped: usize| -> String {
        match dropped {
            0 => String::new(),
            1 => " …(1 more result)".to_string(),
            n => format!(" …({n} more results)"),
        }
    };
    let mut keep = total;
    loop {
        let body = pair.results[..keep].join(" | ");
        let suffix = marker(total - keep);
        if body.len() + suffix.len() <= RESULTS_LINE_MAX {
            return Some(format!("{body}{suffix}"));
        }
        // Nothing else can happen: the next iteration has an empty body, so
        // the only term left is the marker, which fits `RESULTS_LINE_MAX` for
        // any decimal count. That empty-body pass *is* how the line stays
        // bounded when a caller collects with a wider per-result cap, so no
        // `keep == 0` branch is needed here.
        keep -= 1;
    }
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
async fn journal_view(
    ctx: &ToolContext<'_>,
    want: usize,
    with_results: bool,
) -> (Vec<TurnPair>, usize) {
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
    let pairs = if with_results {
        // The extraction window is `offset + limit` pairs deep and `offset`
        // is unbounded, so the per-result bound has to apply here: cloning
        // the journal's raw results first and truncating at render time
        // would pay for the whole journal to print one bounded page.
        crate::session::extract_pairs_with_results(
            &slice.messages,
            RESULT_TEXT_MAX,
            crate::session::truncate_text,
        )
    } else {
        crate::session::extract_pairs(&slice.messages)
    };
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

    /// The journal half of `results=true`: older turns replay the tool output
    /// stored in the journal, not just the running turn's memory. Registered
    /// so `resolve_state_dir` finds it, and unregistered afterwards so the
    /// process-wide registry keeps no ghost.
    #[tokio::test]
    async fn results_read_tool_output_from_the_journal() {
        let dir = tempfile::tempdir().expect("create working dir");
        let state_dir = dir.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        let topic = "context_browse_journal_results";
        jyc_types::state_dir::register_if_absent(topic, &state_dir);
        let mut body = String::new();
        for (i, output) in [(1, "first output"), (2, "second output")] {
            for msg in [
                json!({"role": "user", "content": format!("u{i}")}),
                json!({"role": "assistant", "content": "ran", "tool_calls": [
                    {"id": format!("c{i}"), "type": "function",
                     "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}}
                ]}),
                json!({"role": "tool", "tool_call_id": format!("c{i}"), "content": output}),
                json!({"role": "assistant", "content": format!("a{i}")}),
            ] {
                body.push_str(&format!("{msg}\n"));
            }
        }
        tokio::fs::write(state_dir.join("agent-context.jsonl"), body)
            .await
            .unwrap();

        let tool = ContextBrowseTool;
        let mut ctx = ToolContext::new(dir.path());
        ctx.current_topic = Some(topic.to_string());
        let out = tool
            .execute(json!({"offset": 0, "limit": 5, "results": true}), &ctx)
            .await
            .unwrap();
        let text = out.content;
        assert!(text.contains("[1] RESULTS: first output"), "{text}");
        assert!(text.contains("[2] RESULTS: second output"), "{text}");

        jyc_types::state_dir::unregister(topic);
        jyc_types::state_dir::clear_closed(topic);
    }

    /// `results=true` replays the OpenAI `role:"tool"` shape: the tool
    /// output rides its own `RESULTS:` line, and the default omits it.
    #[tokio::test]
    async fn results_render_the_openai_tool_output() {
        let dir = tempfile::tempdir().expect("create working dir");
        let raw = || {
            vec![
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "", "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}}
                ]}),
                json!({"role": "tool", "tool_call_id": "c1", "content": "exit 0\nall green"}),
                json!({"role": "assistant", "content": "done"}),
            ]
        };
        let tool = ContextBrowseTool;
        let with = tool
            .execute(json!({"results": true}), &tool_ctx(dir.path(), raw()))
            .await
            .unwrap();
        assert!(
            with.content.contains("[1] RESULTS: exit 0\nall green"),
            "results line: {}",
            with.content
        );
        // The call itself still renders on its own line.
        assert!(with.content.contains("[1] TOOLS:"), "{}", with.content);

        let without = tool
            .execute(json!({}), &tool_ctx(dir.path(), raw()))
            .await
            .unwrap();
        assert!(
            !without.content.contains("RESULTS:"),
            "default must not replay results: {}",
            without.content
        );
    }

    /// `results=true` also reads the Anthropic shape, where a `user` message
    /// carries the `tool_result` block instead of a `role:"tool"` message.
    #[tokio::test]
    async fn results_render_the_anthropic_tool_result_block() {
        let dir = tempfile::tempdir().expect("create working dir");
        let raw = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "u1"}]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "running"},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "a.txt\nb.txt"}
            ]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}),
        ];
        let out = ContextBrowseTool
            .execute(json!({"results": true}), &tool_ctx(dir.path(), raw))
            .await
            .unwrap();
        assert!(
            out.content.contains("[1] RESULTS: a.txt\nb.txt"),
            "anthropic result block: {}",
            out.content
        );
    }

    /// Turning results on changes what each pair shows, not which pairs the
    /// page holds: same header, same numbering, same total.
    #[tokio::test]
    async fn results_flag_leaves_the_page_and_total_alone() {
        let dir = tempfile::tempdir().expect("create working dir");
        let raw = || {
            let mut v = Vec::new();
            for i in 1..=4 {
                v.push(json!({"role": "user", "content": format!("u{i}")}));
                v.push(json!({"role": "assistant", "content": "", "tool_calls": [
                    {"id": format!("c{i}"), "type": "function",
                     "function": {"name": "bash", "arguments": "{}"}}
                ]}));
                v.push(json!({"role": "tool", "tool_call_id": format!("c{i}"),
                              "content": format!("out{i}")}));
                v.push(json!({"role": "assistant", "content": format!("a{i}")}));
            }
            v
        };
        let tool = ContextBrowseTool;
        let args = json!({"offset": 0, "limit": 2});
        let plain = tool
            .execute(args.clone(), &tool_ctx(dir.path(), raw()))
            .await
            .unwrap();
        let mut with_args = args;
        with_args["results"] = json!(true);
        let with = tool
            .execute(with_args, &tool_ctx(dir.path(), raw()))
            .await
            .unwrap();

        let header = |text: &str| text.lines().next().unwrap_or("").to_string();
        assert_eq!(header(&plain.content), header(&with.content));
        assert!(header(&with.content).contains("of 4"), "{}", with.content);
        assert!(
            with.content.contains("[3] RESULTS: out3")
                && with.content.contains("[4] RESULTS: out4"),
            "the page must be pairs 3-4: {}",
            with.content
        );
    }

    /// A result much larger than the bound is cut at collection, with the
    /// truncation marker saying how much was dropped — the page cannot grow
    /// with the journal's raw output.
    #[tokio::test]
    async fn oversized_results_are_cut_at_collection() {
        let dir = tempfile::tempdir().expect("create working dir");
        let huge = "z".repeat(RESULT_TEXT_MAX * 5);
        let raw = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": huge}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let out = ContextBrowseTool
            .execute(json!({"results": true}), &tool_ctx(dir.path(), raw))
            .await
            .unwrap();
        assert!(out.content.contains("truncated"), "{}", out.content);
        assert!(
            out.content.len() < RESULT_TEXT_MAX * 2,
            "page grew with the raw result: {} bytes",
            out.content.len()
        );
    }

    /// Results over the line budget are dropped **whole** from the end and
    /// counted, so the reader still knows how many it is not seeing.
    #[tokio::test]
    async fn many_results_keep_the_overflow_count() {
        let dir = tempfile::tempdir().expect("create working dir");
        let mut msgs = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c0", "type": "function", "function": {"name": "bash", "arguments": "{}"}}
            ]}),
        ];
        // Three results of ~1.5 KB each: two fit `RESULTS_LINE_MAX`, so the
        // third must be dropped and counted.
        for i in 1..=3 {
            msgs.push(json!({"role": "tool", "tool_call_id": format!("c{i}"),
                             "content": format!("r{i}{}", "y".repeat(1500))}));
        }
        msgs.push(json!({"role": "assistant", "content": "done"}));
        let out = ContextBrowseTool
            .execute(json!({"results": true}), &tool_ctx(dir.path(), msgs))
            .await
            .unwrap();
        assert!(
            out.content.contains("more result"),
            "overflow count missing: {}",
            out.content
        );
        assert!(
            out.content.contains("r1") && !out.content.contains("r3"),
            "whole results, newest dropped: {}",
            out.content
        );
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
    ///
    /// The dropped pairs are the *oldest* of the requested range, so the
    /// page still ends at the newest pair, and `offset` (which counts from
    /// the newest end) reaches the omitted ones.
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
        // One pair is ~4.1 KB; the bound plus the header, the newest pair
        // kept unconditionally and the omission note.
        assert!(
            text.len() < PAGE_TEXT_MAX + 4_500,
            "page not bounded: {}",
            text.len()
        );
        assert!(
            text.contains("older pairs omitted"),
            "the dropped pairs must be counted: {text}"
        );
        assert!(
            text.contains("raise `offset`"),
            "the note must name the knob that reaches them: {text}"
        );
        // The newest pair survives; the header reports what is actually
        // rendered, not the requested range.
        assert!(text.contains("[20] ASSISTANT:"), "newest pair: {text}");
        assert!(
            !text.contains("[1] USER:"),
            "oldest must be the drop: {text}"
        );
        let header = text.lines().next().unwrap_or_default();
        assert!(!header.contains("pairs 1-20"), "header: {header}");
        assert!(
            header.ends_with("of 20 (oldest→newest, offset 0) ==="),
            "{header}"
        );
    }

    /// A page whose range crosses the journal/current-turn boundary with a
    /// non-zero offset: both halves can be empty and `end` can sit below
    /// the journal total, so both slices need a guard rather than an
    /// unconditional range.
    #[tokio::test]
    async fn offset_into_a_page_entirely_inside_the_journal() {
        let dir = tempfile::tempdir().expect("create working dir");
        let state_dir = dir.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        let topic = "context_browse_journal_offset";
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
        // Two complete pairs in the running turn, so total = 8 and the page
        // [3, 5) lies wholly inside the journal.
        ctx.raw_context = vec![
            json!({"role": "user", "content": "u7"}),
            json!({"role": "assistant", "content": "a7"}),
            json!({"role": "user", "content": "u8"}),
            json!({"role": "assistant", "content": "a8"}),
        ];
        let out = tool
            .execute(json!({"offset": 4, "limit": 2}), &ctx)
            .await
            .unwrap();
        let text = out.content;
        assert!(text.contains("pairs 3-4 of 8"), "header: {text}");
        assert!(text.contains("[3] USER: u3"), "first: {text}");
        assert!(text.contains("[4] ASSISTANT: a4"), "last: {text}");
        assert!(!text.contains("u5"), "newer pair leaked: {text}");
    }

    /// A call-heavy pair still renders: the per-call argument bound and the
    /// per-turn call bound are what keep it small, so a page of such turns
    /// is not the expensive part of the payload. (This is also the shape
    /// the whole-page bound's keep-at-least-one rule exists for, though
    /// with the other caps in place one pair cannot reach it.)
    #[tokio::test]
    async fn call_heavy_pair_is_bounded_and_rendered() {
        let calls: Vec<Value> = (0..40)
            .map(|i| {
                json!({"id": i.to_string(), "type": "function", "function": {
                    "name": "bash",
                    "arguments": json!({"command": "x".repeat(1_900)}).to_string(),
                }})
            })
            .collect();
        let raw = vec![
            json!({"role": "user", "content": "u"}),
            json!({"role": "assistant", "content": "a", "tool_calls": calls}),
        ];
        let dir = tempfile::tempdir().expect("create working dir");
        let tool = ContextBrowseTool;
        let out = tool
            .execute(json!({"limit": 1}), &tool_ctx(dir.path(), raw))
            .await
            .unwrap();
        let text = out.content;
        assert!(
            text.contains("[1] USER: u"),
            "page must not be empty: {text}"
        );
        // 40 calls cannot fit in 4000 bytes: whole calls are dropped and
        // the count survives.
        assert!(text.contains("more calls)"), "tool line: {text}");
        assert!(
            text.len() < TOOLS_LINE_MAX + 500,
            "one pair must stay small: {}",
            text.len()
        );
    }

    /// The built-in registry exposes context_browse.
    #[test]
    fn registered_in_builtin_registry() {
        let registry = crate::tools::builtin::create_builtin_registry();
        assert!(registry.has_tool("context_browse"));
    }
}
