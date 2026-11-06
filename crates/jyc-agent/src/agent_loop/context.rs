//! Raw-context rendering and history compaction helpers.
//!
//! Extracted from the monolithic `agent_loop.rs`.

use std::borrow::Cow;

use jyc_types::channel::{ContextStrategy, ContextStrategyConfig};
use jyc_types::{ContextRegion, ContextRegionInfo};

use crate::provider::Provider;
use crate::types::ContentBlock;

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
///   1. The prior turns replayed **verbatim**: the raw messages as they
///      were sent — text, structured `tool_calls` and their results — with
///      oversized fields cut to [`FOLDED_FIELD_REPLAY_CAP`] and image
///      blocks replaced by a text placeholder. The block is
///      provider-independent: the journal keeps the active provider's wire
///      shape — chat-shaped entries, or Anthropic's block shape (the
///      Responses provider deliberately persists chat shape) — and both are
///      capped field by field, so the same field is bounded whichever
///      provider wrote it. It only ever grows: nothing trims its head.
///   2. A checkpoint for the older part of a long current turn, in the
///      same replayed form. It appears once the turn has more than
///      [`ContextStrategyConfig::checkpoint_interval`] assistant messages
///      and then widens each fold by
///      [`ContextStrategyConfig::checkpoint_growth`], so it holds still
///      between folds and a long turn keeps a growing verbatim tail.
///      An interval of `0` disables it, leaving the whole turn verbatim.
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
/// - `1` = folded history, raw messages with oversized fields capped
///   (`SlidingWindow`), or the whole context (`Full` mode, no split).
///   `Turn` mode has no history at all, so label `1` never occurs there.
/// - `2` = verbatim current-turn suffix (`SlidingWindow` and `Turn`).
/// - `3` = folded earlier work in the current turn (`SlidingWindow` and
///   `Turn`).
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
/// disables capping. The two folded regions follow the same rule: each
/// replays its raw messages with oversized fields (result content, user
/// and assistant text, call arguments, reasoning, Anthropic text blocks)
/// cut to [`FOLDED_FIELD_REPLAY_CAP`], head and tail kept, and image
/// blocks replaced by a text placeholder.
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
        ContextStrategy::SlidingWindow | ContextStrategy::Turn => {
            // A stale `prior_len` must not index past the context: the
            // cancellation truncation is the only shrink, and it clamps
            // `prior_len` itself, but the slice math should not depend on
            // the caller having done so.
            let boundary = prior_len.min(raw_context.len());
            let current = &raw_context[boundary..];

            if cache.boundary != Some(boundary) {
                // The prior is the anchored window the loader read: its
                // head only moves when the loader slides the anchor, so the
                // folded history only ever grows by append — the
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

            let checkpoint_end = current_turn_checkpoint_end(
                current,
                strategy.checkpoint_interval,
                strategy.checkpoint_growth,
            )
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
                // Defensive: a checkpoint with nothing to replay (an empty
                // prefix) leaves the whole current turn verbatim instead of
                // slicing a hole into it.
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
            let turn_cap = jyc_types::channel::effective_result_cap(strategy.turn_tool_result_cap);
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
            (ContextStrategy::SlidingWindow | ContextStrategy::Turn, 1) => ContextRegion::History,
            (ContextStrategy::SlidingWindow | ContextStrategy::Turn, 3) => {
                ContextRegion::Checkpoint
            }
            (ContextStrategy::SlidingWindow | ContextStrategy::Turn, 2) => ContextRegion::Current,
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
/// checkpoint. Not persisted to `raw_context`. The checkpoint replays the
/// turn's own messages — oversized fields (tool results, text, call
/// arguments and their string values, reasoning, Anthropic text blocks) cut
/// to [`FOLDED_FIELD_REPLAY_CAP`] with head and tail kept, images replaced
/// by a placeholder naming their media type — so the marker's "truncated in
/// the middle" describes the fields, and the placeholder states the
/// substitution itself.
pub(crate) const TURN_CHECKPOINT_MARKER_TEXT: &str = "[Current-turn checkpoint] earlier work in this turn follows — the \
     messages as they were sent, oversized fields truncated in the middle.";

/// Where the verbatim current-turn suffix starts, relative to the current
/// turn slice.
///
/// `interval` is [`ContextStrategyConfig::checkpoint_interval`], defaulting to
/// [`jyc_types::channel::DEFAULT_CHECKPOINT_INTERVAL`]; `0` disables the
/// in-turn checkpoint and always returns `None`. `growth` is
/// [`ContextStrategyConfig::checkpoint_growth`]: the first fold fires at
/// `interval` and each later one widens the next interval by
/// `interval / growth`, so the verbatim tail grows with the turn. `0` keeps
/// every interval at `interval`.
///
/// With `n` completed assistant messages, fold the widest whole run of
/// checkpoints that still leaves the latest interaction raw — see
/// [`folded_assistant_count`].
///
/// The boundary is derived from the slice alone, so a cold build and a warm
/// cache produce the same payload. It advances only when `n` crosses the
/// next checkpoint, which keeps the folded prefix byte-stable in between.
fn current_turn_checkpoint_end(
    current: &[serde_json::Value],
    interval: usize,
    growth: usize,
) -> Option<usize> {
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
    let folded = folded_assistant_count(assistant_count, interval, growth);
    current
        .iter()
        .enumerate()
        .filter(|(_, msg)| msg.get("role").and_then(|r| r.as_str()) == Some("assistant"))
        .nth(folded)
        .map(|(idx, _)| idx)
}

/// How many leading assistant messages fold, given `n` of them in the
/// current turn.
///
/// The checkpoints partition the turn: checkpoint `k` (1-based) is `width_k`
/// assistant messages wide, and it folds once the turn has passed the sum of
/// the widths before it plus one — the trailing interaction always stays
/// raw.
///
/// ```text
/// width_k = interval × (growth + k - 1) / growth        (growth > 0)
/// width_k = interval                                    (growth = 0)
/// folded  = Σ width_k, the widest such sum ≤ n - 1
/// ```
///
/// With `growth = 0` every width is `interval`, so the sum is
/// `floor((n - 1) / interval) × interval` — the fixed interval this replaces.
///
/// Each width is at least `interval` (the first factor is never below
/// `growth`), so the walk advances by at least one assistant message per
/// step and terminates in at most `n / interval` iterations.
fn folded_assistant_count(n: usize, interval: usize, growth: usize) -> usize {
    // The walk tests `n - 1`, so an empty turn needs its own exit: with
    // `n == 0` a release build wraps that subtraction to `usize::MAX` and
    // the loop below never terminates. The only caller already implies
    // `n > interval >= 1`; this keeps the helper total for any caller.
    if n == 0 || interval == 0 {
        return 0;
    }
    let mut folded = 0usize;
    let mut completed = 0usize;
    loop {
        let width = if growth == 0 {
            interval
        } else {
            // `interval × (growth + completed) / growth`, written as
            // `interval + interval × completed / growth` so the fraction
            // keeps its value at both ends: an absurdly large `growth` (the
            // "never widen" intent expressed as a number rather than as 0)
            // would otherwise saturate the product to `usize::MAX` and
            // divide back down to 1, folding the whole turn.
            let extra = interval.saturating_mul(completed).saturating_div(growth);
            interval.saturating_add(extra)
        };
        let next = folded.saturating_add(width);
        if next > n - 1 {
            return folded;
        }
        folded = next;
        completed += 1;
    }
}

/// Per-field byte bound for the oversized text fields a folded region
/// replays — tool-result content, `tool_calls[].function.arguments` values
/// and `reasoning_content`, in both the history block and the current-turn
/// checkpoint.
///
/// The checkpoint's job is to hold still: the model saw these messages
/// verbatim earlier this turn, so the replay is a reminder and an index,
/// not the payload. The history's job is the same one turn back: the
/// conclusion survives, and the replay says what was checked to reach it
/// without re-riding every byte. In both, small enough that folding stays
/// cheaper than the verbatim steps it replaces, large enough to say which
/// call it answers and what came back (head and tail kept, the middle
/// replaced by the field's own paging marker). The full text stays one
/// `context_browse results=true` away. Not a knob: the folded shape is a
/// strategy invariant, and a second tunable beside `checkpoint_interval`
/// would be one more wire shape to keep honest.
const FOLDED_FIELD_REPLAY_CAP: usize = 512;

const _: () = assert!(
    FOLDED_FIELD_REPLAY_CAP >= jyc_types::channel::MIN_EDGE_KEEPING_CAP,
    "folded field replay cap must hold the paging marker plus both ends",
);

/// Build the history block from the prior window: the history marker,
/// then every prior message replayed verbatim.
///
/// Nothing here trims: the prior is already the anchored window the loader
/// bounded, and any head-trim here would rewrite the wire prefix on every
/// turn — the bug this shape exists to avoid. The block only grows, so the
/// payload is append-only from one turn to the next.
///
/// Why the raw messages rather than a summary: the model wrote these turns
/// itself, and the shape it wrote them in is the shape it must keep
/// reading. Rendering calls as a text note taught a model that a tool call
/// is text it may write as content; replaying the original
/// assistant/tool messages keeps the call/result pairing the provider
/// protocol requires. The cap leaves each oversized field's head and tail,
/// so the evidence ("read OK", "the file says X") survives while the
/// middle goes through its own paging marker.
fn build_history_region(
    provider: &dyn Provider,
    prior: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    build_verbatim_region(
        provider,
        prior,
        crate::session::HISTORY_MARKER_TEXT,
        FOLDED_FIELD_REPLAY_CAP,
    )
}

/// Fold the older part of the current turn into the same replayed shape as
/// prior-turn history.
fn build_checkpoint_region(
    provider: &dyn Provider,
    prefix: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    build_verbatim_region(
        provider,
        prefix,
        TURN_CHECKPOINT_MARKER_TEXT,
        FOLDED_FIELD_REPLAY_CAP,
    )
}

/// The one folded-region builder shared by history and current-turn
/// checkpoints: the marker, then `raw` replayed **verbatim**.
///
/// Each message keeps its original wire shape — role, string or block
/// content, `tool_calls`, `tool_call_id`, `reasoning_content` — so the
/// model reads back what it actually wrote, not this layer's summary of it.
/// The only rewrite is [`cap_folded_fields`]: oversized fields are cut to
/// `field_cap` with head and tail kept. A message whose legacy inline
/// reasoning block was stripped down to nothing is dropped
/// ([`cap_folded_fields`] returns `None`).
///
/// `field_cap` is the per-field byte bound. Both callers pass
/// [`FOLDED_FIELD_REPLAY_CAP`]; the parameter stays so the bound travels
/// with the call site that chose it.
///
/// Each message depends only on itself, so the block is append-only: a new
/// turn appends its own messages and cannot rewrite an earlier one — that
/// is what keeps the provider's cached prefix intact from turn to turn.
/// A call and its result travel together because the caller's slice ends at
/// a turn boundary, so the dangling-call repair cannot fire in the middle of
/// the block. It can fire at the end: a journal entry lost to a torn write
/// or an unparsable line leaves a call without its result, and
/// [`crate::context_reader::ContextReader::read_from`] repairs that on the
/// way in — the cost is the remainder of that turn, never a later turn
/// (a suffix drop would freeze the anchored window on the pre-corruption
/// prefix).
fn build_verbatim_region(
    provider: &dyn Provider,
    raw: &[serde_json::Value],
    marker: &str,
    field_cap: usize,
) -> Vec<serde_json::Value> {
    // The marker leads the block and explains the shape; without a prior
    // there is nothing to introduce.
    if raw.is_empty() {
        return Vec::new();
    }
    let mut region: Vec<serde_json::Value> =
        vec![provider.format_user_message(&[ContentBlock::Text {
            text: marker.into(),
        }])];
    region.extend(
        raw.iter()
            .filter_map(|msg| cap_folded_fields(msg, field_cap)),
    );
    region
}

/// One folded-region message with its oversized text fields cut to `cap`;
/// `None` when nothing is left to replay.
///
/// The field classes that carry the bulk of a transcript are capped:
///
/// - tool-result content, in both wire shapes (delegated to
///   [`cap_tool_result_content_with`]);
/// - each `tool_calls[].function.arguments` string: parsed and re-serialized
///   so the inner argument values cap individually; arguments that are not
///   JSON (legacy or hand-edited) have their raw string capped instead;
/// - `reasoning_content`;
/// - string `content` (a user instruction or a reply), the same cap the
///   block shape's `text` blocks get — otherwise the same turn would be cut
///   on a Claude topic and ride whole on a chat one;
/// - block-shaped content: `text` blocks and every `tool_use.input` string
///   value ([`cap_block_fields`]).
///
/// Everything else — roles, ids, tool names, short strings — rides
/// untouched. Each field's inner strings are capped individually, so a
/// message with many short arguments keeps them all; the bound is per
/// field, not per message (`chunked`/`truncated` and the paging marker are
/// the per-item story, not an aggregate budget). A call's `arguments`
/// object is likewise not bounded as a whole: every string value is capped,
/// but many short values, or many array elements, ride at their own size.
///
/// One substitution is not a cut: an `image` / `image_url` block becomes a
/// short text placeholder, because a base64 payload replayed on every
/// folded request is not history the model can use. Both folded regions
/// substitute — history *and* the current turn's checkpoint, so an image
/// loaded early in a long turn is gone from the checkpoint while the
/// verbatim recent suffix keeps it.
///
/// The one deletion is the replay-only reasoning rule, applied to string
/// content: the inline duplicate of a non-empty `reasoning_content` is
/// stripped (the extraction layer's field-matched rule, which the old
/// rebuild applied too), and a fieldless message whose content opens with a
/// well-formed inline block loses that block — mirroring the Responses
/// wire's unconditional send-time strip, because no in-tree builder writes
/// a block without the field and replaying reasoning as reply text is what
/// taught models to inline it. A message whose content *was* that inline
/// block is dropped unless it carries structured calls (dropping it would
/// orphan the results); a content that was already empty before the strip
/// is never dropped — no strip produced it, and a `role:"tool"` answer can
/// legitimately be empty (dropping it would orphan its call the same way).
/// The strip runs against the field's original value, before the
/// cap — a capped field no longer matches the inline block it duplicates.
/// Block-shaped content is never inline-stripped: the builders never
/// inlined there, and a block list's text blocks are capped instead.
///
/// Key order: `serde_json` without the `preserve_order` feature stores
/// objects as a `BTreeMap`, so an arguments string that had to be rebuilt
/// is re-serialized with its keys sorted. Only capped arguments change
/// bytes; untouched messages keep their original layout.
fn cap_folded_fields(msg: &serde_json::Value, cap: usize) -> Option<serde_json::Value> {
    let mut capped =
        cap_tool_result_content_with(msg, cap, crate::session::truncate_text_keeping_edges);
    let Some(obj) = capped.as_object_mut() else {
        return Some(capped);
    };

    // The inline duplicate is stripped against the field's *original* value:
    // the capped one carries the paging marker, which no inline block
    // contains, so a prefix match against it can never succeed.
    let original_reasoning = obj
        .get("reasoning_content")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);

    if let Some(reasoning) = &original_reasoning {
        let cut = crate::session::truncate_text_keeping_edges(reasoning, cap);
        if cut != *reasoning {
            obj.insert("reasoning_content".into(), serde_json::Value::String(cut));
        }
    }

    if let Some(calls) = obj.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
        for call in calls {
            let Some(arguments) = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            let cut = cap_arguments(&arguments, cap);
            if cut != arguments
                && let Some(function) = call.get_mut("function").and_then(|f| f.as_object_mut())
            {
                function.insert("arguments".into(), serde_json::Value::String(cut));
            }
        }
    }

    // Block-shaped content (the Anthropic wire shape) is capped block by
    // block: text blocks, each `tool_use.input` value, and images replaced
    // by a placeholder — a base64 payload would otherwise ride every folded
    // replay untouched.
    cap_block_fields(obj, cap);

    // String content is capped like a text block: the two wire shapes must
    // not differ in what survives a folded replay (a chat journal stores the
    // user's instruction and the reply as a string, an Anthropic journal as
    // a text block). The assistant strip runs first, against the original
    // inline block the cap would no longer match.
    if let Some(original) = obj
        .get("content")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
    {
        let mut text = original.clone();
        if obj.get("role").and_then(serde_json::Value::as_str) == Some("assistant") {
            text = match original_reasoning.as_deref() {
                Some(reasoning) if !reasoning.is_empty() => {
                    crate::session::strip_duplicated_inline_reasoning(&text, reasoning)
                }
                _ => crate::session::strip_well_formed_leading_block(&text).unwrap_or(text),
            };
        }
        // Only the assistant strip above can empty a field; a `content` that
        // was empty to begin with (a legacy journal line, a user turn whose
        // blocks carried no text) is replayed exactly as the builders wrote
        // it. Dropping it instead would erase a turn boundary — and a
        // `role:"tool"` answer carries no text at all when the tool returned
        // an empty string, so dropping it would orphan the call its
        // assistant side keeps.
        let stripped_empty = text.is_empty() && !original.is_empty();
        if !text.is_empty() {
            let cut = crate::session::truncate_text_keeping_edges(&text, cap);
            if cut != original {
                obj.insert("content".into(), serde_json::Value::String(cut));
            }
        } else if stripped_empty {
            // The inline duplicate was the whole content: a block-only
            // reasoning turn. Keep the message when it carries calls
            // (dropping it would orphan the results); `null` beside calls is
            // the shape the builders write.
            if !has_tool_calls(obj) {
                return None;
            }
            obj.insert("content".into(), serde_json::Value::Null);
        }
    }

    Some(capped)
}

/// Whether the message carries a non-empty `tool_calls` array.
///
/// The only caller has already established that `content` is a string, so
/// the Anthropic `tool_use` blocks are not a case here: a block-shaped
/// message never reaches this test.
fn has_tool_calls(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    obj.get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|calls| !calls.is_empty())
}

/// Cap one folded message's block-shaped `content` in place.
///
/// Handles the Anthropic wire shape, which the journal keeps when Claude is
/// the active provider: `text` blocks are cut like string content,
/// `tool_use.input` has its string values capped (a `write` call's file
/// body is the payload that matters), and `image` / `image_url` blocks are
/// replaced by a short text placeholder — their base64 rides the request
/// on every replay otherwise, and a folded region cannot usefully re-show
/// the image anyway.
fn cap_block_fields(obj: &mut serde_json::Map<String, serde_json::Value>, cap: usize) {
    let Some(blocks) = obj.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return;
    };
    cap_block_list(blocks, cap);
}

/// [`cap_block_fields`] over one block list, recursing into a
/// `tool_result`'s nested block list (a tool may answer with an image).
fn cap_block_list(blocks: &mut [serde_json::Value], cap: usize) {
    for block in blocks.iter_mut() {
        let Some(kind) = block
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        match kind.as_str() {
            "text" => {
                let Some(text) = block
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                else {
                    continue;
                };
                let cut = crate::session::truncate_text_keeping_edges(&text, cap);
                if cut != text
                    && let Some(obj) = block.as_object_mut()
                {
                    obj.insert("text".into(), serde_json::Value::String(cut));
                }
            }
            "tool_use" => {
                if let Some(input) = block.get_mut("input") {
                    cap_json_strings(input, cap);
                }
            }
            "tool_result" => {
                if let Some(nested) = block
                    .get_mut("content")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    cap_block_list(nested, cap);
                }
            }
            "image" | "image_url" => {
                *block = image_placeholder(block);
            }
            _ => {}
        }
    }
}

/// The text block a folded replay shows in place of an image.
///
/// Deterministic in the block it replaces (media type only, no size or
/// random component) so the folded region stays byte-stable from turn to
/// turn.
fn image_placeholder(block: &serde_json::Value) -> serde_json::Value {
    let media = block
        .pointer("/source/media_type")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            let url = block.pointer("/image_url/url")?.as_str()?;
            let rest = url.strip_prefix("data:")?;
            let (media, _) = rest.split_once(';')?;
            Some(media).filter(|m| !m.is_empty())
        });
    serde_json::json!({
        "type": "text",
        "text": format!(
            "[image omitted from folded replay: {}]",
            media.unwrap_or("image")
        ),
    })
}

/// One `tool_calls[].function.arguments` string with every inner string value
/// cut to `cap` bytes, head and tail kept.
///
/// Arguments are a JSON *string* on the wire, so capping the string itself
/// would cut the JSON in half; parsing first caps what the call actually
/// reads. A string that does not parse as JSON is legacy or hand-edited and
/// is capped as raw text.
fn cap_arguments(arguments: &str, cap: usize) -> String {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(mut parsed) => {
            cap_json_strings(&mut parsed, cap);
            parsed.to_string()
        }
        Err(_) => crate::session::truncate_text_keeping_edges(arguments, cap),
    }
}

/// Recursively cut every string value of `value` to `cap` bytes.
fn cap_json_strings(value: &mut serde_json::Value, cap: usize) {
    match value {
        serde_json::Value::String(s) => {
            let cut = crate::session::truncate_text_keeping_edges(s, cap);
            if cut != *s {
                *s = cut;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                cap_json_strings(item, cap);
            }
        }
        serde_json::Value::Object(fields) => {
            for field in fields.values_mut() {
                cap_json_strings(field, cap);
            }
        }
        _ => {}
    }
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
        Some(c) if c < jyc_types::channel::effective_result_cap(Some(c)) => {
            format!(" (lifted from {c})")
        }
        _ => String::new(),
    };
    let mut s = format!(
        "=== context sent to LLM (strategy={:?} window={} turn_cap={}{} max_hist={:?}) ===\n\
         {} msgs, ~{} bytes\n\n",
        strategy.mode,
        strategy.window,
        jyc_types::channel::effective_result_cap(strategy.turn_tool_result_cap),
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
        (1u8, "History — raw messages, oversized fields capped"),
        (3u8, "Current-turn checkpoint — older work, fields capped"),
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
            reasoning: &str,
            _tool_calls: &[(String, String, String)],
        ) -> serde_json::Value {
            let mut msg = json!({"role": "assistant", "content": text});
            if !reasoning.is_empty() {
                msg["reasoning_content"] = json!(reasoning);
            }
            msg
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
    /// carries reasoning twice — the inline `<think>` block plus the
    /// `reasoning_content` field — the shape the compat builder persisted
    /// before it stopped inlining, which legacy journals still hold.
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

    /// A reasoning longer than the replay cap — the size the strip exists
    /// for. A short fixture never gets capped, so it cannot catch a strip
    /// that runs against the capped value and silently does nothing.
    fn long_reasoning() -> String {
        format!("secret {}", "r".repeat(FOLDED_FIELD_REPLAY_CAP * 2))
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
        // divisor, so an unguarded 0 would panic instead of disabling. The
        // growth is set to the fastest widening (1 adds a full interval per
        // fold) to pin the precedence: widening cannot rescue an interval of
        // 0, because the off check runs first and never consults growth.
        let ctx = current_with_interactions(40, "secret");
        let cfg = ContextStrategyConfig {
            checkpoint_interval: 0,
            checkpoint_growth: 1,
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
        // Reasoning past the cap: the realistic legacy value. With a short
        // one the field is never cut, so the inline-strip assertion below
        // would pass whether or not the strip can still match.
        let reasoning = long_reasoning();
        let ctx = current_with_interactions(33, &reasoning);
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
        // The messages replay as they were sent: the reasoning rides its own
        // field, the inline duplicate is stripped as reply text, each call
        // keeps its structured `tool_calls` entry, and each result follows
        // as its own tool message.
        assert!(folded.contains("secret"), "reasoning dropped: {folded}");
        assert!(
            !folded.contains("think>"),
            "inline think tags leaked: {folded}"
        );
        // The checkpoint replays results (cut to FOLDED_FIELD_REPLAY_CAP
        // when oversized; these 8-byte fixtures ride whole) — the fold
        // drops full-size payloads, not the evidence itself.
        assert!(folded.contains("RESULT-"), "results replay: {folded}");
        assert!(folded.contains("step 0") && folded.contains("step 31"));
        // Structured calls and their results, not a text note: the model
        // reads back the message shape it wrote.
        assert!(folded.contains("\"tool_calls\""), "{folded}");
        assert!(folded.contains("\"role\":\"tool\""), "{folded}");
        assert!(!folded.contains("step 32"), "latest step must stay raw");
    }

    #[test]
    fn folded_messages_keep_their_shape_and_strip_a_duplicated_inline_block() {
        let open = concat!("<", "think>");
        let close = concat!("</", "think>");
        let cap = FOLDED_FIELD_REPLAY_CAP;

        // Field + matching inline prefix — the legacy persisted shape: the
        // inline duplicate strips as reply text, the field rides the
        // message itself.
        let msg = json!({
            "role": "assistant",
            "content": format!("{open}why{close}\n\nanswer"),
            "reasoning_content": "why",
        });
        let folded = cap_folded_fields(&msg, cap).expect("reply text survives");
        assert_eq!(folded["content"].as_str().unwrap(), "answer");
        assert_eq!(folded["reasoning_content"].as_str().unwrap(), "why");

        // The same shape above the cap: the field gets cut, and the strip
        // must still fire. It runs against the field's original value —
        // matching against the capped one can never succeed, so the whole
        // inline block (the full reasoning with it) would ride as reply
        // text.
        let wide = format!("why {}", "w".repeat(cap * 2));
        let msg = json!({
            "role": "assistant",
            "content": format!("{open}{wide}{close}\n\nanswer"),
            "reasoning_content": wide,
        });
        let folded = cap_folded_fields(&msg, cap).expect("reply text survives");
        assert_eq!(
            folded["content"].as_str().unwrap(),
            "answer",
            "a capped field must not defeat the strip"
        );
        let field = folded["reasoning_content"].as_str().unwrap();
        assert!(
            field.len() <= cap && field.starts_with("why "),
            "the field is cut, keeping its head: {} bytes",
            field.len()
        );

        // Field without an inline prefix: content untouched.
        let msg = json!({
            "role": "assistant",
            "content": "plain answer",
            "reasoning_content": "why",
        });
        let folded = cap_folded_fields(&msg, cap).expect("reply text survives");
        assert_eq!(folded["content"].as_str().unwrap(), "plain answer");
        assert_eq!(folded["reasoning_content"].as_str().unwrap(), "why");

        // A fieldless leading block with real text after it — legacy or
        // hand-edited content: no in-tree builder writes the block without
        // the field — strips on the replay path, the same shape the
        // Responses wire strips unconditionally at send time: replaying
        // reasoning as reply text is what taught models to inline it.
        let msg = json!({
            "role": "assistant",
            "content": format!("{open}why{close}\n\nanswer"),
        });
        let folded = cap_folded_fields(&msg, cap).expect("text survives");
        assert_eq!(folded["content"].as_str().unwrap(), "answer");
        assert!(folded.get("reasoning_content").is_none());

        // A block-only entry without the field is a reasoning-only turn of
        // the tag era: dropped from the replay (the view still renders it).
        let msg = json!({
            "role": "assistant",
            "content": format!("{open}why{close}"),
        });
        assert!(cap_folded_fields(&msg, cap).is_none());

        // …unless it carries calls: dropping it would leave its `role:
        // "tool"` results orphaned, which no repair removes (the repair
        // only walks assistant messages). Empty content beside calls is
        // the shape the builders write.
        let msg = json!({
            "role": "assistant",
            "content": format!("{open}why{close}"),
            "tool_calls": [{"id": "c1", "type": "function",
                            "function": {"name": "bash", "arguments": "{}"}}],
        });
        let folded = cap_folded_fields(&msg, cap).expect("a call must not be dropped");
        assert!(folded["content"].is_null(), "{}", folded["content"]);
        assert_eq!(folded["tool_calls"][0]["id"], "c1");

        // An unclosed leading block is not well-formed: preserved verbatim
        // (guessing where the block ends would eat real text).
        let msg = json!({
            "role": "assistant",
            "content": format!("{open}why and the answer"),
        });
        let folded = cap_folded_fields(&msg, cap).expect("text survives");
        assert_eq!(
            folded["content"].as_str().unwrap(),
            format!("{open}why and the answer")
        );
    }

    /// A `content` that was empty to begin with is replayed, never dropped:
    /// the drop rule exists for a turn whose text *was* the inline reasoning
    /// duplicate, and applying it to every role erased real messages. A
    /// `role:"tool"` answer with no text would take its call's only result
    /// with it, leaving exactly the orphan shape the fold is there to avoid.
    #[test]
    fn folded_keeps_messages_whose_content_was_already_empty() {
        let cap = FOLDED_FIELD_REPLAY_CAP;

        for msg in [
            json!({"role": "tool", "tool_call_id": "c1", "content": ""}),
            json!({"role": "user", "content": ""}),
            json!({"role": "assistant", "content": ""}),
        ] {
            let folded = cap_folded_fields(&msg, cap)
                .unwrap_or_else(|| panic!("an empty-but-real message was dropped: {msg}"));
            assert_eq!(folded, msg, "an empty field must replay verbatim");
        }
    }

    /// Structured calls survive the fold, and their argument *values* are
    /// what the cap bites — the arguments string stays parseable JSON.
    #[test]
    fn folded_tool_call_arguments_cap_their_string_values() {
        let cap = FOLDED_FIELD_REPLAY_CAP;
        let wide = "x".repeat(cap * 3);
        let msg = json!({
            "role": "assistant",
            "content": "calling",
            "tool_calls": [{"id": "c1", "type": "function", "function": {
                "name": "bash",
                "arguments": json!({"command": wide}).to_string(),
            }}],
        });
        let folded = cap_folded_fields(&msg, cap).expect("text survives");
        assert_eq!(
            folded["tool_calls"][0]["function"]["name"].as_str(),
            Some("bash"),
            "the call itself must survive"
        );
        let arguments = folded["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments stay a JSON string");
        let parsed: serde_json::Value =
            serde_json::from_str(arguments).expect("capped arguments stay valid JSON");
        let command = parsed["command"].as_str().expect("command survives");
        assert!(
            command.len() <= cap,
            "argument value over the cap: {} bytes",
            command.len()
        );
        assert!(
            command.contains("bytes dropped from the middle"),
            "the cut must be stated: {command}"
        );
    }

    /// Oversized reasoning is capped the same way as a result: head and
    /// tail kept, the middle stated.
    #[test]
    fn folded_reasoning_caps_keeping_both_ends() {
        let cap = FOLDED_FIELD_REPLAY_CAP;
        let wide = format!("HEAD-{}{}-TAIL", "h".repeat(cap), "t".repeat(cap));
        let msg = json!({
            "role": "assistant",
            "content": "answer",
            "reasoning_content": wide,
        });
        let folded = cap_folded_fields(&msg, cap).expect("text survives");
        let reasoning = folded["reasoning_content"]
            .as_str()
            .expect("the field survives");
        assert!(reasoning.len() <= cap, "over the cap: {}", reasoning.len());
        assert!(
            reasoning.starts_with("HEAD-") && reasoning.ends_with("-TAIL"),
            "both ends must survive: {reasoning}"
        );
        assert!(
            reasoning.contains("bytes dropped from the middle"),
            "the cut must be stated: {reasoning}"
        );
    }

    /// The Anthropic wire shape carries the same payloads in blocks: text
    /// blocks and `tool_use.input` values must be capped too, or a `write`
    /// call's file body rides both folded regions whole.
    #[test]
    fn folded_anthropic_blocks_cap_text_and_tool_use_input() {
        let cap = FOLDED_FIELD_REPLAY_CAP;
        let wide = "w".repeat(cap * 4);
        let msg = json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": wide},
                {"type": "tool_use", "id": "t1", "name": "write",
                 "input": {"path": "src/main.rs", "content": wide}},
            ],
        });
        let folded = cap_folded_fields(&msg, cap).expect("the message survives");
        let blocks = folded["content"].as_array().expect("blocks stay blocks");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        let text = blocks[0]["text"].as_str().expect("text stays text");
        assert!(text.len() <= cap, "text block over the cap: {}", text.len());
        assert!(
            text.contains("bytes dropped from the middle"),
            "the cut must be stated: {text}"
        );
        // The call itself survives — id, name, and its short argument.
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "t1");
        assert_eq!(blocks[1]["name"], "write");
        assert_eq!(
            blocks[1]["input"]["path"], "src/main.rs",
            "short argument values ride untouched"
        );
        let body = blocks[1]["input"]["content"]
            .as_str()
            .expect("the argument stays a string");
        assert!(
            body.len() <= cap,
            "input value over the cap: {}",
            body.len()
        );
    }

    /// Images have no usable folded form: their base64 would ride every
    /// replay of the region, and the cap cannot help because the payload is
    /// one string. Folded regions substitute a deterministic placeholder;
    /// the current turn still carries the image verbatim.
    #[test]
    fn folded_images_become_a_deterministic_placeholder() {
        let cap = FOLDED_FIELD_REPLAY_CAP;
        let blob = "QUJD".repeat(cap);
        let msg = json!({"role": "user", "content": [
            {"type": "text", "text": "look at this"},
            {"type": "image", "source": {
                "type": "base64", "media_type": "image/png", "data": blob}},
        ]});
        let folded = cap_folded_fields(&msg, cap).expect("the message survives");
        let blocks = folded["content"].as_array().expect("blocks stay blocks");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "look at this");
        assert_eq!(blocks[1]["type"], "text", "the image becomes text");
        assert!(
            blocks[1]["text"].as_str().unwrap().contains("image/png"),
            "the placeholder names what was dropped: {}",
            blocks[1]["text"]
        );
        let wire = serde_json::to_string(&folded).unwrap();
        assert!(
            !wire.contains(&blob[..64]),
            "the base64 payload must not ride the folded replay"
        );
        // Deterministic — the same block folds to the same bytes, which is
        // what keeps the replayed prefix byte-stable.
        assert_eq!(cap_folded_fields(&msg, cap).unwrap(), folded);

        // OpenAI's `image_url` shape: the media type comes off the data URL.
        let msg = json!({"role": "user", "content": [
            {"type": "image_url", "image_url": {
                "url": format!("data:image/jpeg;base64,{blob}")}},
        ]});
        let folded = cap_folded_fields(&msg, cap).expect("the message survives");
        assert!(
            folded["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("image/jpeg")
        );

        // A tool result may answer with an image too — the nested list is
        // walked as well.
        let msg = json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "text", "text": "screenshot"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/webp", "data": blob}},
            ]},
        ]});
        let folded = cap_folded_fields(&msg, cap).expect("the message survives");
        let nested = &folded["content"][0]["content"];
        assert_eq!(nested[0]["text"], "screenshot");
        assert_eq!(nested[1]["type"], "text");
        assert!(nested[1]["text"].as_str().unwrap().contains("image/webp"));
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

        // The second interval is wider than the first (default growth 8:
        // 32 then 36), so the boundary that the fixed interval put at 65 now
        // sits at 69.
        for i in 34..65 {
            push_interaction(&mut ctx, i, "secret");
        }
        let _ = build_send_context_cached(&prov(), &ctx, 0, &sliding_cfg(), &mut cache);
        assert_eq!(
            cache.checkpoint, first,
            "65 interactions do not reach the widened second interval"
        );
        assert_eq!(cache.checkpoint_rebuilt(), Some(false));

        for i in 65..69 {
            push_interaction(&mut ctx, i, "secret");
        }
        let _ = build_send_context_cached(&prov(), &ctx, 0, &sliding_cfg(), &mut cache);
        assert_ne!(cache.checkpoint, first, "69 interactions advance once");
        assert_eq!(cache.checkpoint_rebuilt(), Some(true));
    }

    #[test]
    fn checkpoint_growth_widens_each_interval() {
        // Default base 32 with growth 8 widens by 32/8 = 4 per fold, so the
        // checkpoints are 32 and 36 wide and the boundaries land on 33 and
        // 69 completed assistant messages.
        assert_eq!(folded_assistant_count(33, 32, 8), 32);
        assert_eq!(folded_assistant_count(65, 32, 8), 32);
        assert_eq!(folded_assistant_count(68, 32, 8), 32);
        assert_eq!(folded_assistant_count(69, 32, 8), 68);

        // growth 0 is the fixed interval this replaced — the second boundary
        // stays at 65, which is exactly what the widening above moves.
        assert_eq!(folded_assistant_count(33, 32, 0), 32);
        assert_eq!(folded_assistant_count(65, 32, 0), 64);
        assert_eq!(folded_assistant_count(69, 32, 0), 64);

        // growth 1 adds a full interval per fold: 32 then 64 wide, so the
        // second boundary is at 97 rather than 69.
        assert_eq!(folded_assistant_count(96, 32, 1), 32);
        assert_eq!(folded_assistant_count(97, 32, 1), 96);

        // An interval wider than growth never floors to a narrower step: the
        // first checkpoint is always exactly `interval` wide.
        assert_eq!(folded_assistant_count(3, 2, 8), 2);
        assert_eq!(folded_assistant_count(4, 2, 8), 2);

        // A huge growth is "never widen" expressed as a number. It must stay
        // at the base width — saturating the product and dividing back down
        // would give a width of 1 and fold the whole turn.
        assert_eq!(folded_assistant_count(100, 32, usize::MAX), 96);

        // The empty turn exits instead of underflowing `n - 1`.
        assert_eq!(folded_assistant_count(0, 32, 8), 0);
    }

    #[test]
    // The folded checkpoint replays the messages' own shape whatever
    // provider is active — the journal holds that provider's wire shape,
    // and Anthropic's block content is capped field by field like the chat
    // shape's strings. The Anthropic conversion then drops the folded
    // reasoning: `assistant_blocks` builds blocks from `content` alone and
    // ignores `reasoning_content` (a thinking block cannot be forged
    // without an API signature). The compat wire keeps the field — that is
    // the replay the model wrote.
    fn current_turn_checkpoint_anthropic_conversion_drops_folded_reasoning() {
        // Past the cap on purpose: a short fixture never gets truncated, so
        // it cannot catch the strip matching against the *capped* field and
        // leaving the inline block in `content` — the shape that leaked
        // folded reasoning onto the Anthropic wire.
        let folded_reasoning = long_reasoning();
        let mut ctx = current_with_interactions(33, &folded_reasoning);
        // The latest step is retained verbatim, so give it a distinct
        // reasoning marker: only the folded marker may disappear from the
        // Anthropic wire.
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
        // The folded region carries the raw chat messages: no provider
        // reformatting happens inside a fold, so a provider switch cannot
        // change the block's bytes.
        for (msg, region) in sent.iter().zip(&regions) {
            if *region == 3 {
                assert!(msg.get("role").is_some(), "raw message expected: {msg}");
            }
        }

        // The fixture's raw suffix is chat-shaped (a journal can mix shapes
        // after a provider switch, and this fixture predates one), so
        // exercising both conversions covers the mixed payload the agent
        // loop actually sends.
        let openai = serde_json::to_string(&crate::provider::filter_valid_messages(&sent)).unwrap();
        let anthropic = serde_json::to_string(&crate::provider::filter_valid_messages(
            &crate::provider::chat_to_anthropic_messages(&sent),
        ))
        .unwrap();
        assert!(
            openai.contains("secret"),
            "the compat replay keeps the folded reasoning field: {openai}"
        );
        assert!(
            !anthropic.contains("secret"),
            "folded reasoning reached the Anthropic wire: {anthropic}"
        );
        for wire in [&openai, &anthropic] {
            assert!(
                wire.contains("RESULT-0"),
                "folded results replay truncated: {wire}"
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

    /// Turn mode loads an empty prior, so a short turn rides verbatim —
    /// exactly like `Full`, but by construction rather than by decision.
    #[test]
    fn turn_mode_short_turn_rides_verbatim() {
        let ctx = current_with_interactions(3, "secret");
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::Turn,
            ..sliding_cfg()
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, 0, &cfg);
        assert_eq!(sent.len(), ctx.len(), "nothing to fold below the interval");
        assert!(regions.iter().all(|r| *r == 2), "every message is current");
        let dumped = serde_json::to_string(&sent).unwrap();
        assert!(
            !dumped.contains(crate::session::HISTORY_MARKER_TEXT),
            "turn mode has no history block: {dumped}"
        );
    }

    /// The checkpoint machinery is the only fold in turn mode, and it is
    /// shared with sliding-window: once the turn passes the interval, older
    /// steps fold with their truncated results while the suffix rides
    /// verbatim.
    #[test]
    fn turn_mode_folds_a_long_turn_through_the_checkpoint() {
        let ctx = current_with_interactions(33, "secret");
        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::Turn,
            ..sliding_cfg()
        };
        let (sent, regions) = build_send_context_with_regions(&prov(), &ctx, 0, &cfg);
        let dumped = serde_json::to_string(&sent).unwrap();
        assert!(
            dumped.contains(TURN_CHECKPOINT_MARKER_TEXT),
            "a long turn folds through the checkpoint: {dumped}"
        );
        assert!(
            dumped.contains("RESULT-32"),
            "the verbatim suffix keeps recent results: {dumped}"
        );
        assert!(
            regions.contains(&3),
            "the checkpoint region is labeled: {regions:?}"
        );
        assert!(
            regions.contains(&2),
            "the verbatim suffix region is labeled: {regions:?}"
        );
        assert!(
            !regions.contains(&1),
            "no history region exists in turn mode: {regions:?}"
        );
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
        // the transcript view: no note machinery renders them there.
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
    fn sliding_window_replays_prior_turns_verbatim() {
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

        // Every prior turn — u1/a1, u2 with its tool call and result, u3/a3 —
        // replays as the messages it was sent as: the call/result pair keeps
        // its structured shape, nothing is reformatted per provider.
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

        // 1. History marker, then every prior message verbatim. Nothing
        //    trims the history block: the window bound is the loader's, not
        //    this function's.
        assert_eq!(
            sent[0],
            json!({"role": "user", "content": crate::session::HISTORY_MARKER_TEXT})
        );
        assert_eq!(&sent[1..1 + prior.len()], prior.as_slice());

        // 2. Current turn verbatim (structured tool_calls preserved).
        assert_eq!(&sent[1 + prior.len()..], current.as_slice());
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
        // The property the strategy exists for: history folds each turn to
        // a shape that depends on that turn alone, so every turn's history
        // block is a prefix of the next turn's — the provider's cached
        // prefix reaches the whole of it, and only the turn that just ended
        // is re-encoded (as it graduates into the folded block) together
        // with the current turn.
        let prov = prov();
        let cfg = ContextStrategyConfig {
            ..ContextStrategyConfig::default()
        };
        // Every turn makes a call, so its structured `tool_calls` and
        // result are part of what has to stay byte-stable — a shape that
        // only folds plain text would never exercise that property.
        let turn = |i: usize| {
            [
                json!({"role": "user", "content": format!("u{i}")}),
                json!({"role": "assistant", "content": format!("a{i}"), "tool_calls": [
                    {"id": format!("c{i}"), "type": "function",
                     "function": {"name": "bash", "arguments": format!(r#"{{"command":"c{i}"}}"#)}}
                ]}),
                json!({"role": "tool", "tool_call_id": format!("c{i}"), "content": format!("R{i}")}),
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

        // The current turn is three raw messages: assert the whole tail,
        // not two elements of it — `ends_with(&[2, 2])` holds for a tail of
        // any length at least two.
        assert_eq!(
            regions_5.len() - region1_5,
            3,
            "the current turn is three messages: {regions_5:?}"
        );
        assert!(
            regions_5[region1_5..].iter().all(|&r| r == 2)
                && regions_6[region1_6..].iter().all(|&r| r == 2),
            "the current turn is the only region-2 block: {regions_5:?} / {regions_6:?}"
        );
        assert!(region1_5 > 0, "the marker and four turns");
        assert_eq!(
            region1_6,
            region1_5 + 3,
            "history grew by exactly one turn: its three raw messages"
        );
        assert_eq!(
            at_6[..region1_5],
            at_5[..region1_5],
            "the whole history block is a byte-stable prefix from one turn to the next"
        );
        // Everything past the history block is the current turn, and the
        // history block's own tail is the turn that just graduated into it,
        // replayed exactly as it was while it was current.
        let turn5 = turn(5);
        assert_eq!(&at_6[region1_5..region1_5 + 3], &turn5[..]);
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
    /// under cache_control on `messages[n-3]`/`[n-2]`). The folded part
    /// replays the raw messages verbatim — conversion happens once, at
    /// send — so this layer synthesizes no block at all, and the
    /// tool_use/tool_result pair stays intact.
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

        // Both prior turns — including u1's tool_use + tool_result — replay
        // verbatim.
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

        // The prior replays verbatim — the tool_use/tool_result pair stays
        // structured, so no block is synthesized (and none can be empty) by
        // this layer.
        assert_eq!(&sent[1..1 + prior.len()], prior.as_slice());

        // Current turn preserved verbatim at the tail.
        let tail_start = sent.len() - current.len();
        assert_eq!(&sent[tail_start..], current.as_slice());
    }

    /// The block shape is capped in a folded region too: an oversized text
    /// block and a `tool_use.input` string are cut, the call stays a
    /// `tool_use` block with the same id, and an image in the current turn's
    /// verbatim suffix rides whole. Sub-cap fixtures (the shape test above)
    /// cannot observe any of this: nothing in them is ever truncated.
    #[test]
    fn sliding_window_caps_oversized_anthropic_blocks_and_keeps_current_turn_images() {
        let cap = FOLDED_FIELD_REPLAY_CAP;
        let wide = "w".repeat(cap * 3);
        let image = json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"},
        });
        let prior = vec![
            json!({"role": "user", "content": [{"type": "text", "text": format!("u1 {wide}")}]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": format!("calling {wide}")},
                {"type": "tool_use", "id": "t1", "name": "write",
                 "input": {"path": "a.txt", "content": wide}},
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
            ]}),
            json!({"role": "user", "content": [{"type": "text", "text": "u2"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "a2"}]}),
        ];
        let current = vec![
            json!({"role": "user", "content": [image.clone()]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "looked"}]}),
        ];
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.extend(current.clone());

        let cfg = ContextStrategyConfig {
            mode: ContextStrategy::SlidingWindow,
            window: 4,
            extend_rate: 0.5,
            turn_tool_result_cap: None,
            max_history_tokens: None,
            // Off, so every capped field below belongs to the history region:
            // the assertion must not be able to pass on a checkpoint instead.
            checkpoint_interval: 0,
            ..ContextStrategyConfig::default()
        };
        let sent = build_send_context_with_regions(&anthropic(), &ctx, prior_len, &cfg)
            .0
            .into_owned();

        // sent[0] is the history marker; the prior follows.
        assert_eq!(sent[0]["content"], crate::session::HISTORY_MARKER_TEXT);
        let hist = &sent[1..1 + prior_len];
        let text = hist[0]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.len() <= cap,
            "an oversized text block must be cut to the cap: {} bytes",
            text.len()
        );
        assert!(text.contains("dropped from the middle"), "{text}");

        // The call stays a structured block with its arguments, capped.
        let tool_use = &hist[1]["content"][1];
        assert_eq!(tool_use["type"], "tool_use");
        assert_eq!(tool_use["id"], "t1");
        let argument = tool_use["input"]["content"].as_str().unwrap();
        assert!(
            argument.len() <= cap,
            "an oversized tool_use.input string must be cut too: {} bytes",
            argument.len()
        );
        assert_eq!(tool_use["input"]["path"], "a.txt");

        // Not byte-verbatim any more — that is the point of the fold.
        assert_ne!(hist, prior.as_slice());

        // The current turn's suffix keeps the image itself.
        let tail_start = sent.len() - current.len();
        assert_eq!(&sent[tail_start..], current.as_slice());
        assert_eq!(sent[tail_start]["content"][0], image);
    }

    /// A prior turn's calls and results reach the wire as the messages they
    /// were sent as — structured `tool_calls` and a `role:"tool"` result —
    /// never as a text note, and never merged into the assistant text.
    #[test]
    fn history_replays_tool_calls_and_results_as_messages() {
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

        // History: marker, then the prior messages verbatim (the call and
        // its result keep their structured shape), then the current turn.
        let mut expected = vec![json!({
            "role": "user",
            "content": crate::session::HISTORY_MARKER_TEXT
        })];
        expected.extend(prior.clone());
        expected.push(current[0].clone());
        assert_eq!(sent, expected);
    }

    /// Each replayed result is cut when the region is built, with the
    /// edge-keeping truncation the current-turn cap uses: head and tail
    /// survive, the dropped middle is stated, and the result never exceeds
    /// the fixed cap (the marker lives inside the budget).
    #[test]
    fn checkpoint_result_is_cut_keeping_both_ends() {
        let result = format!("HEAD-{}{}-TAIL", "h".repeat(300), "m".repeat(4000));
        let prefix = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": result}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let region = build_checkpoint_region(&prov(), &prefix);
        assert_eq!(region[3]["role"], "tool");
        let text = region[3]["content"].as_str().expect("the result is text");
        assert!(
            text.len() <= FOLDED_FIELD_REPLAY_CAP,
            "result over its own cap: {} bytes",
            text.len()
        );
        assert!(
            text.contains("bytes dropped from the middle"),
            "middle cut must be stated: {text}"
        );
        assert!(text.starts_with("HEAD-"), "head must survive: {text}");
        assert!(text.ends_with("-TAIL"), "tail must survive: {text}");
    }

    /// Every replayed message obeys the same cache invariant: it is a
    /// function of itself alone, so the block stays a byte-stable prefix as
    /// later turns arrive.
    #[test]
    fn checkpoint_messages_stay_a_byte_stable_prefix() {
        let short = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "file.txt"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let mut long = short.clone();
        for i in 2..=6 {
            long.push(json!({"role": "user", "content": format!("u{i}")}));
            long.push(json!({"role": "assistant", "content": format!("a{i}")}));
        }
        let short_region = build_checkpoint_region(&prov(), &short);
        let long_region = build_checkpoint_region(&prov(), &long);
        assert_eq!(
            short_region,
            long_region[..short_region.len()],
            "a replayed message must not change when later turns arrive"
        );
    }

    /// The current-turn checkpoint replays the raw messages — the structured
    /// call and the tool message that answers it — with the marker up front.
    #[test]
    fn checkpoint_replays_results_cut_to_the_fixed_cap() {
        let prefix = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "file.txt"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let region = build_checkpoint_region(&prov(), &prefix);
        assert_eq!(region[0]["content"], TURN_CHECKPOINT_MARKER_TEXT);
        assert_eq!(region[2]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(region[3]["role"], "tool");
        assert_eq!(region[3]["content"], "file.txt");
    }

    /// End to end: the history region replays results, each cut to the
    /// fixed cap — the marker states the truncation, and an oversized
    /// result keeps head and tail with the middle stated.
    #[test]
    fn build_send_context_history_replays_results_cut_to_the_cap() {
        let result = format!("HEAD-{}{}-TAIL", "h".repeat(300), "m".repeat(4000));
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": result}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let current = vec![json!({"role": "user", "content": "u2"})];
        let prior_len = prior.len();
        let mut ctx = prior.clone();
        ctx.extend(current.clone());

        let sent = build_send_context_with_regions(&prov(), &ctx, prior_len, &sliding_cfg())
            .0
            .into_owned();

        assert_eq!(sent[0]["content"], crate::session::HISTORY_MARKER_TEXT);
        let body = sent
            .iter()
            .filter(|m| m["role"] == "tool")
            .find_map(|m| m["content"].as_str())
            .expect("history must replay the truncated result");
        assert!(
            body.len() <= FOLDED_FIELD_REPLAY_CAP,
            "the cut result fits in the cap, marker included: {} bytes",
            body.len()
        );
        // Conservation over the stated gap: head + announced middle + tail
        // must account for every byte of the original result — a cut that
        // lost or duplicated an end fails here whatever the sizes are.
        let (head, rest) = body.split_once("\n… [").expect("marker opener");
        let (dropped, rest) = rest
            .split_once(" bytes dropped from the middle")
            .expect("marker body");
        let dropped: usize = dropped.parse().unwrap();
        let tail = rest.split_once("] …\n").expect("marker closer").1;
        assert_eq!(
            head.len() + dropped + tail.len(),
            result.len(),
            "the cut conserves bytes: {head_len} head + {dropped} gap + {tail_len} tail \
             != {} original",
            result.len(),
            head_len = head.len(),
            tail_len = tail.len(),
        );
        assert!(head.starts_with("HEAD-h"), "the head survives: {head:?}");
        assert!(tail.ends_with("-TAIL"), "the tail survives: {tail:?}");
    }

    /// A short result rides the history verbatim: the cap only bites
    /// oversized ones.
    #[test]
    fn history_keeps_a_short_result_verbatim() {
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1", "tool_calls": [{"id":"1","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "file.txt"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let region = build_history_region(&prov(), &prior);
        assert_eq!(region[3]["role"], "tool");
        assert_eq!(region[3]["content"], "file.txt");
    }

    /// A text-only turn replays as exactly its two messages: nothing is
    /// synthesized for it (no notes, no empty blocks).
    #[test]
    fn history_replays_a_text_only_turn_verbatim() {
        let prior = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1"}),
        ];
        let region = build_history_region(&prov(), &prior);
        assert_eq!(
            region,
            vec![
                json!({"role": "user", "content": crate::session::HISTORY_MARKER_TEXT}),
                prior[0].clone(),
                prior[1].clone(),
            ]
        );
    }

    /// The cache invariant behind the replay: every replayed message is a
    /// function of **itself alone**. Were it "the last N turns" or budgeted
    /// per block, a new turn would rewrite the middle of the block and the
    /// provider's cached prefix would stop matching.
    #[test]
    fn replayed_messages_do_not_depend_on_later_turns() {
        let prov = prov();
        let short = vec![
            json!({"role": "user", "content": "u1"}),
            json!({"role": "assistant", "content": "a1", "tool_calls": [
                {"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "file.txt"}),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ];
        let mut long = short.clone();
        for i in 3..=12 {
            long.push(json!({"role": "user", "content": format!("u{i}")}));
            long.push(json!({"role": "assistant", "content": format!("a{i}")}));
        }

        let short_region = build_history_region(&prov, &short);
        let long_region = build_history_region(&prov, &long);

        assert_eq!(short_region.len(), 6, "marker + five raw messages");
        assert_eq!(
            short_region,
            long_region[..short_region.len()],
            "a message must render identically with ten more turns behind it"
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
            dump.contains("History — raw messages"),
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
        // Every prior turn now goes into the same folded block, so the
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
    /// history block folds (text plus calls) and is unbounded by it.
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
            !content.contains("truncated"),
            "a head-only cut would say 'truncated' inside the tool content itself: {content:?}"
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
