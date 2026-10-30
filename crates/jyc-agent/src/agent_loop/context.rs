//! Raw-context rendering and history compaction helpers.
//!
//! Extracted from the monolithic `agent_loop.rs`.

use std::borrow::Cow;

use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};
use jyc_types::{ContextRegion, ContextRegionInfo};

use crate::provider::Provider;
use crate::types::ContentBlock;

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

/// Cold build — [`build_send_context_cached`] with a throwaway cache.
///
/// Test-only: the loop passes its own [`SendContextCache`], so a turn's calls
/// reuse the history block instead of rebuilding it.
#[cfg(test)]
pub(crate) fn build_send_context_with_regions<'a>(
    provider: &dyn Provider,
    raw_context: &'a [serde_json::Value],
    prior_len: usize,
    strategy: &ContextStrategyConfig,
) -> (Cow<'a, [serde_json::Value]>, Vec<u8>) {
    build_send_context_cached(
        provider,
        raw_context,
        prior_len,
        strategy,
        &mut SendContextCache::default(),
    )
}

/// Cross-iteration memo for the sliding-window history block.
///
/// The block depends only on the persisted prefix (`raw_context[..prior_len]`)
/// and the strategy, yet the loop rebuilt them for every LLM call — a scan of
/// the whole history that deep-cloned every user/assistant message
/// (`extract_pairs`) only to keep the tail. Within a turn the prefix is
/// fixed: live injections append past `prior_len`, and the only shrink (the
/// cancellation truncation) ends the turn. The first call of a turn builds
/// the block; the rest reuse it.
#[derive(Default)]
pub(crate) struct SendContextCache {
    /// The split point the cached block was built from (`prior_len`
    /// clamped to the context length); `None` = cold.
    ///
    /// A length is a sound key only because the cache belongs to one
    /// `run()`: `strategy` and `provider` are fixed for it, the persisted
    /// prefix can only grow past this point (injections append after it),
    /// and its one shrink — the cancellation truncation — ends the turn. A
    /// cache that outlived a turn would need a content key.
    boundary: Option<usize>,
    history: Vec<serde_json::Value>,
    /// Absolute raw-context index where the verbatim current-turn suffix
    /// starts; `None` when no checkpoint is active.
    checkpoint_end: Option<usize>,
    checkpoint: Vec<serde_json::Value>,
    /// Whether the last build rebuilt the history block (`Some(true)`), reused it
    /// (`Some(false)`), or built nothing at all (`None`, `Full` mode — the log
    /// must not read that as "reused").
    rebuilt: Option<bool>,
    /// Same three states for the current-turn checkpoint.
    checkpoint_rebuilt: Option<bool>,
}

impl SendContextCache {
    /// Whether the last [`build_send_context_cached`] rebuilt the history
    /// block, or `None` when it built nothing (`Full` mode).
    pub(crate) fn history_rebuilt(&self) -> Option<bool> {
        self.rebuilt
    }

    /// Whether the last build rebuilt the current-turn checkpoint, reused
    /// it, or had no checkpoint to build.
    pub(crate) fn checkpoint_rebuilt(&self) -> Option<bool> {
        self.checkpoint_rebuilt
    }
}

/// Build the context to send to the LLM for the next request.
///
/// The full `raw_context` is always persisted to `.jyc/agent-context.jsonl` (append-only journal)
/// untouched; this function only shapes what is sent to the LLM.
///
/// * `Full` — borrow the full context (no copy).
/// * `SlidingWindow` — three parts, in order:
///   1. The prior turns as user+assistant text pairs, reformatted in
///      provider wire format (string content → provider-correct content
///      shape). This block only ever grows: nothing trims its head.
///   2. A checkpoint for the older part of a long current turn, in the
///      same text-only form. It appears once the turn has more than
///      [`ContextStrategyConfig::checkpoint_interval`] assistant messages
///      and advances in whole intervals, so it changes at most once per
///      interval. An interval of `0` disables it, leaving the whole turn
///      verbatim.
///   3. The recent current-turn suffix (`raw_context[checkpoint_end..]`)
///      verbatim, so the latest tool calls / results stay coherent
///      mid-loop and provider-specific replay fields survive there.
///
/// Also returns a per-message region label (parallel to the wire payload)
/// so callers that need to distinguish the three parts — the wire-payload
/// debug dump and tests — can do so without re-running the strategy
/// logic.
///
/// Region labels:
/// - `1` = text-only history (`SlidingWindow`) or the whole context (`Full`
///   mode, no split).
/// - `2` = verbatim current-turn suffix (`SlidingWindow` only).
/// - `3` = folded earlier work in the current turn (`SlidingWindow` only).
///
/// How much history the prior holds is the loader's decision, not this
/// function's: `window` / `extend_rate` / `max_history_tokens` slide the
/// anchored window there, so the history block only changes when the loader
/// slides. Between slides it grows by append, so the provider's cached
/// prefix carries from one turn to the next.
///
/// The current-turn suffix is uncapped by default — the model is
/// reasoning over those tool results mid-loop, so they must stay complete
/// — and bounded by the opt-in `strategy.turn_tool_result_cap`, which
/// keeps head and tail instead of the head alone. `Some(0)` or `None`
/// disables capping. The checkpoint bounds the *older* part independently:
/// its reasoning and tool results leave the wire exactly as prior-turn
/// history does.
///
/// `cache` lets a turn's calls reuse the history and checkpoint blocks
/// instead of rebuilding them per call (see [`SendContextCache`]).
pub(crate) fn build_send_context_cached<'a>(
    provider: &dyn Provider,
    raw_context: &'a [serde_json::Value],
    prior_len: usize,
    strategy: &ContextStrategyConfig,
    cache: &mut SendContextCache,
) -> (Cow<'a, [serde_json::Value]>, Vec<u8>) {
    match strategy.mode {
        ContextStrategy::Full => {
            // Nothing is built: `Full` borrows the context as it is.
            cache.rebuilt = None;
            cache.checkpoint_rebuilt = None;
            (Cow::Borrowed(raw_context), vec![1; raw_context.len()])
        }
        ContextStrategy::SlidingWindow => {
            // A stale `prior_len` must not index past the context: the
            // cancellation truncation is the only shrink, and it clamps
            // `prior_len` itself, but the slice math should not depend on
            // the caller having done so.
            let boundary = prior_len.min(raw_context.len());
            let current = &raw_context[boundary..];

            if cache.boundary != Some(boundary) {
                // The prior is the anchored window the loader read: its
                // head only moves when the loader slides the anchor, so the
                // text-only history only ever grows by append — the
                // provider's cached prefix reaches as far as it does.
                cache.history = build_history_region(provider, &raw_context[..boundary]);
                cache.boundary = Some(boundary);
                cache.rebuilt = Some(true);
                // Absolute checkpoint indices belong to the previous
                // boundary, so they cannot be reused.
                cache.checkpoint_end = None;
                cache.checkpoint.clear();
                cache.checkpoint_rebuilt = None;
            } else {
                cache.rebuilt = Some(false);
            }

            let checkpoint_end = current_turn_checkpoint_end(current, strategy.checkpoint_interval)
                .map(|idx| boundary + idx);
            match checkpoint_end {
                Some(end) if cache.checkpoint_end != Some(end) => {
                    cache.checkpoint =
                        build_checkpoint_region(provider, &raw_context[boundary..end]);
                    cache.checkpoint_end = Some(end);
                    cache.checkpoint_rebuilt = Some(true);
                }
                Some(_) => cache.checkpoint_rebuilt = Some(false),
                None => {
                    cache.checkpoint.clear();
                    cache.checkpoint_end = None;
                    cache.checkpoint_rebuilt = None;
                }
            }
            if cache.checkpoint.is_empty() {
                // No pair to fold (for example assistants without a user
                // anchor): keep the whole current turn verbatim instead of
                // dropping the prefix.
                cache.checkpoint_end = None;
                cache.checkpoint_rebuilt = None;
            }

            let raw_start = cache.checkpoint_end.unwrap_or(boundary);
            let history_len = cache.history.len();
            let checkpoint_len = cache.checkpoint.len();
            let raw_len = raw_context.len().saturating_sub(raw_start);
            let mut out = Vec::with_capacity(history_len + checkpoint_len + raw_len);
            let mut regions = Vec::with_capacity(out.capacity());

            out.extend_from_slice(&cache.history);
            regions.resize(history_len, 1);
            out.extend_from_slice(&cache.checkpoint);
            regions.resize(history_len + checkpoint_len, 3);

            // Recent current turn. Verbatim by default (`turn_tool_result_cap`
            // unset, so `0` short-circuits): the model is reasoning over
            // these tool results mid-loop, so they normally stay
            // complete. When the cap is configured, oversized results
            // keep head and tail with the middle dropped — what a result
            // is read for usually sits at the ends, and the marker tells
            // the model how to page the gap. Both sentinel and default
            // mean off; see the doc above.
            // The rule (0 = off, non-zero lifted) lives in `jyc_types`,
            // shared with the `/context` echo; the lift happens here, where
            // the value is finally trusted, so an old `config.toml` or a
            // stale override cannot skip it.
            let turn_cap = jyc_types::channel::effective_turn_cap(strategy.turn_tool_result_cap);
            for msg in &raw_context[raw_start..] {
                out.push(cap_tool_result_content_with(
                    msg,
                    turn_cap,
                    crate::session::truncate_text_keeping_edges,
                ));
                regions.push(2);
            }
            (Cow::Owned(out), regions)
        }
    }
}

/// Measure the wire payload by region — how many messages each region
/// carries, their exact compact-JSON bytes, and their estimated token
/// weight (the same heuristic the context budget uses).
///
/// The per-message labels come from [`build_send_context_cached`]; this
/// only aggregates them, so the composition describes exactly what was
/// sent. `Full` mode labels the whole payload as one `Full` region.
///
/// The rows keep the order the regions appear on the wire.
pub(crate) fn context_composition(
    messages: &[serde_json::Value],
    regions: &[u8],
    mode: &ContextStrategy,
) -> Vec<ContextRegionInfo> {
    let mut rows: Vec<ContextRegionInfo> = Vec::new();
    for (msg, label) in messages.iter().zip(regions) {
        let region = match (mode, *label) {
            (ContextStrategy::Full, 1) => ContextRegion::Full,
            (ContextStrategy::SlidingWindow, 1) => ContextRegion::History,
            (ContextStrategy::SlidingWindow, 3) => ContextRegion::Checkpoint,
            (ContextStrategy::SlidingWindow, 2) => ContextRegion::Current,
            _ => continue,
        };
        let bytes = serde_json::to_vec(msg)
            .map(|wire| wire.len() as u64)
            .unwrap_or(0);
        let estimated_tokens = est_message_tokens(msg) as u64;
        if let Some(row) = rows.iter_mut().find(|row| row.region == region) {
            row.messages += 1;
            row.bytes = row.bytes.saturating_add(bytes);
            row.estimated_tokens = row.estimated_tokens.saturating_add(estimated_tokens);
        } else {
            rows.push(ContextRegionInfo {
                region,
                messages: 1,
                bytes,
                estimated_tokens,
            });
        }
    }
    rows
}

/// Wire-only marker inserted at the start of a folded current-turn
/// checkpoint. Not persisted to `raw_context`.
pub(crate) const TURN_CHECKPOINT_MARKER_TEXT: &str = "[Current-turn checkpoint] earlier work in this turn follows — text-only, \
     tool calls/results omitted.";

/// Where the verbatim current-turn suffix starts, relative to the current
/// turn slice.
///
/// `interval` is [`ContextStrategyConfig::checkpoint_interval`], defaulting to
/// [`jyc_types::channel::DEFAULT_CHECKPOINT_INTERVAL`]; `0` disables the
/// in-turn checkpoint and always returns `None`.
///
/// With `n` completed assistant messages, fold every whole interval but
/// leave the latest interaction raw:
///
/// ```text
/// folded = floor((n - 1) / interval) × interval
/// ```
///
/// The boundary is derived from the slice alone, so a cold build and a warm
/// cache produce the same payload. It advances only when `n` crosses an
/// interval, which keeps the folded prefix byte-stable in between.
fn current_turn_checkpoint_end(current: &[serde_json::Value], interval: usize) -> Option<usize> {
    // 0 is the off sentinel: no in-turn checkpoint, so the whole current turn
    // rides verbatim. Guarded here because `interval` is also the divisor.
    if interval == 0 {
        return None;
    }
    let assistant_count = current
        .iter()
        .filter(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("assistant"))
        .count();
    if assistant_count <= interval {
        return None;
    }
    let folded = ((assistant_count - 1) / interval) * interval;
    current
        .iter()
        .enumerate()
        .filter(|(_, msg)| msg.get("role").and_then(|r| r.as_str()) == Some("assistant"))
        .nth(folded)
        .map(|(idx, _)| idx)
}

/// Build the text-only history block from the prior window: the history
/// marker, then every turn as a user/assistant pair.
///
/// Nothing here trims: the prior is already the anchored window the loader
/// bounded, and any head-trim here would rewrite the wire prefix on every
/// turn — the bug this shape exists to avoid. The block only grows, so the
/// payload is append-only from one turn to the next.
///
/// The turns are folded to text: their tool calls, arguments and results do
/// not reach the wire (the journal keeps them; `context_browse` reads the
/// turns and their calls back, without results). Only the current turn
/// carries structured tool calls, and it is sent verbatim by the caller.
fn build_history_region(
    provider: &dyn Provider,
    prior: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    build_text_region(provider, prior, crate::session::HISTORY_MARKER_TEXT)
}

/// Compact the older part of the current turn into the same text-only
/// shape as prior-turn history.
fn build_checkpoint_region(
    provider: &dyn Provider,
    prefix: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    build_text_region(provider, prefix, TURN_CHECKPOINT_MARKER_TEXT)
}

/// The one text-only region builder shared by history and current-turn
/// checkpoints.
fn build_text_region(
    provider: &dyn Provider,
    raw: &[serde_json::Value],
    marker: &str,
) -> Vec<serde_json::Value> {
    // No keep-limit: the window bound is the loader's job, and dropping
    // pairs here would cut the head the cache matches on.
    let compacted = crate::session::extract_user_assistant_pairs(raw, usize::MAX);

    // The block leads with its marker and keeps it: it explains the
    // text-only form the compacted pairs come in.
    let mut region: Vec<serde_json::Value> = Vec::new();
    if !compacted.is_empty() {
        region.push(provider.format_user_message(&[ContentBlock::Text {
            text: marker.into(),
        }]));
    }
    for msg in &compacted {
        // Defensive: skip messages with no extractable text so the wire
        // payload never contains empty text blocks (which Anthropic
        // rejects, especially under cache_control).
        if let Some(formatted) = format_cleaned_message(provider, msg) {
            region.push(formatted);
        }
    }
    region
}

/// Per-message constant added to every wire message's estimate — the role,
/// block types and separators providers wrap a message in.
const PER_MESSAGE_TOKENS: usize = 4;

/// Rough token count of one wire message.
///
/// Not a tokenizer, and deliberately so: pulling in a real one costs a
/// dependency and per-call latency to answer a question about history
/// weight, not exact billing. CJK characters count one token each — a
/// Chinese glyph costs about what a short English word does, so a plain
/// bytes÷4 estimate would undercount this project's mixed CJK/English
/// transcripts by a quarter — and everything else counts a quarter token
/// per byte. Object keys are ignored: tool names are a rounding error next
/// to the result text they carry.
pub(crate) fn est_message_tokens(msg: &serde_json::Value) -> usize {
    PER_MESSAGE_TOKENS + est_value_tokens(msg)
}

fn est_value_tokens(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => est_text_tokens(s),
        serde_json::Value::Array(items) => items.iter().map(est_value_tokens).sum(),
        serde_json::Value::Object(fields) => fields.values().map(est_value_tokens).sum(),
        // Numbers, booleans and null ride along inside strings on the wire
        // and cost nothing worth counting here.
        _ => 0,
    }
}

/// One token per wide character, one per four bytes otherwise.
fn est_text_tokens(text: &str) -> usize {
    let mut wide = 0usize;
    let mut narrow_bytes = 0usize;
    for ch in text.chars() {
        if is_wide_char(ch) {
            wide += 1;
        } else {
            narrow_bytes += ch.len_utf8();
        }
    }
    wide + narrow_bytes.div_ceil(4)
}

/// Characters that tokenize at roughly one token per character — CJK
/// ideographs, kana, Hangul and the full-width punctuation around them.
/// ASCII prose is left out: at ~¼ token per byte it already lands right.
fn is_wide_char(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x11FF     // Hangul Jamo
        | 0x2E80..=0x303F   // CJK radicals, Kangxi, punctuation
        | 0x3040..=0x30FF   // Hiragana + Katakana
        | 0x3130..=0x318F   // Hangul compatibility Jamo
        | 0x3400..=0x4DBF   // CJK unified ideographs extension A
        | 0x4E00..=0x9FFF   // CJK unified ideographs
        | 0xA960..=0xA97F   // Hangul Jamo extended A
        | 0xAC00..=0xD7AF   // Hangul syllables
        | 0xF900..=0xFAFF   // CJK compatibility ideographs
        | 0xFF01..=0xFF60   // full-width ASCII (，：（） live here)
        | 0x20000..=0x2FA1F) // CJK extensions B–F, compatibility supplement
}

/// Truncate oversized tool-result `content` to `cap` bytes with the given
/// `truncate` strategy.
/// Returns the input unchanged for non-tool messages and for `cap == 0`
/// (the explicit-off sentinel). Handles both wire formats:
///
/// - OpenAI / simple: `{"role":"tool","content":"<string>"}` — the string
///   is truncated in place.
/// - Anthropic: `{"role":"user","content":[{"type":"tool_result",
///   "content":"<string>"}, …]}` — each `tool_result` block's string
///   `content` is truncated. Non-`tool_result` blocks (text/image) and
///   block-level array `content` are left untouched (rare for tool
///   results, and the truncate_text marker is string-specific).
fn cap_tool_result_content_with(
    msg: &serde_json::Value,
    cap: usize,
    truncate: fn(&str, usize) -> String,
) -> serde_json::Value {
    if cap == 0 {
        return msg.clone();
    }
    let mut new_msg = msg.clone();
    let role = new_msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

    match role {
        "tool" => {
            let Some(content) = new_msg.get("content").and_then(|c| c.as_str()) else {
                return new_msg;
            };
            let capped = truncate(content, cap);
            if capped == content {
                return new_msg;
            }
            if let Some(obj) = new_msg.as_object_mut()
                && let Some(v) = obj.get_mut("content")
            {
                *v = serde_json::Value::String(capped);
            }
        }
        "user" => {
            let Some(blocks) = new_msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
                return new_msg;
            };
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                    continue;
                }
                let Some(content) = block.get("content").and_then(|c| c.as_str()) else {
                    continue;
                };
                let capped = truncate(content, cap);
                if capped == content {
                    continue;
                }
                if let Some(obj) = block.as_object_mut()
                    && let Some(v) = obj.get_mut("content")
                {
                    *v = serde_json::Value::String(capped);
                }
            }
        }
        _ => {}
    }
    new_msg
}

/// Test-only debug dump — human-readable, region-labeled view of the
/// wire payload. Used by the test suite to verify the region partition
/// (`dump_labels_*` tests). Production debug dumps go through
/// `session::append_wire_payload_dump` instead, which writes
/// structured JSONL — see `docs/architecture/context.md` / Debug dump.
#[cfg(test)]
fn dump_send_context(
    provider: &dyn Provider,
    raw_context: &[serde_json::Value],
    prior_len: usize,
    strategy: &ContextStrategyConfig,
) -> String {
    let (sent, regions) =
        build_send_context_with_regions(provider, raw_context, prior_len, strategy);
    // Report the cap that is actually applied: printing the configured
    // value would show `turn_cap=3` for a turn really trimmed at the
    // edge-keeping floor.
    let cap_note = match strategy.turn_tool_result_cap {
        // `0` is the off sentinel: it maps to itself, so the comparison
        // alone is enough — a configured value only gets a note when the
        // applied one is larger.
        Some(c) if c < jyc_types::channel::effective_turn_cap(Some(c)) => {
            format!(" (lifted from {c})")
        }
        _ => String::new(),
    };
    let mut s = format!(
        "=== context sent to LLM (strategy={:?} window={} turn_cap={}{} max_hist={:?}) ===\n\
         {} msgs, ~{} bytes\n\n",
        strategy.mode,
        strategy.window,
        jyc_types::channel::effective_turn_cap(strategy.turn_tool_result_cap),
        cap_note,
        strategy.max_history_tokens,
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

#[cfg(test)]
fn append_region_summary(
    s: &mut String,
    sent: &[serde_json::Value],
    regions: &[u8],
    is_full: bool,
) {
    if is_full {
        s.push_str("(Full mode — the whole context, no windowing)\n");
        for (i, msg) in sent.iter().enumerate() {
            s.push_str(&format!("  [{i}] {}\n", msg_one_line(msg)));
        }
        return;
    }

    // SlidingWindow: walk through msgs, group by part, emit ASCII box per
    // part. The headers use the same terms as the docs — "history" and
    // "current turn" — never a numbered shorthand, so a dump can be read
    // without a legend.
    let region_headers = [
        (1u8, "History — text-only turns, calls/results folded out"),
        (3u8, "Current-turn checkpoint — older work, text-only"),
        (2u8, "Current turn — recent suffix, verbatim"),
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

#[cfg(test)]
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
    use crate::types::Message;
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
    /// `build_send_context_with_regions` never emits empty text blocks — which Anthropic
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

    fn sliding_cfg() -> ContextStrategyConfig {
        ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        }
    }

    /// Append one assistant tool-call step and its result. The assistant
    /// carries reasoning twice, exactly as the OpenAI-compatible builder
    /// persists it.
    fn push_interaction(ctx: &mut Vec<serde_json::Value>, i: usize, reasoning: &str) {
        let (open, close) = (concat!("<", "think>"), concat!("</", "think>"));
        ctx.push(json!({
            "role": "assistant",
            "content": format!("{open}{reasoning}{close}\n\nstep {i}"),
            "reasoning_content": reasoning,
            "tool_calls": [{
                "id": format!("call-{i}"),
                "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }]
        }));
        ctx.push(json!({
            "role": "tool",
            "tool_call_id": format!("call-{i}"),
            "content": format!("RESULT-{i}")
        }));
    }

    fn current_with_interactions(n: usize, reasoning: &str) -> Vec<serde_json::Value> {
        let mut ctx = vec![json!({"role": "user", "content": "task"})];
        for i in 0..n {
            push_interaction(&mut ctx, i, reasoning);
        }
        ctx
    }

    #[test]
    fn current_turn_has_no_checkpoint_through_thirty_two_interactions() {
        let ctx = current_with_interactions(32, "secret");
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, 0, &sliding_cfg());
        assert_eq!(sent.len(), ctx.len());
        assert!(
            regions.iter().all(|r| *r == 2),
            "no checkpoint before the interval: {regions:?}"
        );
    }

    #[test]
    fn custom_interval_moves_the_checkpoint_boundary() {
        // interval = 2: three interactions fold the first two and leave the
        // latest raw. This is the knob the default 32 stands in for — the
        // boundary moves with it, and only the latest interaction is spared.
        let ctx = current_with_interactions(3, "secret");
        let cfg = ContextStrategyConfig {
            checkpoint_interval: 2,
            ..sliding_cfg()
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, 0, &cfg);
        assert!(
            regions.contains(&3),
            "a shorter interval must fold: {regions:?}"
        );
        let raw_start = regions
            .iter()
            .position(|r| *r == 2)
            .expect("the latest interaction stays raw");
        assert_eq!(sent[raw_start], ctx[5], "only the latest step stays raw");
        assert_eq!(sent[raw_start + 1], ctx[6]);

        // interval = 4 over the same three interactions: under the interval,
        // so nothing folds.
        let cfg = ContextStrategyConfig {
            checkpoint_interval: 4,
            ..sliding_cfg()
        };
        let (_, regions) = build_send_context_with_regions(&prov(), &ctx, 0, &cfg);
        assert!(
            regions.iter().all(|r| *r == 2),
            "under the interval nothing folds: {regions:?}"
        );
    }

    #[test]
    fn zero_interval_disables_the_current_turn_checkpoint() {
        // 0 is the off sentinel: the whole turn rides verbatim, bounded only
        // by turn_tool_result_cap. The guard matters — `interval` is also the
        // divisor, so an unguarded 0 would panic instead of disabling.
        let ctx = current_with_interactions(40, "secret");
        let cfg = ContextStrategyConfig {
            checkpoint_interval: 0,
            ..sliding_cfg()
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, 0, &cfg);
        assert_eq!(sent.len(), ctx.len());
        assert!(
            regions.iter().all(|r| *r == 2),
            "the checkpoint must be off: {regions:?}"
        );
        let wire = serde_json::to_string(sent.as_ref()).unwrap();
        assert!(
            !wire.contains(TURN_CHECKPOINT_MARKER_TEXT),
            "no checkpoint marker without folding: {wire}"
        );
    }

    #[test]
    fn current_turn_checkpoint_folds_older_interactions_only() {
        let ctx = current_with_interactions(33, "secret");
        let (cold, cold_regions) =
            build_send_context_with_regions(&prov(), &ctx, 0, &sliding_cfg());
        let mut cache = SendContextCache::default();
        let (warm, warm_regions) =
            build_send_context_cached(&prov(), &ctx, 0, &sliding_cfg(), &mut cache);
        assert_eq!(cold, warm, "cold and cached builds must agree");
        assert_eq!(cold_regions, warm_regions);
        assert_eq!(cache.checkpoint_rebuilt(), Some(true));

        let sent = warm.into_owned();
        let raw_start = warm_regions
            .iter()
            .position(|r| *r == 2)
            .expect("the latest interaction stays raw");
        assert!(
            warm_regions[..raw_start].contains(&3),
            "older work must be checkpointed: {warm_regions:?}"
        );
        assert_eq!(
            sent[0],
            json!({"role": "user", "content": TURN_CHECKPOINT_MARKER_TEXT})
        );
        assert_eq!(
            sent[raw_start], ctx[65],
            "the latest assistant step stays verbatim"
        );
        assert_eq!(
            sent[raw_start + 1],
            ctx[66],
            "its tool result stays verbatim"
        );

        let folded = serde_json::to_string(&sent[..raw_start]).unwrap();
        assert!(!folded.contains("secret"), "reasoning leaked: {folded}");
        assert!(!folded.contains("RESULT-"), "tool result leaked: {folded}");
        assert!(folded.contains("step 0") && folded.contains("step 31"));
        assert!(!folded.contains("step 32"), "latest step must stay raw");
    }

    #[test]
    fn current_turn_checkpoint_is_stable_between_boundaries() {
        let mut ctx = current_with_interactions(33, "secret");
        let mut cache = SendContextCache::default();
        let _ = build_send_context_cached(&prov(), &ctx, 0, &sliding_cfg(), &mut cache);
        let first = cache.checkpoint.clone();
        assert_eq!(cache.checkpoint_rebuilt(), Some(true));

        push_interaction(&mut ctx, 33, "secret");
        let _ = build_send_context_cached(&prov(), &ctx, 0, &sliding_cfg(), &mut cache);
        assert_eq!(cache.checkpoint, first);
        assert_eq!(cache.checkpoint_rebuilt(), Some(false));

        for i in 34..65 {
            push_interaction(&mut ctx, i, "secret");
        }
        let _ = build_send_context_cached(&prov(), &ctx, 0, &sliding_cfg(), &mut cache);
        assert_ne!(cache.checkpoint, first, "65 interactions advance once");
        assert_eq!(cache.checkpoint_rebuilt(), Some(true));
    }

    #[test]
    fn current_turn_checkpoint_uses_provider_wire_shape() {
        let mut ctx = current_with_interactions(33, "folded-reasoning");
        // The latest step is retained verbatim, so give it a distinct
        // reasoning marker: only the folded marker may disappear from the wire.
        let last_assistant = ctx
            .iter()
            .rposition(|msg| msg["role"] == "assistant")
            .expect("fixture has assistant steps");
        ctx[last_assistant]["reasoning_content"] = serde_json::json!("retained-reasoning");
        ctx[last_assistant]["content"] = serde_json::json!(format!(
            "{open}retained-reasoning{close}\n\nstep 32\n",
            open = concat!("<", "think", ">"),
            close = concat!("</", "think", ">"),
        ));

        let (sent, regions) =
            build_send_context_with_regions(&anthropic(), &ctx, 0, &sliding_cfg());
        assert!(
            regions.contains(&3),
            "checkpoint region missing: {regions:?}"
        );
        for (msg, region) in sent.iter().zip(&regions) {
            if *region != 3 {
                continue;
            }
            let blocks = msg["content"]
                .as_array()
                .expect("checkpoint uses Anthropic content blocks");
            assert!(!blocks.is_empty(), "empty Anthropic block: {msg}");
        }

        // The retained raw suffix is still chat-shaped (the journal may predate
        // a provider switch), so exercising both conversions covers the mixed
        // payload the agent loop actually sends.
        let openai = serde_json::to_string(&crate::provider::filter_valid_messages(&sent)).unwrap();
        let anthropic = serde_json::to_string(&crate::provider::filter_valid_messages(
            &crate::provider::chat_to_anthropic_messages(&sent),
        ))
        .unwrap();
        for wire in [&openai, &anthropic] {
            assert!(
                !wire.contains("folded-reasoning"),
                "folded reasoning reached the wire: {wire}"
            );
            assert!(
                !wire.contains("RESULT-0"),
                "folded tool result reached the wire: {wire}"
            );
            assert!(
                wire.contains("retained-reasoning"),
                "retained reasoning was dropped: {wire}"
            );
            assert!(
                wire.contains("RESULT-32") && wire.contains("step 32"),
                "the retained assistant/tool-result pair was dropped: {wire}"
            );
        }
    }

    #[test]
    fn full_mode_keeps_a_long_turn_verbatim() {
        let ctx = current_with_interactions(33, "secret");
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::Full,
            window: 10,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, 0, &cfg);
        assert!(matches!(sent, Cow::Borrowed(_)));
        assert_eq!(sent.len(), ctx.len());
        assert!(regions.iter().all(|r| *r == 1));
    }

    #[test]
    fn composition_full_mode_is_one_full_region() {
        let ctx = current_with_interactions(3, "secret");
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::Full,
            ..sliding_cfg()
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, 0, &cfg);
        let rows = context_composition(sent.as_ref(), &regions, &cfg.mode);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].region, ContextRegion::Full);
        assert_eq!(rows[0].messages, sent.len() as u64);
        let expected_bytes: u64 = sent
            .iter()
            .map(|msg| serde_json::to_vec(msg).unwrap().len() as u64)
            .sum();
        let expected_tokens: u64 = sent.iter().map(|msg| est_message_tokens(msg) as u64).sum();
        assert_eq!(rows[0].bytes, expected_bytes);
        assert_eq!(rows[0].estimated_tokens, expected_tokens);
    }

    #[test]
    fn composition_without_checkpoint_splits_history_and_current() {
        let prior = vec![
            json!({"role": "user", "content": "previous task"}),
            json!({"role": "assistant", "content": "previous answer"}),
        ];
        let prior_len = prior.len();
        let mut ctx = prior;
        ctx.extend(current_with_interactions(32, "secret"));
        let (sent, regions) =
            build_send_context_with_regions(&prov(), &ctx, prior_len, &sliding_cfg());
        assert!(
            !regions.contains(&3),
            "fixture must stay below the interval"
        );

        let rows = context_composition(sent.as_ref(), &regions, &ContextStrategy::SlidingWindow);
        assert_eq!(
            rows.iter().map(|r| r.region).collect::<Vec<_>>(),
            vec![ContextRegion::History, ContextRegion::Current]
        );
        assert_eq!(
            rows[0].messages,
            regions.iter().filter(|r| **r == 1).count() as u64
        );
        assert_eq!(
            rows[1].messages,
            regions.iter().filter(|r| **r == 2).count() as u64
        );
        assert_eq!(
            rows.iter().map(|r| r.messages).sum::<u64>(),
            sent.len() as u64
        );
    }

    #[test]
    fn composition_splits_current_turn_checkpoint_from_raw_suffix() {
        let ctx = current_with_interactions(33, "secret");
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, 0, &sliding_cfg());
        assert!(regions.contains(&3), "fixture must cross the interval");

        let rows = context_composition(sent.as_ref(), &regions, &ContextStrategy::SlidingWindow);
        assert_eq!(
            rows.iter().map(|r| r.region).collect::<Vec<_>>(),
            vec![ContextRegion::Checkpoint, ContextRegion::Current]
        );
        assert_eq!(
            rows[0].messages,
            regions.iter().filter(|r| **r == 3).count() as u64
        );
        assert_eq!(
            rows[1].messages,
            regions.iter().filter(|r| **r == 2).count() as u64
        );
        let expected_bytes: u64 = sent
            .iter()
            .zip(&regions)
            .filter(|(_, region)| **region == 3)
            .map(|(msg, _)| serde_json::to_vec(msg).unwrap().len() as u64)
            .sum();
        assert_eq!(rows[0].bytes, expected_bytes);
        assert!(rows[0].estimated_tokens > 0 && rows[1].estimated_tokens > 0);
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
        let rendered = crate::session::render_raw_context_as_text(&ctx);
        assert!(rendered.contains("USER: fix bug"));
        // One turn → one merged ASSISTANT block: first step's text, the
        // turn's tool calls, then the reply. Tool results are not part of
        // the transcript: no note machinery renders them any more.
        assert!(rendered.contains("ASSISTANT: I'll start"));
        assert!(rendered.contains("bash()"));
        assert!(!rendered.contains("output"));
        assert!(rendered.contains("Done."));
    }

    #[test]
    fn truncates_long_tool_args() {
        let long = "x".repeat(2000);
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{
                "id": "1", "type": "function",
                "function": {
                    "name": "bash",
                    "arguments": format!("{{\"command\":\"{long}\"}}"),
                }
            }]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "ok"}),
        ];
        let rendered = crate::session::render_raw_context_as_text(&ctx);
        // The argument value cap (`TOOL_ARG_MAX` = 200) applies, plus the
        // header and USER/ASSISTANT framing.
        assert!(rendered.len() < 400);
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
        let rendered = crate::session::render_raw_context_as_text(&ctx);
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
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg).0;
        assert!(matches!(sent, Cow::Borrowed(_)));
        assert_eq!(sent.len(), ctx.len());
    }

    #[test]
    fn sliding_window_folds_prior_turns_to_text() {
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

        // Every prior turn — u1/a1, u2 with its tool call, u3/a3 — folds
        // to text: tool calls and results do not reach the wire.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 2,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg).0;
        let sent = sent.into_owned();

        // 1. History marker, then every prior turn in pure text (tool
        //    calls dropped). Nothing trims the history block: the window
        //    bound is the loader's, not this function's.
        assert_eq!(
            sent[0],
            json!({"role": "user", "content": crate::session::HISTORY_MARKER_TEXT})
        );
        assert_eq!(sent[1], json!({"role": "user", "content": "u1"}));
        assert_eq!(sent[2], json!({"role": "assistant", "content": "a1"}));
        assert_eq!(sent[3], json!({"role": "user", "content": "u2"}));
        assert_eq!(sent[4], json!({"role": "assistant", "content": "a2"}));
        assert_eq!(sent[5], json!({"role": "user", "content": "u3"}));
        assert_eq!(sent[6], json!({"role": "assistant", "content": "a3"}));

        // 2. Current turn verbatim (tool_calls preserved).
        assert_eq!(sent[7], current[0]);
        assert_eq!(sent[8], current[1]);
        assert_eq!(sent[9], current[2]);
    }

    #[test]
    fn sliding_window_prior_shorter_than_raw_context() {
        // A defensive boundary: prior_len never exceeds raw_context.len().
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
        ];
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, 99, &cfg)
            .0
            .into_owned();
        // boundary clamps to raw_context.len(), so the whole ctx is
        // treated as prior. Expected: history marker + 1 complete pair
        // (u1, a1) = 3 messages.
        assert_eq!(sent.len(), 3);
        assert_eq!(
            sent[0],
            json!({"role": "user", "content": crate::session::HISTORY_MARKER_TEXT})
        );
        assert_eq!(sent[1], json!({"role": "user", "content": "u1"}));
        assert_eq!(sent[2], json!({"role": "assistant", "content": "a1"}));
    }

    #[test]
    fn history_stays_a_byte_stable_prefix_across_turns() {
        // The property the strategy exists for: history is text-only, so
        // every turn's history block is a prefix of the next turn's — the
        // provider's cached prefix reaches the whole of it, and only the
        // turn that just ended is re-encoded (as it graduates into the
        // text-only block) together with the current turn.
        let prov = prov();
        let cfg = ContextStrategyConfig {
            ..ContextStrategyConfig::default()
        };
        let turn = |i: usize| {
            [
                json!({"role": "user", "content": format!("u{i}")}),
                json!({"role": "assistant", "content": format!("a{i}")}),
            ]
        };
        // Turn k's payload: prior = turns 1..k-1, current turn = k.
        let payload = |k: usize| {
            let mut ctx = Vec::new();
            for i in 1..k {
                ctx.extend(turn(i));
            }
            let prior_len = ctx.len();
            ctx.extend(turn(k));
            let (sent, regions) = build_send_context_with_regions(&prov, &ctx, prior_len, &cfg);
            (sent.into_owned(), regions)
        };

        let (at_5, regions_5) = payload(5); // prior 1..4 → history = 1..4
        let (at_6, regions_6) = payload(6); // prior 1..5 → history = 1..5
        let region1_5 = regions_5.iter().take_while(|&&r| r == 1).count();
        let region1_6 = regions_6.iter().take_while(|&&r| r == 1).count();

        assert!(
            regions_5.ends_with(&[2, 2]) && regions_6.ends_with(&[2, 2]),
            "the current turn is the only region-2 block"
        );
        assert!(region1_5 > 0, "the marker and four turns");
        assert_eq!(region1_6, region1_5 + 2, "history grew by exactly one turn");
        assert_eq!(
            at_6[..region1_5],
            at_5[..region1_5],
            "the whole history block is a byte-stable prefix from one turn to the next"
        );
        // Everything past the history block is the current turn, and the
        // history block's own tail is the turn that just graduated into it.
        assert_eq!(at_6[region1_5]["content"], "u5");
        assert_eq!(at_6[region1_5 + 1]["content"], "a5");
        assert_eq!(at_5[region1_5]["content"], "u5");
        assert_eq!(at_6[region1_6]["content"], "u6");
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
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, 0, &cfg)
            .0
            .into_owned();
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

        // Both prior turns — including u1's tool_use + tool_result —
        // compact to text.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 4,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let sent = build_send_context_with_regions(&anthropic(), &ctx, prior_len, &cfg)
            .0
            .into_owned();

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

        // u1 turn compacted: text merged ("a1\ncalling bash"), no tool
        // blocks.
        assert!(
            sent.iter()
                .any(|m| m["role"] == "user" && m["content"][0]["text"].as_str() == Some("u1"))
        );
        assert!(sent.iter().any(|m| m["role"] == "assistant"
            && m["content"][0]["text"].as_str() == Some("a1\ncalling bash")));
        // u2 turn compacted the same way.
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

    /// A prior turn's tool calls and results do not reach the wire: the
    /// history block is text-only, and only the current turn is verbatim.
    #[test]
    fn sliding_window_folds_tool_calls_out_of_history() {
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

        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg)
            .0
            .into_owned();

        // History: marker, then both prior turns as text only — the
        // structured tool-call step and its result are gone.
        assert_eq!(
            sent,
            vec![
                json!({"role": "user", "content": crate::session::HISTORY_MARKER_TEXT}),
                json!({"role": "user", "content": "u1"}),
                json!({"role": "assistant", "content": "a1"}),
                json!({"role": "user", "content": "u2"}),
                json!({"role": "assistant", "content": "calling bash\ndone"}),
                current[0].clone(),
            ]
        );
    }

    /// `with_regions` and `dump_send_context` — verify region partition is
    /// stable across `Full` and `SlidingWindow` strategies.
    #[test]
    fn dump_labels_full_mode_as_one_block() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
        ];
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::Full,
            window: 10,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
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
            dump.contains("no windowing"),
            "Full dump should say the context is whole: {dump}"
        );
        assert!(dump.contains("\"u1\""), "missing msg preview: {dump}");
    }

    #[test]
    fn dump_labels_sliding_window_two_parts() {
        // history = marker + (u1, a1) + (u2, a2); current turn = (u3).
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
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, prior.len(), &cfg);
        assert_eq!(sent.len(), 6);
        assert_eq!(regions, vec![1, 1, 1, 1, 1, 2]);
        assert_eq!(
            sent[0]["content"].as_str().unwrap(),
            crate::session::HISTORY_MARKER_TEXT
        );

        let dump = dump_send_context(&prov(), &ctx, prior.len(), &cfg);
        assert!(
            dump.contains("History — text-only turns"),
            "missing the history box: {dump}"
        );
        assert!(
            dump.contains("Current turn — recent suffix"),
            "missing the current-turn box: {dump}"
        );
        assert!(
            !dump.contains('\u{2460}') && !dump.contains('\u{2461}'),
            "dump must use plain terms, not the numbered shorthand: {dump}"
        );
        assert!(
            dump.contains("--- wire payload ---"),
            "missing payload section: {dump}"
        );
    }

    /// The cached history block must be indistinguishable from a cold
    /// build: same payload and labels on every iteration of a turn (the
    /// current turn grows, the cached prefix does not), and a rebuild once
    /// the prefix itself changes at the next turn.
    #[test]
    fn cached_build_matches_cold_build_across_calls() {
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ];
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 2,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.push(json!({"role": "user", "content": "u3"}));

        let mut cache = SendContextCache::default();
        let (first, first_regions) =
            build_send_context_cached(&prov(), &ctx, prior_len, &cfg, &mut cache);
        let (cold, cold_regions) = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg);
        assert_eq!(first.as_ref(), cold.as_ref());
        assert_eq!(first_regions, cold_regions);

        // Another iteration of the same turn: the current turn grew, the
        // cached prefix did not — the history block is reused as-is.
        ctx.push(json!({"role": "assistant", "content": "a3"}));
        let (second, second_regions) =
            build_send_context_cached(&prov(), &ctx, prior_len, &cfg, &mut cache);
        let (cold2, cold2_regions) =
            build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg);
        assert_eq!(second.as_ref(), cold2.as_ref());
        assert_eq!(second_regions, cold2_regions);

        // Next turn: the prefix grew, so the cached block must be rebuilt.
        let next_prior_len = ctx.len();
        ctx.push(json!({"role": "user", "content": "u4"}));
        let (third, third_regions) =
            build_send_context_cached(&prov(), &ctx, next_prior_len, &cfg, &mut cache);
        let (cold3, cold3_regions) =
            build_send_context_with_regions(&prov(), &ctx, next_prior_len, &cfg);
        assert_eq!(third.as_ref(), cold3.as_ref());
        assert_eq!(third_regions, cold3_regions);
    }

    /// The equivalence test above passes whether the regions are reused or
    /// rebuilt (a rebuild derives the same bytes), so prove reuse directly: a
    /// sentinel spliced into the cache can only reach the payload if the next
    /// call reuses it.
    #[test]
    fn cached_build_reuses_history_regions() {
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ];
        // `window` does not trim the history block, so both pairs stay and
        // the sentinel proves reuse of a populated block, not an empty one.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 1,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.push(json!({"role": "user", "content": "u3"}));

        let mut cache = SendContextCache::default();
        let _ = build_send_context_cached(&prov(), &ctx, prior_len, &cfg, &mut cache);
        assert_eq!(
            cache.history_rebuilt(),
            Some(true),
            "a cold cache must build"
        );

        // Spliced into the cache, not into `raw_context`: a rebuild derives
        // the regions from `raw_context` and would drop it.
        cache
            .history
            .push(json!({"role": "user", "content": "CACHED-SENTINEL"}));
        let (warm, _) = build_send_context_cached(&prov(), &ctx, prior_len, &cfg, &mut cache);
        assert!(
            warm.iter()
                .any(|m| m.get("content").and_then(|c| c.as_str()) == Some("CACHED-SENTINEL")),
            "same prefix: the cached regions must be reused"
        );
        assert_eq!(cache.history_rebuilt(), Some(false));

        // The next turn moves the prefix: rebuild, so the sentinel is gone.
        ctx.push(json!({"role": "assistant", "content": "a3"}));
        let next_prior_len = ctx.len();
        let (rebuilt, _) =
            build_send_context_cached(&prov(), &ctx, next_prior_len, &cfg, &mut cache);
        assert!(
            !rebuilt
                .iter()
                .any(|m| m.get("content").and_then(|c| c.as_str()) == Some("CACHED-SENTINEL")),
            "a new prefix must rebuild the regions"
        );
        assert_eq!(cache.history_rebuilt(), Some(true));
    }

    #[test]
    fn history_marker_present_whenever_prior_is_non_empty() {
        // Every prior turn now goes into the same text-only block, so the
        // marker is emitted exactly when there is history to introduce.
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
        ];
        let current = vec![json!({"role": "user", "content": "u2"})];
        let mut ctx = prior.clone();
        ctx.extend(current.clone());
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 1,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, prior.len(), &cfg);
        assert_eq!(sent.len(), 4);
        assert_eq!(regions, vec![1, 1, 1, 2]);
        assert_eq!(
            sent[0],
            json!({"role": "user", "content": crate::session::HISTORY_MARKER_TEXT})
        );
        assert_eq!(sent[1], json!({"role": "user", "content": "u1"}));
        assert_eq!(sent[2], json!({"role": "assistant", "content": "a1"}));
        assert_eq!(sent[3], current[0]);
    }

    #[test]
    fn history_marker_is_static_and_provider_agnostic() {
        let marker_text = crate::session::HISTORY_MARKER_TEXT;
        let prior1 = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ];
        let prior2 = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
            json!({"role": "user", "content": "u3"}),
            json!({"role": "assistant", "content": "a3"}),
        ];

        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 2,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };

        let (sent1, _) = build_send_context_with_regions(&prov(), &prior1, prior1.len(), &cfg);
        let (sent2, _) = build_send_context_with_regions(&prov(), &prior2, prior2.len(), &cfg);

        let marker1 = sent1[0]["content"].as_str().unwrap();
        let marker2 = sent2[0]["content"].as_str().unwrap();
        assert_eq!(marker1, marker_text);
        assert_eq!(marker2, marker_text);

        // Same marker text under the Anthropic provider shape as well.
        let (sent_a, _) =
            build_send_context_with_regions(&anthropic(), &prior1, prior1.len(), &cfg);
        let first_block = sent_a[0]["content"].as_array().unwrap()[0].clone();
        assert_eq!(first_block["text"].as_str().unwrap(), marker_text);
    }

    #[test]
    fn full_mode_has_no_history_marker() {
        let ctx = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
        ];
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::Full,
            window: 10,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let (sent, _) = build_send_context_with_regions(&prov(), &ctx, 0, &cfg);
        let texts: Vec<&str> = sent.iter().filter_map(|m| m["content"].as_str()).collect();
        assert!(
            !texts
                .iter()
                .any(|t| t.starts_with(crate::session::HISTORY_MARKER_TEXT)),
            "Full mode must not inject the compacted-history marker, got: {texts:?}"
        );
    }

    /// With `turn_tool_result_cap` set, the current turn is bounded too —
    /// head and tail kept, middle dropped with a paging marker — while the
    /// history block stays text-only and unbounded by it.
    #[test]
    fn build_send_context_caps_current_turn_when_turn_cap_set() {
        let big = "z".repeat(4000) + "ERROR: the tail matters";
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
        ];
        let current = vec![
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": big}),
        ];
        let mut ctx = prior.clone();
        ctx.extend(current);
        let prior_len = prior.len();

        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            extend_rate: 0.5,
            // Only the current turn is capped, so anything trimmed below
            // must be the turn cap.
            turn_tool_result_cap: Some(600),
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let (out, regions) = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg);
        let wire = serde_json::to_string(out.as_ref()).unwrap();

        assert!(
            wire.contains("dropped from the middle"),
            "turn cap must produce the middle-drop marker: {wire}"
        );
        assert!(
            wire.contains("ERROR: the tail matters"),
            "turn cap must keep the tail — that is the point of it: {wire}"
        );
        assert!(
            !wire.contains(&"z".repeat(4000)),
            "the oversized middle must be gone: {wire}"
        );
        let tool_idx = out
            .iter()
            .position(|m| m["role"] == "tool")
            .expect("tool result must be in output");
        assert_eq!(
            regions[tool_idx], 2,
            "capped result stays in the current turn"
        );
        // The capped result must respect the byte bound it was given.
        let content = out[tool_idx]["content"].as_str().unwrap();
        assert!(
            content.len() <= 600,
            "current-turn content must honour the cap: {}",
            content.len()
        );
    }

    /// `Some(0)` is the explicit-off sentinel and must behave exactly like
    /// `None` — the "switch off" path the command writes.
    #[test]
    fn build_send_context_turn_cap_zero_is_off() {
        let big = "q".repeat(4000);
        let prior = vec![json!({"role": "user", "content": "u1"})];
        let current = vec![
            json!({"role": "user", "content": "u2"}),
            json!({"role": "tool", "tool_call_id": "1", "content": big}),
        ];
        let mut ctx = prior.clone();
        ctx.extend(current);
        let prior_len = prior.len();

        let none_cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let zero_cfg = ContextStrategyConfig {
            turn_tool_result_cap: Some(0),
            max_history_tokens: None,
            ..none_cfg.clone()
        };
        let (a, _) = build_send_context_with_regions(&prov(), &ctx, prior_len, &none_cfg);
        let (b, _) = build_send_context_with_regions(&prov(), &ctx, prior_len, &zero_cfg);
        assert_eq!(
            serde_json::to_string(a.as_ref()).unwrap(),
            serde_json::to_string(b.as_ref()).unwrap(),
            "Some(0) must behave identically to None"
        );
        assert!(
            serde_json::to_string(b.as_ref()).unwrap().contains(&big),
            "turn cap 0 must leave the current turn whole"
        );
    }

    /// A configured cap below the edge-keeping floor is lifted, not
    /// honoured: `turn_tool_result_cap = 3` in a config file or an old
    /// override cannot gut every current-turn result. The lift is visible
    /// in the output — the result keeps its tail instead of degrading to a
    /// head-only cut — and the bound it reports is the floors, not the 3.
    #[test]
    fn build_send_context_lifts_a_below_floor_turn_cap() {
        // Ends in a single distinctive byte: at the floor the tail is one
        // byte long, which is what makes "it kept both ends" observable.
        let big = "h".repeat(150) + "TAIL!";
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
        ];
        let current = vec![
            json!({"role": "user", "content": "u2"}),
            json!({"role": "tool", "tool_call_id": "1", "content": big}),
        ];
        let mut ctx = prior.clone();
        ctx.extend(current);
        let prior_len = prior.len();

        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            extend_rate: 0.5,
            // A slot-shifted leftover command: 3 bytes would leave nothing.
            turn_tool_result_cap: Some(3),
            max_history_tokens: None,
            ..ContextStrategyConfig::default()
        };
        let (out, _) = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg);
        let wire = serde_json::to_string(out.as_ref()).unwrap();

        assert!(
            wire.contains("dropped from the middle"),
            "the lifted cap must still produce the paging marker: {wire}"
        );
        let tool_idx = out
            .iter()
            .position(|m| m["role"] == "tool")
            .expect("tool result must be in output");
        let content = out[tool_idx]["content"].as_str().unwrap();
        assert!(
            content.len() <= crate::session::MIN_EDGE_KEEPING_CAP,
            "the lifted cap bounds the content too: {} bytes",
            content.len()
        );
        // Both ends survive: the head byte and the tail byte. A head-only
        // cut — what an unlifted 3 would have produced — loses the tail and
        // uses the other marker.
        assert!(
            content.starts_with('h') && content.ends_with('!'),
            "the lift must keep both ends at the floor: {content:?}"
        );
        assert!(
            !wire.contains("truncated"),
            "a head-only cut would say 'truncated' instead: {wire}"
        );
        // The dump reports the applied cap, not the configured one, and says
        // where it came from — printing `turn_cap=3` would describe a turn
        // the model never saw.
        let dump = dump_send_context(&prov(), &ctx, prior_len, &cfg);
        assert!(
            dump.contains(&format!(
                "turn_cap={} (lifted from 3)",
                crate::session::MIN_EDGE_KEEPING_CAP
            )),
            "the dump must report the applied cap: {}",
            dump.lines().next().unwrap_or("")
        );
    }

    // ── max_history_tokens ───────────────────────────────────────────

    /// The estimator is the CJK-aware one this project needs: a Chinese
    /// glyph costs about what a short English word costs, so counting it as
    /// a quarter of its (3-byte) length undercounts a mixed transcript by a
    /// quarter. Full-width punctuation counts as wide too — it is the most
    /// common non-ideograph character in Chinese text.
    #[test]
    fn est_text_tokens_counts_wide_chars_per_char_narrow_at_quarter_byte() {
        assert_eq!(est_text_tokens("你好，世界"), 5);
        assert_eq!(est_text_tokens("abcde"), 2, "rounding is up, never down");
        assert_eq!(est_text_tokens(""), 0);
    }

    /// The wide-character ranges have to land where the actual characters
    /// are, or the estimate quietly drifts by a factor of four on the text
    /// that matters most here. Both edges of each boundary are pinned.
    #[test]
    fn wide_char_boundaries_are_where_the_characters_are() {
        // Full-width forms: （U+FF08） and ！（U+FF01） are wide; the
        // half-width katakana that starts right after them is not.
        assert!(is_wide_char('！'));
        assert!(is_wide_char('（'));
        assert!(
            !is_wide_char('\u{FF61}'),
            "half-width katakana is ASCII-cost"
        );
        // CJK unified ideographs: ext-A ends at U+4DBF, base block starts at
        // U+4E00.
        assert!(is_wide_char('\u{4DBF}'));
        assert!(is_wide_char('\u{4E00}'));
        // Ideographic space sits below the radicals block.
        assert!(is_wide_char('\u{3000}'));
        // Plain ASCII prose stays narrow.
        assert!(!is_wide_char('a'));
        assert!(!is_wide_char('7'));
        assert!(!is_wide_char(' '));
        assert_eq!(
            est_text_tokens("！\u{FF61}"),
            2,
            "！ is one wide char; half-width katakana is 3 bytes, which still rounds up to a token"
        );
    }

    /// Strings nested in blocks (tool results, content arrays) are counted;
    /// object keys and bare numbers are not — they cost less than the text
    /// they carry, and this bounds history rather than billing it.
    #[test]
    fn est_message_tokens_walks_nested_strings_plus_per_message_constant() {
        let msg = json!({"role": "user", "content": [{"type": "text", "text": "abcdefgh"}]});
        // "user" (4 bytes → 1) + "text" (4 bytes → 1) + "abcdefgh" (8 → 2).
        assert_eq!(est_message_tokens(&msg), PER_MESSAGE_TOKENS + 4);
    }
}
