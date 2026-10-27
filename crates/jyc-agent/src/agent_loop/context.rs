//! Raw-context rendering and history compaction helpers.
//!
//! Extracted from the monolithic `agent_loop.rs`.

use std::borrow::Cow;

use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};

use crate::provider::Provider;
use crate::types::{ContentBlock, PriorDepth};

/// Region-① block capacity: how many compact pairs the block holds
/// before its head jumps forward. `(window − note_window) +
/// round(window × ext_rate)` — the classic sliding window's `window −
/// note_window` pairs plus the `ext_rate` extension. The visible history
/// span therefore oscillates between `window` and `window +
/// ext_rate × window` turns. Capped at
/// [`jyc_types::channel::MAX_BLOCK_PAIRS`] so a large window × rate
/// composition cannot inflate the per-turn payload and the reader scan
/// back towards a full-journal read.
///
/// Shared by [`prior_depth`] (the reader depth) and
/// [`build_history_regions`] (the block builder): the reader must deliver
/// exactly the pairs the builder anchors in, and the differential test
/// pins the two against drift.
fn block_capacity(strategy: &ContextStrategyConfig) -> usize {
    // `None` note_window (whole prior verbatim) leaves no compact region;
    // `usize::MAX` saturates the base to 0, matching `build_history_regions`.
    let base = strategy
        .window
        .max(1)
        .saturating_sub(strategy.note_window.unwrap_or(usize::MAX));
    let extended = base + (strategy.window.max(1) as f64 * strategy.ext_rate()).round() as usize;
    extended.min(jyc_types::channel::MAX_BLOCK_PAIRS)
}

/// Region-① jump landing: the block keeps this many pairs after a jump.
/// Constant `window − note_window`, independent of `ext_rate`, so every
/// jump returns the visible history to exactly `window` turns. With
/// `ext_rate = 0` the capacity equals the landing and the head advances
/// one pair per turn — the classic sliding window, no special case.
fn block_landing(strategy: &ContextStrategyConfig) -> usize {
    strategy
        .window
        .max(1)
        .saturating_sub(strategy.note_window.unwrap_or(usize::MAX))
}

/// How much of the prior context the strategy needs to build its payload.
///
/// The sliding window reads the last `block_capacity + note_window (+ 1)`
/// pair-producing turns — the region-① block capacity plus the verbatim
/// region's pairs. The block is anchored independently of the verbatim
/// turns (the head can sit anywhere inside the block span), so the reader
/// must cover both or a reader-slice build would see a shorter compact
/// region than a whole-journal build (the differential test pins payload
/// equality); opener-only trailing turns ride on the `openers` count. An
/// unanswered user message is an opener without a pair, and the verbatim
/// region counts openers. The `+ 1` slack covers a verbatim turn that
/// ever yields more than the single pair [`crate::session::extract_pairs`]
/// assumes today — without it one such turn would push the anchor pair
/// out of the delivered slice and silently cold-rebuild. `full` mode and
/// an unset `note_window` send the whole prior, so no bounded slice can
/// replace it.
///
/// The reader is told this before it reads: it parses only that much of
/// the journal, which is what keeps a long history cheap.
pub(crate) fn prior_depth(strategy: &ContextStrategyConfig) -> PriorDepth {
    match strategy.mode {
        ContextStrategy::Full => PriorDepth::All,
        ContextStrategy::SlidingWindow => match strategy.note_window {
            Some(openers) => PriorDepth::Window {
                pairs: block_capacity(strategy)
                    .saturating_add(openers)
                    .saturating_add(1),
                openers,
            },
            // Without a note window the whole prior is verbatim — the
            // window does not bound it.
            None => PriorDepth::All,
        },
    }
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

/// Cold build — [`build_send_context_cached`] with a throwaway cache.
///
/// Test-only: the loop passes its own [`SendContextCache`], so a turn's calls
/// reuse the history regions instead of rebuilding them. Starts from no
/// block anchor; block tests use [`build_send_context_anchor_with_regions`].
#[cfg(test)]
pub(crate) fn build_send_context_with_regions<'a>(
    provider: &dyn Provider,
    raw_context: &'a [serde_json::Value],
    prior_len: usize,
    strategy: &ContextStrategyConfig,
) -> (Cow<'a, [serde_json::Value]>, Vec<u8>) {
    let (payload, regions, _anchor) =
        build_send_context_anchor_with_regions(provider, raw_context, prior_len, strategy, None);
    (payload, regions)
}

/// Cold build that also threads the region-① block anchor in and out —
/// what the loop does across turns, without a cache to reuse.
#[cfg(test)]
fn build_send_context_anchor_with_regions<'a>(
    provider: &dyn Provider,
    raw_context: &'a [serde_json::Value],
    prior_len: usize,
    strategy: &ContextStrategyConfig,
    block_anchor: Option<crate::session::BlockAnchor>,
) -> (
    Cow<'a, [serde_json::Value]>,
    Vec<u8>,
    Option<crate::session::BlockAnchor>,
) {
    let mut cache = SendContextCache::default();
    let (payload, regions) = build_send_context_cached(
        provider,
        raw_context,
        prior_len,
        strategy,
        block_anchor,
        &mut cache,
    );
    (payload, regions, cache.block_anchor())
}

/// Cross-iteration memo for the sliding-window history regions (①+②).
///
/// The regions depend only on the persisted prefix (`raw_context[..prior_len]`)
/// and the strategy, yet the loop rebuilt them for every LLM call — a scan of
/// the whole history that deep-cloned every user/assistant message
/// (`extract_pairs`) only to keep the tail. Within a turn the prefix is
/// fixed: live injections append past `prior_len`, and the only shrink (the
/// cancellation truncation) ends the turn. The first call of a turn builds
/// the regions; the rest reuse them.
#[derive(Default)]
pub(crate) struct SendContextCache {
    /// The split point the cached regions were built from (`prior_len`
    /// clamped to the context length); `None` = cold.
    ///
    /// A length is a sound key only because the cache belongs to one
    /// `run()`: `strategy` and `provider` are fixed for it, the persisted
    /// prefix can only grow past this point (injections append after it),
    /// and its one shrink — the cancellation truncation — ends the turn. A
    /// cache that outlived a turn would need a content key.
    boundary: Option<usize>,
    region1: Vec<serde_json::Value>,
    region2: Vec<serde_json::Value>,
    /// Whether the last build rebuilt the regions (`Some(true)`), reused them
    /// (`Some(false)`), or built nothing at all (`None`, `Full` mode — the log
    /// must not read that as "reused").
    rebuilt: Option<bool>,
    /// Region-① block anchor computed at the last rebuild: the block-head
    /// pair's identity (text hashes + pair count). The caller persists it
    /// to the session file when it changed (a block jump or a cold
    /// rebuild), so the next turn continues the same block instead of
    /// re-deriving the head. Meaningless for `Full` mode, which never
    /// rebuilds.
    block_anchor: Option<crate::session::BlockAnchor>,
}

impl SendContextCache {
    /// Whether the last [`build_send_context_cached`] rebuilt the history
    /// regions, or `None` when it built nothing (`Full` mode).
    pub(crate) fn history_rebuilt(&self) -> Option<bool> {
        self.rebuilt
    }

    /// Region-① block anchor from the last rebuild — the value the caller
    /// persists when it differs from what the session file holds.
    pub(crate) fn block_anchor(&self) -> Option<crate::session::BlockAnchor> {
        self.block_anchor
    }
}

/// Build the context to send to the LLM for the next request.
///
/// The full `raw_context` is always persisted to `.jyc/agent-context.jsonl` (append-only journal)
/// untouched; this function only shapes what is sent to the LLM.
///
/// * `Full` — borrow the full context (no copy).
/// * `SlidingWindow` — three parts, in order:
///   1. The block-anchored compact pairs: everything from the persisted
///      block head (`block_anchor`) up to the verbatim region, reformatted
///      in provider wire format (string content → provider-correct content
///      shape). The head stays put while the block holds `capacity`
///      (`window − note_window + round(window × ext_rate)`) pairs or fewer,
///      so within a block this region only *appends* each turn and the
///      wire prefix stays byte-stable; only a block jump (or a cold
///      rebuild when the anchor is missing) rewrites it.
///   2. The most recent `note_window` prior turns **verbatim** (structured
///      `tool_calls` + tool results intact, like the current turn), so the
///      model keeps seeing tool calls in the wire format it is supposed to
///      emit instead of user-role text notes.
///   3. The full current turn (`raw_context[prior_len..]`) verbatim, so
///      tool calls / results stay coherent mid-loop.
///
/// Also returns a per-message region label (parallel to the wire payload)
/// so callers that need to distinguish regions — the wire-payload debug
/// dump and tests — can do so without re-running the strategy logic.
///
/// Region labels:
/// - `1` = first region — compacted history (`SlidingWindow`) or the
///   whole context (`Full` mode, no region split).
/// - `2` = second region — verbatim prior turns (`SlidingWindow` only).
/// - `3` = third region — current turn verbatim (`SlidingWindow` only).
///
/// When `strategy.tool_result_cap` is `Some(cap > 0)`, every tool result
/// in region ② is truncated to at most `cap` bytes with the standard
/// `… [truncated N bytes]` marker. Region ③ (the current turn) is
/// uncapped by default — the model is reasoning over those tool results
/// mid-loop, so they must stay complete — and bounded by the separate,
/// opt-in `strategy.turn_tool_result_cap`, which keeps head and tail
/// instead of the head alone. `Some(0)` or `None` disables capping
/// everywhere.
///
/// `strategy.max_history_tokens` is the second lever, orthogonal to
/// `window`: it caps what regions ① and ② weigh together, not how many
/// turns they hold. When the estimate is over budget the oldest history
/// goes first (see [`fit_history_to_budget`]). Region ③ is never part of
/// it. `Some(0)` or `None` sends the windowed history untouched.
///
/// `block_anchor` is the persisted region-① block head (hashes of the
/// head pair's user/assistant text, from the session file); the rebuild
/// stores the computed anchor back into `cache` for the caller to persist
/// when it changed.
///
/// `cache` lets a turn's calls reuse the history regions instead of
/// rebuilding them per call (see [`SendContextCache`]).
pub(crate) fn build_send_context_cached<'a>(
    provider: &dyn Provider,
    raw_context: &'a [serde_json::Value],
    prior_len: usize,
    strategy: &ContextStrategyConfig,
    block_anchor: Option<crate::session::BlockAnchor>,
    cache: &mut SendContextCache,
) -> (Cow<'a, [serde_json::Value]>, Vec<u8>) {
    match strategy.mode {
        ContextStrategy::Full => {
            // Nothing is built: `Full` borrows the context as it is.
            cache.rebuilt = None;
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
                let (region1, region2, anchor) = build_history_regions(
                    provider,
                    &raw_context[..boundary],
                    strategy,
                    block_anchor,
                );
                cache.region1 = region1;
                cache.region2 = region2;
                cache.block_anchor = anchor;
                cache.boundary = Some(boundary);
                cache.rebuilt = Some(true);
            } else {
                cache.rebuilt = Some(false);
            }

            let region1_len = cache.region1.len();
            let region2_len = cache.region2.len();
            let mut out = Vec::with_capacity(region1_len + region2_len + current.len());
            let mut regions = Vec::with_capacity(out.capacity());

            out.extend_from_slice(&cache.region1);
            regions.resize(region1_len, 1);
            out.extend_from_slice(&cache.region2);
            regions.resize(region1_len + region2_len, 2);

            // Current turn. Verbatim by default (`turn_tool_result_cap`
            // unset, so `0` short-circuits): the model is reasoning over
            // these tool results mid-loop, so they normally stay
            // complete. When the cap is configured, oversized results
            // keep head and tail with the middle dropped — what a result
            // is read for usually sits at the ends, and the marker tells
            // the model how to page the gap. Both sentinel and default
            // mean off; see the doc above.
            let turn_cap = strategy.turn_tool_result_cap.unwrap_or(0);
            for msg in current {
                out.push(cap_tool_result_content_with(
                    msg,
                    turn_cap,
                    crate::session::truncate_text_keeping_edges,
                ));
                regions.push(3);
            }
            (Cow::Owned(out), regions)
        }
    }
}

/// Build regions ① (compacted older pairs) and ② (recent turns verbatim) from
/// the persisted prefix, fitted to `max_history_tokens`.
///
/// Also computes the region-① block anchor (hashes of the block-head
/// pair's user/assistant text, plus the block's pair count) so the caller
/// can persist it when it changed; the next turn feeds it back in via
/// `block_anchor`.
fn build_history_regions(
    provider: &dyn Provider,
    prior: &[serde_json::Value],
    strategy: &ContextStrategyConfig,
    block_anchor: Option<crate::session::BlockAnchor>,
) -> (
    Vec<serde_json::Value>,
    Vec<serde_json::Value>,
    Option<crate::session::BlockAnchor>,
) {
    // `tool_result_cap`: Some(0) and None both mean "off" — let the
    // helper short-circuit on cap == 0.
    let cap = strategy.tool_result_cap.unwrap_or(0);

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

    // Region ① is block-anchored. The persisted anchor names the block
    // head, so within a block the region only *appends* one pair per turn
    // and the wire prefix stays byte-stable (prompt-cache friendly); the
    // head jumps forward on the turn after the block outgrew the
    // capacity, and that jump turn is the one turn where the prefix
    // breaks. A missing or unmatched anchor rebuilds the block cold,
    // holding the last `capacity` pairs.
    //
    // Capacity is the classic sliding window (`window − note_window`
    // pairs) plus the `ext_rate` extension, so the visible history grows
    // from `window` up to `window + ext_rate × window` turns and a jump
    // returns it to `window` (see `ContextStrategyConfig::ext_rate`'s
    // doc). At `ext_rate = 0` capacity equals the landing and the block
    // degenerates to the pre-block sliding window.
    let max_pairs = block_capacity(strategy);
    let landing = block_landing(strategy);

    let pairs = crate::session::extract_pairs(compact_prior);
    let pair_count = pairs.len();

    // Locate the block head: the persisted anchor pair while it is still
    // in the delivered slice, otherwise the cold tail block. Duplicate
    // text hashes to the same anchor (e.g. a topic that keeps receiving
    // "ok"), so a bare `position` would pin the *oldest* duplicate and
    // slide the block every turn. The anchor's `keep` (pair count at
    // write time) names the expected head — `pair_count − keep − 1`,
    // the `- 1` accounting for the pair that aged into the compact
    // region since the write — and the matching pair nearest it wins;
    // a legacy anchor without `keep` falls back to the match nearest
    // the tail, which is where the last turn's head must be.
    let anchor_match = block_anchor.and_then(|anchor| {
        let expected = pair_count.saturating_sub(anchor.keep.saturating_add(1) as usize);
        let mut best: Option<usize> = None;
        let mut best_dist = usize::MAX;
        for (i, p) in pairs.iter().enumerate() {
            if pair_anchor_hash(p) == anchor.hash {
                let dist = i.abs_diff(expected);
                if dist < best_dist || (dist == best_dist && best.is_some_and(|b| i > b)) {
                    best = Some(i);
                    best_dist = dist;
                }
            }
        }
        best
    });
    let cold = anchor_match.is_none();
    let mut head = anchor_match.unwrap_or_else(|| pair_count.saturating_sub(max_pairs));
    tracing::debug!(
        pair_count,
        head,
        capacity = max_pairs,
        cold,
        "region-① block anchored"
    );

    // Aging adds one pair per turn, so this fires on the turn right after
    // the block reached capacity. The landing keeps `window − note_window`
    // pairs — the visible history returns to `window` turns; the dropped
    // pairs number `round(window × ext_rate)`, so at `ext_rate = 0`
    // nothing drops except the pair that aged in, which is the classic
    // one-pair-per-turn slide.
    if pair_count.saturating_sub(head) > max_pairs {
        head = pair_count - landing;
    }

    // Older pairs compact to text-only user/assistant. No history
    // notes here — the recent pairs are already verbatim, so notes
    // would be redundant (and are the leak trigger).
    let keep = pair_count - head;
    let compacted = crate::session::extract_user_assistant_pairs(compact_prior, keep, Some(0));
    let new_anchor = if keep > 0 {
        Some(crate::session::BlockAnchor {
            hash: pair_anchor_hash(&pairs[head]),
            keep: keep as u64,
        })
    } else {
        None
    };

    // Built region by region so `max_history_tokens` can weigh and
    // trim them as blocks before anything reaches the payload.
    // Region ① leads with the history marker and keeps it: it
    // explains the text-only block the compacted pairs form.
    let mut region1: Vec<serde_json::Value> = Vec::new();
    if !compacted.is_empty() {
        region1.push(provider.format_user_message(&[ContentBlock::Text {
            text: crate::session::HISTORY_MARKER_TEXT.into(),
        }]));
    }
    for msg in &compacted {
        // Defensive: skip messages with no extractable text so the
        // wire payload never contains empty text blocks (which
        // Anthropic rejects, especially under cache_control).
        if let Some(formatted) = format_cleaned_message(provider, msg) {
            region1.push(formatted);
        }
    }

    // Region ② — recent turns verbatim, minus round-tripped
    // history notes (metadata, not conversation; re-sending them as
    // user text is the pattern that triggered the leak). Apply the
    // tool-result byte cap to each message (non-tool messages are
    // pass-through, so the cap is cheap).
    let mut region2: Vec<serde_json::Value> = Vec::new();
    for msg in verbatim_prior {
        if crate::session::is_history_note(msg) {
            continue;
        }
        region2.push(cap_tool_result_content(msg, cap));
    }

    fit_history_to_budget(&mut region1, &mut region2, strategy.max_history_tokens);

    (region1, region2, new_anchor)
}

/// FNV-1a 64-bit hash. Deliberately hand-rolled and stable across builds
/// and platforms: the block anchor must compare equal between the process
/// that wrote it and any later process reading it, so `DefaultHasher`
/// (deliberately unspecified across Rust versions) is off the table.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Identity of one compact pair for the block anchor: independent FNV-1a
/// hashes of the user-side and assistant-side text. Hashes *text* (not
/// the raw message value) so the anchor survives provider-shape
/// differences between the session that wrote it and the one reading it.
fn pair_anchor_hash(pair: &crate::session::TurnPair) -> [u64; 2] {
    let user = crate::session::extract_message_text(&pair.user);
    let assistant = pair
        .assistant
        .as_ref()
        .map(crate::session::extract_message_text)
        .unwrap_or_default();
    [fnv1a_64(user.as_bytes()), fnv1a_64(assistant.as_bytes())]
}

/// Per-message constant added to every wire message's estimate — the role,
/// block types and separators providers wrap a message in.
const PER_MESSAGE_TOKENS: usize = 4;

/// Enforce `max_history_tokens` across the two history regions.
///
/// Oldest content goes first: region ① sheds its oldest messages (the
/// leading history marker survives — it is what explains the block), then
/// region ② sheds its oldest **whole** turns. Whole turns are the unit
/// because a partial cut would leave a tool result whose `tool_use` is gone,
/// which providers reject.
///
/// `None` and `Some(0)` mean off: the caller then sends exactly what an
/// unconfigured build sends. A budget below the marker alone still sends the
/// marker — it is the floor this function trims to.
fn fit_history_to_budget(
    region1: &mut Vec<serde_json::Value>,
    region2: &mut Vec<serde_json::Value>,
    max_history_tokens: Option<usize>,
) {
    let Some(budget) = max_history_tokens.filter(|&b| b > 0) else {
        return;
    };

    // Sizes are estimated once and carried alongside, so trimming subtracts
    // what it drops instead of re-estimating what stays on every step.
    let mut sizes1: Vec<usize> = region1.iter().map(est_message_tokens).collect();
    let mut sizes2: Vec<usize> = region2.iter().map(est_message_tokens).collect();
    let mut total: usize = sizes1.iter().sum::<usize>() + sizes2.iter().sum::<usize>();

    // Region ① — oldest first, the marker at index 0 protected.
    while total > budget && region1.len() > 1 {
        region1.remove(1);
        total -= sizes1.remove(1);
    }
    // Region ② — oldest whole turn first.
    while total > budget {
        let Some(turn) = oldest_turn_len(region2) else {
            break;
        };
        for _ in 0..turn {
            total -= sizes2.remove(0);
        }
        region2.drain(..turn);
    }
}

/// Number of leading region ② messages forming its oldest droppable unit:
/// one turn, or — when the region opens with tool results whose `tool_use`
/// fell outside the verbatim window — those orphans on their own. `None`
/// when the region is empty. Turns are delimited by the shared
/// [`crate::session::opens_turn`], the same boundary that split ① from ②.
fn oldest_turn_len(msgs: &[serde_json::Value]) -> Option<usize> {
    if msgs.is_empty() {
        return None;
    }
    let Some(opener) = msgs.iter().position(crate::session::opens_turn) else {
        return Some(msgs.len());
    };
    if opener > 0 {
        return Some(opener);
    }
    let end = msgs[1..]
        .iter()
        .position(crate::session::opens_turn)
        .map_or(msgs.len(), |p| p + 1);
    Some(end)
}

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
fn est_message_tokens(msg: &serde_json::Value) -> usize {
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

/// Truncate oversized tool-result `content` to `cap` bytes, head-only.
/// Region ②'s cap; see [`cap_tool_result_content_with`].
fn cap_tool_result_content(msg: &serde_json::Value, cap: usize) -> serde_json::Value {
    cap_tool_result_content_with(msg, cap, crate::session::truncate_text)
}

/// Truncate oversized tool-result `content` to `cap` bytes with the given
/// `truncate` strategy (head-only for region ②, head+tail for region ③).
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
/// (`with_regions_labels_*` tests). Production debug dumps go through
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
    let mut s = format!(
        "=== context sent to LLM (strategy={:?} window={} note_window={:?} turn_cap={:?} max_hist={:?}) ===\n\
         {} msgs, ~{} bytes\n\n",
        strategy.mode,
        strategy.window,
        strategy.note_window,
        strategy.turn_tool_result_cap,
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
        let rendered = crate::session::render_raw_context_as_text(&ctx);
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
            note_window: None,
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg).0;
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

        // window=2, note_window=1, ext_rate=0.5: the last prior turn (u3,
        // a3) is verbatim; the compact block holds (2−1)+round(0.5×2) = 2
        // pairs, so both older turns (u2 with its tool call included)
        // compact to text-only. No history note anywhere — notes taught
        // the model to write tool calls as text (the thinking-leak
        // trigger).
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 2,
            note_window: Some(1),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: Some(0.5),
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg).0;
        let sent = sent.into_owned();

        // 1. Compacted history marker, then both older pairs in pure text
        //    (no note, tool_calls dropped).
        assert_eq!(
            sent[0],
            json!({"role": "user", "content": crate::session::HISTORY_MARKER_TEXT})
        );
        assert_eq!(sent[1], json!({"role": "user", "content": "u1"}));
        assert_eq!(sent[2], json!({"role": "assistant", "content": "a1"}));
        assert_eq!(sent[3], json!({"role": "user", "content": "u2"}));
        assert_eq!(sent[4], json!({"role": "assistant", "content": "a2"}));

        // 2. Recent pair verbatim.
        assert_eq!(sent[5], json!({"role": "user", "content": "u3"}));
        assert_eq!(sent[6], json!({"role": "assistant", "content": "a3"}));

        // 3. Current turn verbatim (tool_calls preserved).
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
            note_window: None,
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, 99, &cfg)
            .0
            .into_owned();
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
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
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

        // note_window=1: only the last prior turn (u2, a2) is verbatim; the
        // older u1 turn (with its tool_use + tool_result) compacts to text.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 4,
            note_window: Some(1),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
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
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg)
            .0
            .into_owned();

        // Compacted history marker, then older turn text-only.
        assert_eq!(
            sent[0],
            json!({"role": "user", "content": crate::session::HISTORY_MARKER_TEXT})
        );
        assert_eq!(sent[1], json!({"role": "user", "content": "u1"}));
        assert_eq!(sent[2], json!({"role": "assistant", "content": "a1"}));
        // Recent turn VERBATIM: user, structured tool-call assistant, result.
        assert_eq!(sent[3], json!({"role": "user", "content": "u2"}));
        assert_eq!(sent[4], prior[3]);
        assert_eq!(sent[5], prior[4]);
        assert_eq!(sent[6], json!({"role": "assistant", "content": "done"}));
        // Current turn verbatim.
        assert_eq!(sent[7], current[0]);
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
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let sent = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg)
            .0
            .into_owned();

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

    // ── Region-① block anchor ─────────────────────────────────────────

    /// N plain user/assistant turns, contents unique per index.
    fn turns(n: usize) -> Vec<serde_json::Value> {
        let mut v = Vec::new();
        for i in 1..=n {
            v.push(json!({"role": "user", "content": format!("u{i}")}));
            v.push(json!({"role": "assistant", "content": format!("a{i}")}));
        }
        v
    }

    /// Region-① texts (history marker included) from a payload + labels.
    fn region1_texts(sent: &[serde_json::Value], regions: &[u8]) -> Vec<String> {
        sent.iter()
            .zip(regions)
            .filter(|(_, r)| **r == 1)
            .map(|(m, _)| text_of(m))
            .collect()
    }

    /// window=w, note_window=1 (so a k-turn prior compacts k−1 pairs), no
    /// caps, explicit `ext_rate`.
    fn block_cfg(window: usize, ext_rate: f64) -> ContextStrategyConfig {
        ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window,
            note_window: Some(1),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: Some(ext_rate),
        }
    }

    /// The block head stays put while the block holds `capacity` pairs:
    /// every turn's region ① is a strict extension of the previous turn's.
    /// The turn after the block reached capacity is the jump — the one
    /// turn where the prefix breaks — and the landing keeps `window −
    /// note_window` pairs, returning the visible history to `window` turns.
    #[test]
    fn block_head_stable_within_block_and_jumps_at_capacity() {
        // window=2, ext_rate=1 → capacity (2−1)+2 = 3, landing 2−1 = 1.
        let cfg = block_cfg(2, 1.0);
        let mut anchor = None;
        let mut prev_region1: Option<Vec<String>> = None;
        let mut jump_at = None;
        for k in 2..=6 {
            let prior = turns(k);
            let (sent, regions, next_anchor) =
                build_send_context_anchor_with_regions(&prov(), &prior, prior.len(), &cfg, anchor);
            let r1 = region1_texts(&sent, &regions);
            assert_eq!(r1[0], crate::session::HISTORY_MARKER_TEXT);
            if let Some(prev) = &prev_region1 {
                let extended = r1.len() > prev.len() && r1[..prev.len()] == prev[..];
                if !extended {
                    jump_at = Some(k);
                }
            }
            prev_region1 = Some(r1);
            anchor = next_anchor;
        }
        // Compact pairs 1..=3 (turns 2..=4) grow the block without a jump;
        // the 4th compact pair (turn 5) triggers it, landing on 1 pair
        // whose head is pair 3; turn 6 extends that block again.
        assert_eq!(jump_at, Some(5));
        let r1 = prev_region1.unwrap();
        assert_eq!(r1.len(), 1 + 2 * 2);
        assert_eq!(r1[1], "u4");
        assert_eq!(r1[r1.len() - 1], "a5");
    }

    /// Feeding the persisted anchor back in reproduces the block exactly,
    /// and while the compact region fits inside the capacity a cold build
    /// (no anchor) agrees with the anchored one.
    #[test]
    fn block_anchor_roundtrip_is_deterministic() {
        let cfg = block_cfg(2, 1.0); // capacity (2−1)+2 = 3
        let prior = turns(4); // 3 compact pairs, exactly at capacity
        let (s1, r1, anchor) =
            build_send_context_anchor_with_regions(&prov(), &prior, prior.len(), &cfg, None);
        assert!(anchor.is_some());
        let (s2, r2, anchor2) =
            build_send_context_anchor_with_regions(&prov(), &prior, prior.len(), &cfg, anchor);
        assert_eq!(s1.as_ref(), s2.as_ref());
        assert_eq!(r1, r2);
        assert_eq!(anchor, anchor2);
        let r1_texts = region1_texts(&s1, &r1);
        assert_eq!(
            r1_texts.len(),
            1 + 2 * 3,
            "cold block keeps every compact pair"
        );
    }

    /// `ext_rate = 0` degenerates to the pre-block behavior: the capacity
    /// equals the landing (`window − note_window` pairs), so the block is
    /// constant-size and its head slides one pair forward every turn,
    /// re-anchoring each time.
    #[test]
    fn block_ext_rate_zero_slides_one_pair_every_turn() {
        let cfg = block_cfg(2, 0.0); // capacity = landing = 2−1 = 1 pair
        let mut anchor = None;
        for k in 3..=5 {
            let prior = turns(k);
            let (sent, regions, next_anchor) =
                build_send_context_anchor_with_regions(&prov(), &prior, prior.len(), &cfg, anchor);
            let r1 = region1_texts(&sent, &regions);
            assert_eq!(r1.len(), 1 + 2 * 1, "constant-size block at turn {k}");
            assert_eq!(r1[1], format!("u{}", k - 1));
            assert_eq!(r1[r1.len() - 1], format!("a{}", k - 1));
            assert_ne!(anchor, next_anchor, "ext_rate=0 must re-anchor every turn");
            anchor = next_anchor;
        }
    }

    /// `ext_rate = 1` gives the widest admissible block: capacity
    /// `(window − note_window) + window`, landing `window − note_window`.
    /// The span oscillates between them — here 1..=3 pairs, a jump every
    /// `round(window × ext_rate) + 1 = 3` turns.
    #[test]
    fn block_ext_rate_one_oscillates_between_landing_and_capacity() {
        let cfg = block_cfg(2, 1.0); // capacity 3, landing 1
        let mut anchor = None;
        let mut lens = Vec::new();
        for k in 2..=7 {
            let prior = turns(k);
            let (sent, regions, next_anchor) =
                build_send_context_anchor_with_regions(&prov(), &prior, prior.len(), &cfg, anchor);
            lens.push(region1_texts(&sent, &regions).len());
            anchor = next_anchor;
        }
        assert_eq!(
            lens,
            vec![3, 5, 7, 3, 5, 7],
            "span grows 1→3 pairs, jumps back to 1, repeats every 3 turns"
        );
    }

    /// Spec check for the `window + ext_rate × window` span, the w=4 /
    /// note=1 / ext_rate=0.5 shape: turns t1..t6 accumulate (5 compact
    /// pairs), t7 overgrows the capacity and the visible history jumps
    /// back to [t4, t5, t6, t7] — `window` turns.
    #[test]
    fn block_ext_rate_half_jumps_back_to_window_turns() {
        let cfg = block_cfg(4, 0.5); // capacity (4−1)+2 = 5, landing 4−1 = 3
        let prior = turns(6); // 5 compact pairs: at capacity, no jump yet
        let (sent, regions, anchor) =
            build_send_context_anchor_with_regions(&prov(), &prior, prior.len(), &cfg, None);
        let r1 = region1_texts(&sent, &regions);
        assert_eq!(r1.len(), 1 + 2 * 5, "t1..t5 compacted, t6 verbatim");
        assert_eq!(r1[1], "u1");

        let prior = turns(7); // 6 compact pairs > capacity 5
        let (sent, regions, _) =
            build_send_context_anchor_with_regions(&prov(), &prior, prior.len(), &cfg, anchor);
        let r1 = region1_texts(&sent, &regions);
        assert_eq!(r1.len(), 1 + 2 * 3, "jump keeps the last 3 compact pairs");
        assert_eq!(r1[1], "u4", "visible history is [t4,t5,t6] + verbatim t7");
        assert_eq!(r1[r1.len() - 1], "a6");
    }

    /// An anchor whose pair left the delivered slice (journal rewritten,
    /// strategy changed) rebuilds the block cold from the tail capacity
    /// pairs, and the rebuilt anchor continues the block on the next turn.
    #[test]
    fn block_cold_rebuilds_when_anchor_pair_is_gone() {
        let cfg = block_cfg(2, 1.0); // capacity (2−1)+2 = 3, landing 1
        let prior = turns(7); // 6 compact pairs
        let (sent, regions, anchor) = build_send_context_anchor_with_regions(
            &prov(),
            &prior,
            prior.len(),
            &cfg,
            Some(crate::session::BlockAnchor {
                hash: [0, 0],
                keep: 0,
            }), // garbage: matches no pair
        );
        let r1 = region1_texts(&sent, &regions);
        assert_eq!(r1.len(), 1 + 2 * 3, "cold block holds the capacity");
        assert_eq!(r1[1], "u4");

        // Next turn: the rebuilt anchor (pair 3) is found, and the block
        // jumps from 4 pairs to the 1-pair landing.
        let prior2 = turns(8); // 7 compact pairs; pair 3 is still pair 3
        let (sent2, regions2, _) =
            build_send_context_anchor_with_regions(&prov(), &prior2, prior2.len(), &cfg, anchor);
        let r2 = region1_texts(&sent2, &regions2);
        assert_eq!(r2.len(), 1 + 2 * 1);
        assert_eq!(r2[1], "u7");
    }

    /// Two pairs with identical text hash to the same anchor. The
    /// persisted `keep` names which occurrence is the head: resolved by
    /// position alone a duplicate near the tail would be picked, the
    /// block would read as under capacity and never jump — the silent
    /// degradation to an ever-growing block.
    #[test]
    fn block_anchor_resolves_duplicate_text_to_the_expected_pair() {
        // window=2, ext_rate=1 → capacity 3, landing 1. Turns 1 and 3 are
        // text-identical ("ping"/"pong").
        let cfg = block_cfg(2, 1.0);
        let script = [
            ("ping", "pong"),
            ("u2", "a2"),
            ("ping", "pong"),
            ("u4", "a4"),
            ("u5", "a5"),
            ("u6", "a6"),
            ("u7", "a7"),
        ];
        let mut journal: Vec<serde_json::Value> = Vec::new();
        let mut anchor = None;
        let mut r1_lens = Vec::new();
        let mut anchors = Vec::new();
        for (u, a) in script {
            journal.push(json!({"role": "user", "content": u}));
            journal.push(json!({"role": "assistant", "content": a}));
            let (sent, regions, next_anchor) = build_send_context_anchor_with_regions(
                &prov(),
                &journal,
                journal.len(),
                &cfg,
                anchor,
            );
            r1_lens.push(region1_texts(&sent, &regions).len());
            anchors.push(next_anchor);
            anchor = next_anchor;
        }
        // Turn-by-turn (k = turns so far; the last turn is verbatim, so
        // the compact region holds k−1 pairs):
        // - k=4: 3 pairs, at capacity — no jump. The anchor from k=3 says
        //   "ping, keep 2", so the expected head is pair 0: the *first*
        //   "ping" wins by distance, not the tail-most duplicate.
        // - k=5: 4 pairs over the capacity — the head jumps to the landing
        //   (1 pair, pair 3 = "u4").
        // - k=6: anchor "u4, keep 1" → expected head = 5 − 1 − 1 = 3, the
        //   block extends to 2 pairs without a jump.
        // - k=7: anchor "u4, keep 2" → expected head = 6 − 2 − 1 = 3, the
        //   block extends to 3 pairs without a jump.
        assert_eq!(r1_lens[4], 1 + 2 * 1, "jumped block at k=5: {r1_lens:?}");
        assert_eq!(r1_lens[5], 1 + 2 * 2, "extended block at k=6: {r1_lens:?}");
        assert_eq!(
            r1_lens[6],
            1 + 2 * 3,
            "duplicate-text anchor must extend the block, not re-jump: {r1_lens:?}"
        );
        // The k=7 build resolved the anchor to pair 3 ("u4") and did not
        // jump, so the head pair is unchanged (`keep` recomputes to the
        // fresh block size every build — the persisted guard compares the
        // hash only).
        assert_eq!(
            anchors[5].map(|a| a.hash),
            anchors[6].map(|a| a.hash),
            "no-jump turn must keep the same head pair: {anchors:?}"
        );
    }

    /// The reader depth covers the block capacity *plus* the verbatim
    /// region's pairs and a one-pair slack: the block is anchored
    /// independently of the verbatim turns, and a reader-slice build must
    /// see the same compact pairs as a whole-journal build.
    #[test]
    fn prior_depth_covers_block_capacity_plus_verbatim() {
        // block_cfg has note_window=1: capacity = (w−1) + round(ext_rate×w),
        // reader pairs = capacity + note_window + 1 slack.
        assert_eq!(
            prior_depth(&block_cfg(4, 1.0)), // capacity 3+4 = 7
            PriorDepth::Window {
                pairs: 9,
                openers: 1
            }
        );
        assert_eq!(
            prior_depth(&block_cfg(4, 0.5)), // capacity 3+2 = 5
            PriorDepth::Window {
                pairs: 7,
                openers: 1
            }
        );
        assert_eq!(
            prior_depth(&block_cfg(4, 0.0)), // capacity 3+0 = 3: the classic window
            PriorDepth::Window {
                pairs: 5,
                openers: 1
            }
        );
        let no_note = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            note_window: None,
            ..Default::default()
        };
        assert_eq!(prior_depth(&no_note), PriorDepth::All);
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
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
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
        // ① compacted = marker + (u1, a1), ② verbatim = (u2, a2), ③ current = (u3).
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
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, prior.len(), &cfg);
        assert_eq!(sent.len(), 6);
        assert_eq!(regions, vec![1, 1, 1, 2, 2, 3]);
        assert_eq!(
            sent[0]["content"].as_str().unwrap(),
            crate::session::HISTORY_MARKER_TEXT
        );

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

    /// The cached history regions must be indistinguishable from a cold
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
            note_window: Some(1),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.push(json!({"role": "user", "content": "u3"}));

        let mut cache = SendContextCache::default();
        let (first, first_regions) =
            build_send_context_cached(&prov(), &ctx, prior_len, &cfg, None, &mut cache);
        let (cold, cold_regions) = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg);
        assert_eq!(first.as_ref(), cold.as_ref());
        assert_eq!(first_regions, cold_regions);

        // Another iteration of the same turn: the current turn grew, the
        // cached prefix did not — the history regions are reused as-is.
        ctx.push(json!({"role": "assistant", "content": "a3"}));
        let (second, second_regions) =
            build_send_context_cached(&prov(), &ctx, prior_len, &cfg, None, &mut cache);
        let (cold2, cold2_regions) =
            build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg);
        assert_eq!(second.as_ref(), cold2.as_ref());
        assert_eq!(second_regions, cold2_regions);

        // Next turn: the prefix grew, so the cached regions must be rebuilt.
        let next_prior_len = ctx.len();
        ctx.push(json!({"role": "user", "content": "u4"}));
        let (third, third_regions) =
            build_send_context_cached(&prov(), &ctx, next_prior_len, &cfg, None, &mut cache);
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
        // `window: 1` leaves one pair in the compacted region ①, so the
        // sentinel proves reuse of a populated region, not of an empty one.
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 1,
            note_window: Some(1),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.push(json!({"role": "user", "content": "u3"}));

        let mut cache = SendContextCache::default();
        let _ = build_send_context_cached(&prov(), &ctx, prior_len, &cfg, None, &mut cache);
        assert_eq!(
            cache.history_rebuilt(),
            Some(true),
            "a cold cache must build"
        );

        // Spliced into the cache, not into `raw_context`: a rebuild derives
        // the regions from `raw_context` and would drop it.
        cache
            .region1
            .push(json!({"role": "user", "content": "CACHED-SENTINEL"}));
        let (warm, _) = build_send_context_cached(&prov(), &ctx, prior_len, &cfg, None, &mut cache);
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
            build_send_context_cached(&prov(), &ctx, next_prior_len, &cfg, None, &mut cache);
        assert!(
            !rebuilt
                .iter()
                .any(|m| m.get("content").and_then(|c| c.as_str()) == Some("CACHED-SENTINEL")),
            "a new prefix must rebuild the regions"
        );
        assert_eq!(cache.history_rebuilt(), Some(true));
    }

    #[test]
    fn history_marker_omitted_when_compacted_region_is_empty() {
        // window == verbatim_pairs, so the compact region has zero pairs.
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
            note_window: Some(1),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, prior.len(), &cfg);
        assert_eq!(sent.len(), 3);
        assert_eq!(regions, vec![2, 2, 3]);
        let texts: Vec<&str> = sent.iter().filter_map(|m| m["content"].as_str()).collect();
        assert!(
            !texts
                .iter()
                .any(|t| t.starts_with(crate::session::HISTORY_MARKER_TEXT)),
            "marker must not appear when compacted region is empty, got: {texts:?}"
        );
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
            note_window: Some(1),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
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
            note_window: None,
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
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

    /// `cap_tool_result_content` — unit tests for the byte cap helper.
    /// Integration via `build_send_context_with_regions` is covered by
    /// `build_send_context_applies_cap_to_verbatim_region` below.
    #[test]
    fn cap_tool_result_truncates_openai_shape() {
        let big = "x".repeat(2000);
        let msg = json!({"role": "tool", "tool_call_id": "1", "content": big.clone()});
        let capped = cap_tool_result_content(&msg, 100);
        let content = capped["content"].as_str().unwrap();
        assert!(content.len() < 200, "should be much smaller than 2000");
        assert!(content.starts_with('x'), "preserves leading content");
        assert!(content.contains("[truncated"), "appends truncation marker");
        assert!(content.contains("bytes]"), "marker mentions bytes");
        // The marker must report the exact number of dropped bytes —
        // 1900 for a 100-byte cap on a 2000-byte input.
        assert!(
            content.contains("[truncated 1900 bytes]"),
            "marker should report exact dropped bytes, got: {content}"
        );
    }

    #[test]
    fn cap_tool_result_passthrough_when_under_cap() {
        let msg = json!({"role": "tool", "tool_call_id": "1", "content": "short"});
        let capped = cap_tool_result_content(&msg, 100);
        // Identity check (no clone-modify churn) — should be the same content.
        assert_eq!(capped["content"].as_str().unwrap(), "short");
        assert!(!capped["content"].as_str().unwrap().contains("truncated"));
    }

    #[test]
    fn cap_tool_result_passthrough_for_user_or_assistant() {
        // Only `role=tool` (OpenAI) and `role=user` with tool_result
        // blocks (Anthropic) get capped — every other role is a no-op.
        let msg = json!({"role": "assistant", "content": "x".repeat(2000)});
        let capped = cap_tool_result_content(&msg, 100);
        assert_eq!(capped, msg, "assistant should pass through unchanged");

        let msg = json!({"role": "user", "content": "x".repeat(2000)});
        let capped = cap_tool_result_content(&msg, 100);
        assert_eq!(
            capped, msg,
            "user with string content (no tool_result blocks) should pass through"
        );
    }

    #[test]
    fn cap_tool_result_zero_is_passthrough() {
        // Some(0) is the explicit-off sentinel.
        let msg = json!({"role": "tool", "tool_call_id": "1", "content": "x".repeat(2000)});
        let capped = cap_tool_result_content(&msg, 0);
        assert_eq!(capped, msg);
    }

    #[test]
    fn cap_tool_result_truncates_anthropic_shape() {
        let big = "x".repeat(2000);
        let msg = json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "framing text"},
                {"type": "tool_result", "tool_use_id": "t1", "content": big},
                // A second tool_result in the same user message — both
                // should be capped.
                {"type": "tool_result", "tool_use_id": "t2", "content": "short"},
                // A non-tool_result block — should pass through.
                {"type": "image", "source": {"type": "base64", "data": "..."}},
            ]
        });
        let capped = cap_tool_result_content(&msg, 100);
        let blocks = capped["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 4, "block count preserved");
        assert_eq!(blocks[0]["text"], "framing text");
        assert!(
            blocks[1]["content"]
                .as_str()
                .unwrap()
                .contains("[truncated")
        );
        assert_eq!(blocks[2]["content"].as_str().unwrap(), "short");
        assert_eq!(
            blocks[3],
            json!({"type": "image", "source": {"type": "base64", "data": "..."}})
        );
    }

    /// Integration: a real-shaped sliding window with a 50-byte cap must
    /// reduce the serialized payload size vs. uncapped, and every capped
    /// tool result must carry the standard marker.
    #[test]
    fn build_send_context_applies_cap_to_verbatim_region() {
        let big = "y".repeat(2000);
        // Two prior turns, each with a tool call + big result. Both tool
        // results live in region ② (verbatim prior) and must get capped.
        // Region ③ (the current turn) is no longer capped — covered by
        // `build_send_context_does_not_cap_current_turn_even_when_cap_set`.
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "calling bash", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": big.clone()}),
            json!({"role": "assistant", "content": "done1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "calling bash", "tool_calls": [{"id":"2","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "2", "content": big.clone()}),
            json!({"role": "assistant", "content": "done2"}),
        ];
        let current = vec![json!({"role": "user", "content": "u3"})];
        let mut ctx = prior.clone();
        ctx.extend(current.clone());
        let prior_len = prior.len();

        let uncapped_cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            note_window: Some(5),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let (uncapped, _) =
            build_send_context_with_regions(&prov(), &ctx, prior_len, &uncapped_cfg);
        let uncapped_bytes = serde_json::to_string(uncapped.as_ref()).unwrap().len();

        let capped_cfg = ContextStrategyConfig {
            tool_result_cap: Some(50),
            ..uncapped_cfg.clone()
        };
        let (capped, _) = build_send_context_with_regions(&prov(), &ctx, prior_len, &capped_cfg);
        let capped_bytes = serde_json::to_string(capped.as_ref()).unwrap().len();

        assert!(
            capped_bytes < uncapped_bytes,
            "capping should reduce payload size: uncapped={uncapped_bytes} capped={capped_bytes}"
        );
        // The cap should remove most of the 2000-byte bodies (two of them).
        assert!(
            uncapped_bytes - capped_bytes > 3000,
            "cap should drop at least 3KB across two prior tool results: uncapped={uncapped_bytes} capped={capped_bytes}"
        );

        // The standard truncation marker must appear in the capped payload.
        let capped_str = serde_json::to_string(capped.as_ref()).unwrap();
        assert!(
            capped_str.contains("[truncated ") && capped_str.contains("bytes]"),
            "missing truncation marker: {capped_str}"
        );
    }

    /// Region ③ (current turn) is never truncated, regardless of
    /// `tool_result_cap`. The full tool result must appear in the wire
    /// payload with no truncation marker.
    #[test]
    fn build_send_context_does_not_cap_current_turn_even_when_cap_set() {
        let big = "y".repeat(2000);
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
        ];
        // Current turn contains a tool result mid-loop. The model is
        // reasoning over this content; truncating it would corrupt
        // mid-loop state.
        let current = vec![
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": big.clone()}),
        ];
        let mut ctx = prior.clone();
        ctx.extend(current.clone());
        let prior_len = prior.len();

        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            note_window: Some(5),
            tool_result_cap: Some(50), // cap is ON
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let (out, regions) = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg);
        let wire = serde_json::to_string(out.as_ref()).unwrap();

        // Full 2000-byte content must appear in region 3.
        assert!(
            wire.contains(&big),
            "current-turn tool result was truncated: {wire}"
        );
        assert!(
            !wire.contains("[truncated"),
            "current-turn tool result has truncation marker: {wire}"
        );

        // The tool message is labelled region 3 (not 2).
        let tool_idx = out
            .iter()
            .position(|m| m["role"] == "tool")
            .expect("tool result must be in output");
        assert_eq!(
            regions[tool_idx], 3,
            "tool result should be in region 3 (current turn)"
        );
    }

    /// With `turn_tool_result_cap` set, region ③ is bounded too — head and
    /// tail kept, middle dropped with a paging marker — while region ② stays
    /// governed by `tool_result_cap` alone.
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
            note_window: Some(5),
            // region ② cap is OFF, so anything trimmed below must be the
            // turn cap, not this one.
            tool_result_cap: None,
            turn_tool_result_cap: Some(600),
            max_history_tokens: None,
            ext_rate: None,
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
        assert_eq!(regions[tool_idx], 3, "capped result stays region ③");
        // The capped result must respect the byte bound it was given.
        let content = out[tool_idx]["content"].as_str().unwrap();
        assert!(
            content.len() <= 600,
            "region ③ content must honour the cap: {}",
            content.len()
        );
    }

    /// The turn cap must not leak into region ② — a prior-turn result stays
    /// whole while the turn cap is on and `tool_result_cap` is off.
    #[test]
    fn build_send_context_turn_cap_does_not_touch_prior_turn() {
        let big = "w".repeat(3000);
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2", "tool_calls": [{"id":"9","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "9", "content": big.clone()}),
            json!({"role": "assistant", "content": "a3"}),
        ];
        let current = vec![json!({"role": "user", "content": "u3"})];
        let mut ctx = prior.clone();
        ctx.extend(current);
        let prior_len = prior.len();

        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            note_window: Some(10),
            tool_result_cap: None,
            turn_tool_result_cap: Some(600),
            max_history_tokens: None,
            ext_rate: None,
        };
        let (out, _) = build_send_context_with_regions(&prov(), &ctx, prior_len, &cfg);
        let wire = serde_json::to_string(out.as_ref()).unwrap();
        assert!(
            wire.contains(&big),
            "region ② must stay untouched by the turn cap: {}",
            &wire[..wire.len().min(200)]
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
            note_window: Some(5),
            tool_result_cap: Some(2048),
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let zero_cfg = ContextStrategyConfig {
            turn_tool_result_cap: Some(0),
            max_history_tokens: None,
            ext_rate: None,
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

    #[test]
    fn build_send_context_cap_zero_is_passthrough() {
        // Some(0) is the explicit-off sentinel — must match None behavior.
        let big = "y".repeat(2000);
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "calling", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": big}),
        ];
        let prior_len = prior.len();
        let cfg_none = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 5,
            note_window: Some(5),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let cfg_zero = ContextStrategyConfig {
            tool_result_cap: Some(0),
            ..cfg_none.clone()
        };
        let (a, _) = build_send_context_with_regions(&prov(), &prior, prior_len, &cfg_none);
        let (b, _) = build_send_context_with_regions(&prov(), &prior, prior_len, &cfg_zero);
        assert_eq!(
            serde_json::to_string(a.as_ref()).unwrap(),
            serde_json::to_string(b.as_ref()).unwrap(),
            "Some(0) must behave identically to None"
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

    fn text_of(msg: &serde_json::Value) -> String {
        crate::session::extract_message_text(msg)
    }

    /// Prior context whose oldest turn is distinctive, shaped so that
    /// `note_window = 1` leaves the last turn verbatim and folds the two
    /// older pairs into region ①. Current turn is one message.
    fn three_turn_context() -> (Vec<serde_json::Value>, usize) {
        let prior = vec![
            json!({"role": "user", "content": "oldest-user"}),
            json!({"role": "assistant", "content": "oldest-reply"}),
            json!({"role": "user", "content": "middle-user"}),
            json!({"role": "assistant", "content": "middle-reply"}),
            json!({"role": "user", "content": "recent-user"}),
            json!({"role": "assistant", "content": "recent-reply"}),
        ];
        let current = vec![json!({"role": "user", "content": "current-turn"})];
        let prior_len = prior.len();
        let mut ctx = prior;
        ctx.extend(current);
        (ctx, prior_len)
    }

    fn budget_base() -> ContextStrategyConfig {
        ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            note_window: Some(1),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        }
    }

    /// `None` and `Some(0)` are the same "off" state — the payload must be
    /// byte-identical to what an unconfigured build sends.
    #[test]
    fn max_history_tokens_none_and_zero_send_identical_payload() {
        let (ctx, prior_len) = three_turn_context();
        let base = budget_base();
        let zero = ContextStrategyConfig {
            max_history_tokens: Some(0),
            ext_rate: None,
            ..base.clone()
        };
        let (a, ra) = build_send_context_with_regions(&prov(), &ctx, prior_len, &base);
        let (b, rb) = build_send_context_with_regions(&prov(), &ctx, prior_len, &zero);
        assert_eq!(
            serde_json::to_string(a.as_ref()).unwrap(),
            serde_json::to_string(b.as_ref()).unwrap(),
            "Some(0) must behave identically to None"
        );
        assert_eq!(ra, rb);
    }

    /// A budget nobody reaches is inert: the payload and the region labels
    /// are exactly what the unconfigured build produces.
    #[test]
    fn max_history_tokens_under_budget_leaves_payload_untouched() {
        let (ctx, prior_len) = three_turn_context();
        let base = budget_base();
        let roomy = ContextStrategyConfig {
            max_history_tokens: Some(1_000_000),
            ext_rate: None,
            ..base.clone()
        };
        let (plain, plain_regions) =
            build_send_context_with_regions(&prov(), &ctx, prior_len, &base);
        let (budgeted, budgeted_regions) =
            build_send_context_with_regions(&prov(), &ctx, prior_len, &roomy);
        assert_eq!(plain.as_ref(), budgeted.as_ref());
        assert_eq!(plain_regions, budgeted_regions);
    }

    /// Over budget, the oldest history goes first and nothing newer is
    /// touched. The budget here is exactly the weight of the payload minus
    /// the oldest pair, so the expected wire payload can be stated outright
    /// instead of counted.
    #[test]
    fn max_history_tokens_drops_oldest_compacted_pairs_first() {
        let (ctx, prior_len) = three_turn_context();
        let base = budget_base();
        let (uncapped, _) = build_send_context_with_regions(&prov(), &ctx, prior_len, &base);
        let uncapped = uncapped.into_owned();

        let expected: Vec<serde_json::Value> = uncapped
            .iter()
            .filter(|m| !text_of(m).contains("oldest"))
            .cloned()
            .collect();
        assert!(
            uncapped.len() - expected.len() == 2,
            "fixture must shed exactly the oldest pair, got {uncapped:?}"
        );
        let budget: usize = expected.iter().map(est_message_tokens).sum();

        let tight = ContextStrategyConfig {
            max_history_tokens: Some(budget),
            ext_rate: None,
            ..base.clone()
        };
        let (out, regions) = build_send_context_with_regions(&prov(), &ctx, prior_len, &tight);
        assert_eq!(out.into_owned(), expected);
        // Region labels follow their messages: ① marker + two pairs, ② the
        // verbatim turn, ③ the current turn.
        assert_eq!(regions, vec![1, 1, 1, 2, 2, 3]);
    }

    /// Region ② loses whole turns, never halves: a tool result whose
    /// `tool_use` was trimmed away is rejected by the API, so the cut has
    /// to land on turn boundaries.
    #[test]
    fn max_history_tokens_drops_oldest_whole_turns_from_verbatim_region() {
        let prior = vec![
            json!({"role": "user", "content": "turn-1-user"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "t1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "t1", "content": "A".repeat(4000)}),
            json!({"role": "assistant", "content": "turn-1-final"}),
            json!({"role": "user", "content": "turn-2-user"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "t2", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "t2", "content": "small"}),
            json!({"role": "assistant", "content": "turn-2-final"}),
        ];
        let current = vec![json!({"role": "user", "content": "current-turn"})];
        let prior_len = prior.len();
        let mut ctx = prior;
        ctx.extend(current);

        let base = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            note_window: Some(2),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let (uncapped, _) = build_send_context_with_regions(&prov(), &ctx, prior_len, &base);
        let expected: Vec<serde_json::Value> = uncapped
            .iter()
            .filter(|m| {
                serde_json::to_string(m).unwrap().find("turn-1").is_none()
                    && serde_json::to_string(m).unwrap().find("t1").is_none()
            })
            .cloned()
            .collect();
        let budget: usize = expected.iter().map(est_message_tokens).sum();

        let tight = ContextStrategyConfig {
            max_history_tokens: Some(budget),
            ext_rate: None,
            ..base.clone()
        };
        let (out, _) = build_send_context_with_regions(&prov(), &ctx, prior_len, &tight);
        let out = out.into_owned();
        assert_eq!(out, expected);

        let wire = serde_json::to_string(&out).unwrap();
        assert!(!wire.contains("turn-1"), "oldest turn survived: {wire}");
        assert!(wire.contains("turn-2"), "newer turn was cut: {wire}");
        // Every tool result still has the assistant call that produced it.
        for msg in &out {
            if msg["role"] == "tool" {
                let id = msg["tool_call_id"].as_str().unwrap();
                assert!(
                    out.iter().any(|m| {
                        m["tool_calls"]
                            .as_array()
                            .is_some_and(|calls| calls.iter().any(|c| c["id"].as_str() == Some(id)))
                    }),
                    "orphan tool result {id} left in payload: {wire}"
                );
            }
        }
    }

    /// A budget this small still leaves the current turn intact — it is the
    /// work in progress, and `turn_tool_result_cap` is its lever, not this
    /// one. The history marker is the floor region ① trims to.
    #[test]
    fn max_history_tokens_never_touches_the_current_turn() {
        let (ctx, prior_len) = three_turn_context();
        let tiny = ContextStrategyConfig {
            max_history_tokens: Some(1),
            ext_rate: None,
            ..budget_base()
        };
        let (out, regions) = build_send_context_with_regions(&prov(), &ctx, prior_len, &tiny);
        let out = out.into_owned();
        assert_eq!(text_of(&out[0]), crate::session::HISTORY_MARKER_TEXT);
        assert_eq!(text_of(&out[1]), "current-turn");
        assert_eq!(regions, vec![1, 3]);
    }

    /// The two rungs have to compose: region ① runs out (down to the
    /// marker) and the budget is *still* over, so region ② must then shed
    /// its oldest turn — while the newest verbatim turn survives. The
    /// expected payload is stated outright, so the assertion pins the whole
    /// shape rather than a count.
    #[test]
    fn max_history_tokens_falls_through_region_1_into_region_2() {
        let prior = vec![
            json!({"role": "user", "content": "compacted-user"}),
            json!({"role": "assistant", "content": "compacted-reply"}),
            json!({"role": "user", "content": "turn-2-user"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "t1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "t1", "content": "old result"}),
            json!({"role": "assistant", "content": "turn-2-final"}),
            json!({"role": "user", "content": "turn-3-user"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "t2", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "t2", "content": "recent result"}),
            json!({"role": "assistant", "content": "turn-3-final"}),
        ];
        let current = vec![json!({"role": "user", "content": "current-turn"})];
        let prior_len = prior.len();
        let mut ctx = prior;
        ctx.extend(current);

        let base = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 10,
            note_window: Some(2),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };
        let (uncapped, _) = build_send_context_with_regions(&prov(), &ctx, prior_len, &base);
        let expected: Vec<serde_json::Value> = uncapped
            .iter()
            .filter(|m| {
                let m = serde_json::to_string(m).unwrap();
                !m.contains("compacted") && !m.contains("t1") && !m.contains("turn-2")
            })
            .cloned()
            .collect();
        assert_eq!(
            expected.len(),
            6,
            "marker (1) + the newest verbatim turn (user, tool call, tool result, final = 4) + the current turn (1): {expected:?}"
        );
        let budget: usize = expected.iter().map(est_message_tokens).sum();

        let tight = ContextStrategyConfig {
            max_history_tokens: Some(budget),
            ext_rate: None,
            ..base
        };
        let (out, regions) = build_send_context_with_regions(&prov(), &ctx, prior_len, &tight);
        assert_eq!(out.into_owned(), expected);
        // marker ①, three messages of the newest turn ②, current turn ③
        assert_eq!(regions, vec![1, 2, 2, 2, 2, 3]);
    }

    /// A region ② that opens with tool results whose `tool_use` fell outside
    /// the verbatim window drops those orphans first — as a group, so the
    /// turns behind them still go whole.
    #[test]
    fn oldest_turn_len_drops_leading_orphans_as_their_own_group() {
        let msgs = vec![
            json!({"role": "tool", "tool_call_id": "x", "content": "orphan"}),
            json!({"role": "tool", "tool_call_id": "y", "content": "orphan"}),
            json!({"role": "user", "content": "u"}),
            json!({"role": "assistant", "content": "a"}),
        ];
        assert_eq!(oldest_turn_len(&msgs), Some(2));
        assert_eq!(oldest_turn_len(&[]), None);
    }

    /// The payload must not change when the prior comes from the
    /// reader's window instead of the whole journal: the reader exists to
    /// make a long history cheap to read, not to change what the model
    /// sees.
    #[tokio::test]
    async fn the_window_payload_is_the_same_from_a_reader_slice() {
        let mut prior: Vec<serde_json::Value> = Vec::new();
        for i in 1..=12 {
            prior.push(json!({"role": "user", "content": format!("u{i}")}));
            if i == 4 {
                // A tool turn in the middle of the history.
                prior.push(json!({"role": "assistant", "content": "a4", "tool_calls": [{"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}));
                prior.push(json!({"role": "tool", "tool_call_id": "1", "content": "out"}));
            } else {
                prior.push(json!({"role": "assistant", "content": format!("a{i}")}));
            }
        }
        let current = vec![
            json!({"role": "user", "content": "current"}),
            json!({"role": "assistant", "content": "reply"}),
        ];

        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 4,
            note_window: Some(2),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };

        // The journal the reader reads back.
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        let mut body = String::new();
        for v in &prior {
            body.push_str(&serde_json::to_string(v).unwrap());
            body.push('\n');
        }
        tokio::fs::write(
            state_dir.join(jyc_core::session_state::AGENT_CONTEXT_FILE),
            body,
        )
        .await
        .unwrap();
        let reader = crate::context_reader::ContextReader::new("reader-diff", &state_dir);
        let slice = reader.read_prior(prior_depth(&cfg)).await;

        // The whole journal and the reader's window must produce the same
        // payload.
        let mut whole = prior.clone();
        whole.extend(current.clone());
        let (whole_payload, whole_regions) =
            build_send_context_with_regions(&prov(), &whole, prior.len(), &cfg);

        let mut windowed = slice.messages.clone();
        windowed.extend(current.clone());
        let (windowed_payload, windowed_regions) =
            build_send_context_with_regions(&prov(), &windowed, slice.messages.len(), &cfg);

        assert_eq!(
            whole_payload.into_owned(),
            windowed_payload.into_owned(),
            "the window must not change the payload"
        );
        assert_eq!(whole_regions, windowed_regions);
    }

    /// The cold differential test above starts from no anchor; production
    /// threads the persisted anchor into every build. A slice one pair
    /// short of the anchor's pair would silently cold-rebuild into a
    /// different block, so the anchored path needs its own parity pin —
    /// including after a jump (the anchor below comes from a build one
    /// turn short, where the block had already jumped) and across
    /// `ext_rate` values.
    #[tokio::test]
    async fn the_anchored_window_payload_is_the_same_from_a_reader_slice() {
        let mut prior: Vec<serde_json::Value> = Vec::new();
        for i in 1..=12 {
            prior.push(json!({"role": "user", "content": format!("u{i}")}));
            if i == 4 {
                // A tool turn in the middle of the history.
                prior.push(json!({"role": "assistant", "content": "a4", "tool_calls": [{"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}));
                prior.push(json!({"role": "tool", "tool_call_id": "1", "content": "out"}));
            } else {
                prior.push(json!({"role": "assistant", "content": format!("a{i}")}));
            }
        }
        let current = vec![
            json!({"role": "user", "content": "current"}),
            json!({"role": "assistant", "content": "reply"}),
        ];

        // The journal the reader reads back.
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&state_dir).await.unwrap();
        let mut body = String::new();
        for v in &prior {
            body.push_str(&serde_json::to_string(v).unwrap());
            body.push('\n');
        }
        tokio::fs::write(
            state_dir.join(jyc_core::session_state::AGENT_CONTEXT_FILE),
            body,
        )
        .await
        .unwrap();
        let reader = crate::context_reader::ContextReader::new("reader-anchored-diff", &state_dir);

        let base = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 4,
            note_window: Some(2),
            tool_result_cap: None,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            ext_rate: None,
        };

        for cfg in [
            base.clone(),
            ContextStrategyConfig {
                ext_rate: Some(1.0),
                ..base.clone()
            },
        ] {
            // The anchor a whole-journal build at one turn short would
            // have persisted: 9 compact pairs against a capacity of 4
            // (default ext_rate 0.5) or 6 (ext_rate 1.0) — past capacity,
            // so the next build overgrows it and jumps. The full-journal
            // build below must land on the same pair as a reader-slice
            // build fed the same anchor.
            let short = &prior[..prior.len() - 2];
            let (_, _, anchor) =
                build_send_context_anchor_with_regions(&prov(), short, short.len(), &cfg, None);
            assert!(anchor.is_some());

            let slice = reader.read_prior(prior_depth(&cfg)).await;

            let mut whole = prior.clone();
            whole.extend(current.clone());
            let (whole_payload, whole_regions, _) =
                build_send_context_anchor_with_regions(&prov(), &whole, prior.len(), &cfg, anchor);

            let mut windowed = slice.messages.clone();
            windowed.extend(current.clone());
            let (windowed_payload, windowed_regions, _) = build_send_context_anchor_with_regions(
                &prov(),
                &windowed,
                slice.messages.len(),
                &cfg,
                anchor,
            );

            let whole_payload = whole_payload.into_owned();
            let windowed_payload = windowed_payload.into_owned();
            assert_eq!(
                whole_payload, windowed_payload,
                "the window must not change the anchored payload (ext_rate={:?})",
                cfg.ext_rate
            );
            assert_eq!(whole_regions, windowed_regions);

            // The default-ext_rate build must actually have taken the
            // anchored path: with 10 compact pairs and capacity 4 a cold
            // build would start at pair 6 ("u7"); the anchor names pair 5
            // ("u6"), the block overgrows it, and the jump lands on
            // pair 8 ("u9").
            if cfg.ext_rate.is_none() {
                let r1 = region1_texts(&whole_payload, &whole_regions);
                assert_eq!(
                    r1[1], "u9",
                    "anchor must resolve inside the slice, not cold-rebuild"
                );
            }
        }
    }
}
