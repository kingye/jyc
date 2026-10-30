//! Shared token-display helpers for the dashboard TUI.
//!
//! The topic info pane (the chat screen's right column, and the overview's
//! twin) pushes one `Line` per row into a `Vec<Line>`, and these helpers
//! build the `Span`s of those rows — the token counts, the two percentage
//! rows with their bars, the cost and latency figures — so the format and
//! styling stay consistent across the pane and `/info`.
//!
//! Kept module-private (`pub(super)`) — these are TUI internals, not part
//! of the dashboard's public surface.

use jyc_core::duration::format_latency;
use jyc_types::{ContextRegion, ContextRegionInfo, TopicInfo};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

/// The label of the token-usage row.
const TOKENS_LABEL: &str = "Tokens: ";

/// The label of the latest turn's cache row — it names the scope; the
/// group the cache rows belong to carries its own header.
const CACHE_TURN_LABEL: &str = "Turn: ";

/// The label of the session's cache row.
const CACHE_SESSION_LABEL: &str = "Session: ";

/// The label of the cache-creation row — the cache's write volume.
const CACHE_CREATE_LABEL: &str = "Create: ";

/// The label of the day's spend row — the longest label of the pane, so
/// it sets the width every other label is padded to.
const TODAY_ALL_LABEL: &str = "Today (all): ";

/// The width every label of the info pane is padded to, so that every
/// value in the pane starts in the same column and the pane reads as one
/// table instead of a ragged list. The longest label sets it: a longer
/// one would leave less room for the values than the value column
/// allows, and the wrapping would cut them.
///
/// A label longer than this pushes its own value out of the column —
/// raise it when a longer label arrives.
pub(super) const LABEL_WIDTH: usize = TODAY_ALL_LABEL.len();

/// The glyph a bar draws where it has something to show: the heavy rule.
/// Every filled part of the pane's bars uses it — the share of a percentage
/// bar, and every cell of the context composition, whose cells are all a
/// region of the context. One cell wide in every font, so a segment's width
/// is exactly its share of the bar.
const BAR_FULL: &str = "━";

/// The glyph a bar draws where it has nothing to show: the light rule, the
/// same line as [`BAR_FULL`] one weight down, so a bar reads as one rule
/// whose filled part is heavier. Only a percentage bar draws it — the part
/// of the bar the share does not reach. The context composition has no such
/// part: every cell of it is a region.
const BAR_EMPTY: &str = "─";

/// The colour of a context region, shared by its bar segment and its legend
/// label so the two identify the same part of the context.
fn context_region_color(region: ContextRegion) -> Color {
    match region {
        ContextRegion::Full | ContextRegion::History => Color::Green,
        ContextRegion::Checkpoint => Color::Yellow,
        ContextRegion::Current => Color::Cyan,
    }
}

/// A region's legend label: padded to `label_width` and followed by one
/// space, so the shares line up in one column under the bar. Coloured to
/// match the region's segment.
///
/// The label column itself is built by
/// [`jyc_types::format_context_region_label`], shared with `/info`. The
/// indent is the layout's — the pane's value column in the table layout,
/// flush left in the stacked one — so the span carries none of its own.
pub(super) fn context_region_label_span(
    region: ContextRegion,
    label_width: usize,
) -> Span<'static> {
    Span::styled(
        jyc_types::format_context_region_label(region, label_width, 0),
        Style::default()
            .fg(context_region_color(region))
            .add_modifier(Modifier::BOLD),
    )
}

/// Append the context-composition bar to `spans`: one coloured run of
/// [`BAR_FULL`] cells per region, sized by its estimated token share — the
/// glyph every filled part of the pane's bars is drawn from, so a bar's
/// weight says whether a cell carries something and only the region colours
/// say which part of the context a cell is.
///
/// `bar_cells` is the pane's bar width — the same value the token row gets
/// (see [`bar_width`]), so the two bars start and end in the same columns.
/// Every region with a non-zero estimate gets at least one cell when the
/// bar is wide enough for them all; a narrower bar falls back to
/// proportional rounding.
///
/// Returns `false` — and pushes nothing — when the composition has no
/// estimated weight, or when `bar_cells` is below [`MIN_BAR_WIDTH`]: a bar
/// that narrow would misreport the share, so the legend figures stand
/// alone, the same rule the token bar follows.
///
/// The caller keeps the `Context:` label row either way — the legend rows
/// below it still need their header. `false` therefore reads as "no bar on
/// this row", not "no row at all".
pub(super) fn push_context_composition_bar_spans(
    spans: &mut Vec<Span>,
    t: &TopicInfo,
    bar_cells: usize,
) -> bool {
    let total = jyc_types::context_composition_total(&t.context_composition);
    if bar_cells < MIN_BAR_WIDTH || total == 0 {
        return false;
    }
    let widths = composition_widths(&t.context_composition, total, bar_cells);
    let mut drew = false;
    for (region, cells) in t.context_composition.iter().zip(widths) {
        if cells == 0 {
            continue;
        }
        spans.push(Span::styled(
            BAR_FULL.repeat(cells),
            Style::default().fg(context_region_color(region.region)),
        ));
        drew = true;
    }
    drew
}

/// Split `width` cells among the regions in proportion to their estimated
/// token weight.
///
/// With room for every weighted region, each starts with one cell (so a
/// small region cannot vanish) and the rest follows the largest-remainder
/// method. A bar narrower than the number of weighted regions instead
/// rounds cumulative boundaries, which still fills the width exactly.
fn composition_widths(regions: &[ContextRegionInfo], total: u64, width: usize) -> Vec<usize> {
    let mut widths = vec![0usize; regions.len()];
    if width == 0 || total == 0 {
        return widths;
    }
    let weighted: Vec<usize> = regions
        .iter()
        .enumerate()
        .filter_map(|(i, region)| (region.estimated_tokens > 0).then_some(i))
        .collect();
    if weighted.is_empty() {
        return widths;
    }
    if width < weighted.len() {
        let total = total as u128;
        let mut cumulative = 0u128;
        let mut previous = 0usize;
        for (i, region) in regions.iter().enumerate() {
            cumulative += region.estimated_tokens as u128;
            let end = ((cumulative * width as u128 + total / 2) / total) as usize;
            widths[i] = end.saturating_sub(previous);
            previous = end;
        }
        return widths;
    }

    for &i in &weighted {
        widths[i] = 1;
    }
    let remaining = width - weighted.len();
    let mut used = 0usize;
    let mut remainders: Vec<(u128, usize)> = Vec::with_capacity(weighted.len());
    for &i in &weighted {
        let scaled = regions[i].estimated_tokens as u128 * remaining as u128;
        let share = (scaled / total as u128) as usize;
        widths[i] += share;
        used += share;
        remainders.push((scaled % total as u128, i));
    }
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for &(_, i) in remainders.iter().take(remaining - used) {
        widths[i] += 1;
    }
    widths
}

/// The cells a bar keeps to the pane's right edge. The bar stops two
/// cells short of the edge, so it never touches the border and the pane
/// keeps a margin of its own.
const BAR_EDGE_ROOM: usize = 2;

/// The narrowest bar worth drawing. The fill rounds to the nearest cell, so
/// a bar of five cells is within ten points of the share; below that the
/// rounding error grows past it and the bar misreports the figure it stands
/// for — the percentage printed beside it says everything anyway, so a
/// narrow pane gets its figures alone.
///
/// Only a column of fewer than five cells can reach it: the table layout
/// leaves the label's room (at least thirteen cells) to the bar, and the
/// stacked one gives the bar the whole column.
const MIN_BAR_WIDTH: usize = 5;

/// How the info pane lays its rows out. Decided by the pane's inner
/// width alone (see [`layout_for`]), so the whole pane reads one way —
/// a row that switches layout on its own would break the alignment the
/// layout exists for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneLayout {
    /// Label and value share one row: the label is padded to
    /// [`LABEL_WIDTH`] and every value starts in the column after it, so
    /// the pane reads as one table.
    Table,
    /// Label and value split over two rows: the label takes a row of its
    /// own and the value follows flush left on the next one. A pane too
    /// narrow for a value column would otherwise cut every value short.
    Stacked,
}

/// The inner width a pane needs before its label and its value can share
/// one row: the label column ([`LABEL_WIDTH`]) plus a value column wide
/// enough for a model name ("minimax/Minimax-M3", 18 cells). A narrower
/// pane stacks the value under its label instead of cutting it.
const TABLE_MIN_INNER_WIDTH: u16 = LABEL_WIDTH as u16 + 18;

/// The layout the info pane of the given inner width uses: the table
/// layout while the pane can hold a label column and a value column, the
/// stacked one below that.
pub(super) fn layout_for(inner_width: u16) -> PaneLayout {
    if inner_width >= TABLE_MIN_INNER_WIDTH {
        PaneLayout::Table
    } else {
        PaneLayout::Stacked
    }
}

/// The width of the pane's value column: what the label leaves in the
/// table layout, the whole row in the stacked one. Every value of the
/// pane starts in that column, so a row that fills it fills the pane.
pub(super) fn value_width(inner_width: u16, layout: PaneLayout) -> usize {
    match layout {
        PaneLayout::Table => (inner_width as usize).saturating_sub(LABEL_WIDTH).max(1),
        PaneLayout::Stacked => inner_width as usize,
    }
}

/// The bar width for a pane of the given inner width, laid out as
/// `layout` says: as wide as the pane allows, stopping
/// [`BAR_EDGE_ROOM`] cells short of its right edge. A bar wider than the
/// room its row leaves would be cut in half by the paragraph's wrapping —
/// half a bar reads worse than a short one — and a bar narrower than
/// [`MIN_BAR_WIDTH`] would misreport the share, so both ends are refused.
/// Zero means the row carries its figures alone.
///
/// In the table layout the bar shares its row with the padded label, so
/// the room is what the label leaves; in the stacked layout the bar has
/// the row to itself.
pub(super) fn bar_width(inner_width: u16, layout: PaneLayout) -> usize {
    let room = value_width(inner_width, layout).saturating_sub(BAR_EDGE_ROOM);
    if room < MIN_BAR_WIDTH {
        return 0;
    }
    room
}

/// A pane label: bold, and padded on the right to [`LABEL_WIDTH`] so that
/// every value of the pane starts in the same column. The pane's rows
/// that are not `label: value` (the files list, the task list) keep
/// their own shape and are not padded.
pub(super) fn label_span(label: &str) -> Span<'static> {
    Span::styled(
        format!("{label:<width$}", width = LABEL_WIDTH),
        Style::default().add_modifier(Modifier::BOLD),
    )
}

/// Append a `width`-cell bar for `pct` (0..=100): [`BAR_FULL`] bold yellow
/// for the filled part, [`BAR_EMPTY`] dim gray for the rest. The two glyphs
/// are one line at two weights, so the bar reads as a single rule and the
/// share is told by both the weight and the colour — a rendering without
/// colour still shows how far the bar is filled. The halves stay separate
/// spans, so the colour changes exactly where the share does. The filled
/// part rounds to the nearest cell, so the bar claims at most half a cell
/// of share it does not have. A zero width draws no bar at all.
fn push_bar_spans(spans: &mut Vec<Span>, pct: u32, width: usize) {
    if width == 0 {
        return;
    }
    let filled = (((pct.min(100) as usize) * width) + 50) / 100;
    if filled > 0 {
        spans.push(Span::styled(
            BAR_FULL.repeat(filled),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if filled < width {
        spans.push(Span::styled(
            BAR_EMPTY.repeat(width - filled),
            Style::default().fg(Color::DarkGray),
        ));
    }
}

/// Compute the context-window percentage for a topic: the last LLM call's
/// input plus its output, against the model's context window. Returns
/// `None` when the input tokens or the window are missing, or the window is
/// zero. The figure itself comes from the shared helper, so `/info` prints
/// the same percentage.
pub(super) fn context_pct(t: &TopicInfo) -> Option<u32> {
    jyc_types::context_token_pct(
        t.context_input_tokens,
        t.context_output_tokens,
        t.max_tokens,
    )
}

/// Append the "Tokens: <bar>" row to `spans` — the context size (the
/// last call's input plus its output) against the context window, with
/// the same share drawn as a bar of `bar_cells` cells. The label is
/// padded to the pane's label width, so the bar starts in the column
/// every other value starts in.
///
/// Returns the row's figures — "44.17K / 117.19K (38%)" — for the
/// caller to lay out on the row under the bar, where they line up with
/// it. A pane too narrow for a bar keeps the figures in the row itself
/// and gets `None` back. Pushes nothing when `context_input_tokens` or
/// `max_tokens` is missing.
pub(super) fn push_tokens_span(
    spans: &mut Vec<Span>,
    t: &TopicInfo,
    bar_cells: usize,
) -> Option<Span<'static>> {
    if let (Some(cur), Some(max)) = (t.context_input_tokens, t.max_tokens) {
        let used = jyc_types::context_tokens(Some(cur), t.context_output_tokens).unwrap_or(cur);
        let pct = context_pct(t).unwrap_or(0);
        let figures = format!(
            "{} / {} ({pct}%)",
            jyc_types::format_token_count(used),
            jyc_types::format_token_count(max)
        );
        spans.push(label_span(TOKENS_LABEL));
        if bar_cells == 0 {
            spans.push(Span::raw(figures));
            return None;
        }
        push_bar_spans(spans, pct, bar_cells);
        Some(Span::raw(figures))
    } else {
        None
    }
}

/// Append the "Output: ..." row to `spans` — the session total and the
/// latest turn, from the shared formatter. Pushes nothing when the session
/// has produced no output at all.
pub(super) fn push_output_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(figures) = jyc_types::format_output_pair(t.output_tokens, t.turn_output_tokens) {
        spans.push(label_span("Output: "));
        spans.push(Span::raw(figures));
    }
}

/// Append the "Create: N" row to `spans`. Pushes nothing when
/// `total_cache_creation_tokens` is missing or zero. Only Anthropic and
/// GPT-5.6 (`cache_write_tokens`) report writes separately from reads; for
/// every other vendor this stays `None` and the row never renders. Sits next to
/// the cache rows in the chat info pane and dashboard topic info area so
/// users with cache-heavy Anthropic workflows can see the write volume
/// that the cache-creation premium rate applies to.
pub(super) fn push_cache_creation_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(cache_creation) = t.total_cache_creation_tokens
        && cache_creation > 0
    {
        spans.push(label_span(CACHE_CREATE_LABEL));
        spans.push(Span::raw(jyc_types::format_token_count(cache_creation)));
    }
}

/// Append a cache-rate row: the cache-hit tokens, the input tokens they
/// are a share of, and that share — with the share drawn as a bar of
/// `bar_cells` cells. The label is padded to the pane's label width, so
/// the bar starts in the column every other value starts in.
///
/// Returns the row's figures for the caller to lay out on the row under
/// the bar, where they line up with it; a row too narrow for a bar keeps
/// the figures in the row itself and gets `None` back.
///
/// `always` decides what a scope with no input tokens yet looks like.
/// `false` omits the row — there is nothing for the hits to be a share
/// of, and a zero would misreport it. `true` draws the row at zero, for
/// the surface where a row that stays put reads better than one that
/// comes and goes.
///
/// A scope that has input tokens but no recorded hits still shows its
/// row, at 0%: the hits are a share of the input tokens, and a provider
/// without prompt caching simply has no hits to report.
///
/// The figures come from the shared helper, so `/info` prints the same
/// string where it prints a row at all. At zero the two surfaces differ
/// on purpose: the pane synthesizes `0 / 0 (0%)`, and `/info` omits a
/// scope it has no figures for.
fn push_cache_rate_row(
    spans: &mut Vec<Span>,
    label: &str,
    cache_hits: Option<u64>,
    total_input: Option<u64>,
    bar_cells: usize,
    always: bool,
) -> Option<Span<'static>> {
    let (figures, pct) = match jyc_types::format_cache_rate(cache_hits, total_input) {
        Some(figures) => (
            figures,
            jyc_types::cache_util_pct(cache_hits, total_input).unwrap_or(0),
        ),
        None if always => {
            // The scope has no input tokens yet — the turn has made no
            // call. Draw it at zero: the row stays put across the turn
            // boundary instead of vanishing until the first call lands.
            let zero = jyc_types::format_token_count(0);
            (format!("{zero} / {zero} (0%)"), 0)
        }
        None => return None,
    };
    spans.push(label_span(label));
    if bar_cells == 0 {
        spans.push(Span::raw(figures));
        return None;
    }
    push_bar_spans(spans, pct, bar_cells);
    Some(Span::raw(figures))
}

/// Append the "Turn: <bar>" row — the cache figures of the latest turn,
/// so a long turn's own cache rate is visible next to the session's.
///
/// The row is drawn even before the turn's first call: the turn's
/// counters are zeroed at every turn boundary, and a row that vanishes
/// until the first call lands reads as missing data rather than as a
/// zero.
pub(super) fn push_turn_cache_span(
    spans: &mut Vec<Span>,
    t: &TopicInfo,
    bar_cells: usize,
) -> Option<Span<'static>> {
    push_cache_rate_row(
        spans,
        CACHE_TURN_LABEL,
        t.turn_cache_hit_tokens,
        t.turn_input_tokens,
        bar_cells,
        true,
    )
}

/// Append the "Session: <bar>" row — the cache-hit tokens of every LLM
/// call in the session and the input tokens they are a share of. Omitted
/// while the session has no input tokens: a session that has made no call
/// has no rate to report, and the overview would carry the zero row for
/// every topic.
pub(super) fn push_session_cache_span(
    spans: &mut Vec<Span>,
    t: &TopicInfo,
    bar_cells: usize,
) -> Option<Span<'static>> {
    push_cache_rate_row(
        spans,
        CACHE_SESSION_LABEL,
        t.total_cache_hit_tokens,
        t.total_input_tokens,
        bar_cells,
        false,
    )
}

/// Append the "Reasoning: N turn · M session" row to `spans`. Pushes
/// nothing when the provider reports no reasoning tokens — the hidden
/// chain-of-thought share of the output, already counted in the output
/// tokens.
///
/// The figures are spelled by the same helper the Output row uses, turn
/// first and session second, so the two rows read alike and `/info`
/// prints the same string.
pub(super) fn push_reasoning_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(pair) =
        jyc_types::format_output_pair(t.total_reasoning_tokens, t.turn_reasoning_tokens)
    {
        spans.push(label_span("Reasoning: "));
        spans.push(Span::raw(pair));
    }
}

/// Append the "Cost: $Y today · $X session" row to `spans`. Pushes
/// nothing when the topic has no cost data (model without configured
/// `pricing`), so an unpriced topic shows no row at all rather than a
/// misleading zero.
///
/// `session` resets with the agent session; `today` is the durable
/// per-day total from the billing ledger — they differ after a reset by
/// design. The row is spelled by `jyc_types::format_cost_pair`, which
/// `/info` shares, so the two surfaces cannot drift apart.
pub(super) fn push_cost_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(ref c) = t.cost {
        spans.push(label_span("Cost: "));
        spans.push(Span::raw(jyc_types::format_cost_pair(c)));
    }
}

/// Append the "Today (all): $X" row to `spans` — the day's spend across
/// every topic, per currency. Pushes nothing when the day's ledger holds
/// no entry, so the row never shows a misleading zero.
pub(super) fn push_today_all_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(totals) = jyc_types::format_currency_totals(&t.today_all) {
        spans.push(label_span(TODAY_ALL_LABEL));
        spans.push(Span::raw(totals));
    }
}

/// Append the "Latency: 1.1s/call (3) · 820ms/call (37)" row to `spans`.
/// Pushes nothing when neither pair is present — no call measured yet
/// (fresh session, or a session file written before the fields existed) —
/// so an unmeasured topic shows no row rather than `0ms`.
///
/// The left figure is the latest turn's average wall-clock time per LLM
/// call and how many calls it covers; the right is the session's. A turn
/// with no measured call yet (just started, or the session was reset)
/// collapses the row to the session figure. Both answer "how long one API
/// call takes", not how many tokens per second it produces. The `/info`
/// command renders the same words from the same four fields, both through
/// [`format_latency`].
pub(super) fn push_latency_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    let turn = t.avg_turn_call_ms.zip(t.turn_call_count);
    let session = t.avg_call_ms.zip(t.call_count);
    if let Some(latency) = format_latency(turn, session) {
        spans.push(label_span("Latency: "));
        spans.push(Span::raw(latency));
    }
}

/// The word that names the blocks on the status row. It says what the
/// blocks count — the queue's occupancy — so they cannot be read as the
/// progress of the turn the status word names.
const QUEUE_WORD: &str = "queue ";

/// The separator between the status word and the queue's part of the row.
const QUEUE_SEPARATOR: &str = " · ";

/// The block that stands for one message of the topic's queue.
///
/// A quarter short of a cell, so the blocks keep a gap between them and
/// the row reads as a row of blocks rather than a solid bar. The block
/// elements are one cell wide in every font — the wider box glyphs are
/// ambiguous, and a CJK font would stretch them.
const QUEUE_BLOCK: &str = "▊";

/// Append the status row to `spans` — the topic's live status and the
/// backlog its queue holds, on one row: `Processing · queue ▊▊▊ 2 / 10`.
///
/// The blocks say how much of the queue is taken (the queued ones yellow,
/// the free slots dark gray), the figures carry the exact counts, and
/// the word `queue` is what tells the reader that the blocks count the
/// queue and not the progress of the turn the status word names.
///
/// The row gives way as the pane narrows, in this order: the figures
/// move to the row under it, then the blocks go (and the word with them
/// — it names blocks that are no longer there), then the figures move
/// under the status word. Nothing is dropped: every figure is still on
/// the pane.
///
/// The backlog is what the model has not read yet: the messages still in
/// the queue, the ones the running turn pulled aside for a turn of their
/// own, and the ones the running turn has taken in and the model has not
/// read yet. It can exceed the capacity — the running turn holds
/// messages the queue gave up room for — and the blocks then fill the row
/// without covering it: the figures say what is really waiting.
///
/// Returns the figures when they belong on the row under the status
/// word, for the caller to lay out there; `None` when they share the
/// row. A topic whose source reports no capacity has no queue to draw:
/// the row is then the status word alone.
pub(super) fn push_status_span(
    spans: &mut Vec<Span>,
    t: &TopicInfo,
    value_cells: usize,
) -> Option<Span<'static>> {
    spans.push(label_span("Status: "));
    let status = format!("{}", t.status);
    let status_cells = status.width();
    spans.push(Span::raw(status));
    let capacity = t.queue_capacity;
    if capacity == 0 {
        return None;
    }
    let figures = format!("{} / {}", t.queued, capacity);
    // The row with the figures on it: status · queue <blocks> <figures>.
    let with_figures =
        status_cells + QUEUE_SEPARATOR.width() + QUEUE_WORD.width() + 1 + figures.width();
    let blocks = value_cells.saturating_sub(with_figures);
    if blocks >= MIN_BAR_WIDTH {
        spans.push(Span::raw(QUEUE_SEPARATOR));
        spans.push(Span::raw(QUEUE_WORD));
        push_queue_blocks(spans, t, blocks);
        spans.push(Span::raw(" "));
        spans.push(Span::raw(figures));
        return None;
    }
    // The row without them: status · queue <blocks>, the figures on the
    // row under it. The blocks keep two cells to the pane's edge, so they
    // never touch the border.
    let blocks = value_cells.saturating_sub(
        status_cells + QUEUE_SEPARATOR.width() + QUEUE_WORD.width() + BAR_EDGE_ROOM,
    );
    if blocks >= MIN_BAR_WIDTH {
        spans.push(Span::raw(QUEUE_SEPARATOR));
        spans.push(Span::raw(QUEUE_WORD));
        push_queue_blocks(spans, t, blocks);
        return Some(Span::raw(figures));
    }
    // No blocks: the word names blocks that are no longer there, so it
    // goes with them.
    if value_cells >= status_cells + QUEUE_SEPARATOR.width() + figures.width() {
        spans.push(Span::raw(QUEUE_SEPARATOR));
        spans.push(Span::raw(figures));
        return None;
    }
    Some(Span::raw(figures))
}

/// Append the queue's blocks to `spans` — one block per message the
/// queue can hold, the queued ones yellow and the free slots dark gray,
/// in at most `room` cells of room.
///
/// A capacity wider than the room is drawn as a share of it: the blocks
/// would be cut in half by the wrapping, so the row shows how much of the
/// queue is taken, rounded to the nearest cell. The figures beside the
/// blocks carry the exact counts.
fn push_queue_blocks(spans: &mut Vec<Span>, t: &TopicInfo, room: usize) {
    let capacity = t.queue_capacity;
    let (cells, filled) = if capacity <= room {
        (capacity, t.queued.min(capacity))
    } else {
        let filled = ((t.queued.min(capacity) * room) + capacity / 2) / capacity;
        (room, filled.min(room))
    };
    if filled > 0 {
        spans.push(Span::styled(
            QUEUE_BLOCK.repeat(filled),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if filled < cells {
        spans.push(Span::styled(
            QUEUE_BLOCK.repeat(cells - filled),
            Style::default().fg(Color::DarkGray),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One context-composition row for the width tests.
    fn region(
        region: ContextRegion,
        messages: u64,
        bytes: u64,
        estimated_tokens: u64,
    ) -> ContextRegionInfo {
        ContextRegionInfo {
            region,
            messages,
            bytes,
            estimated_tokens,
        }
    }

    /// A pane label as the pane draws it: padded to the label width, so
    /// that every value of the pane starts in the same column.
    fn padded(label: &str) -> String {
        format!("{label:<width$}", width = LABEL_WIDTH)
    }

    /// A bar of twenty cells — the width the tests that draw a bar use.
    /// The bar's width follows the pane; these tests pin what a bar of a
    /// given width looks like, not the width the pane picks.
    const BAR: usize = 20;

    /// A topic's detail, with the token figures the helpers render.
    fn info_with(
        input: Option<u64>,
        max: Option<u64>,
        output: Option<u64>,
        total_input: Option<u64>,
        cache_hit: Option<u64>,
        cache_creation: Option<u64>,
    ) -> TopicInfo {
        info_with_context_output(
            input,
            None,
            max,
            output,
            total_input,
            cache_hit,
            cache_creation,
        )
    }

    /// The same, with the last call's own output tokens — the reply that
    /// is appended to the context.
    fn info_with_context_output(
        input: Option<u64>,
        context_output: Option<u64>,
        max: Option<u64>,
        output: Option<u64>,
        total_input: Option<u64>,
        cache_hit: Option<u64>,
        cache_creation: Option<u64>,
    ) -> TopicInfo {
        TopicInfo {
            name: "t".into(),
            channel: "c".into(),
            pattern: None,
            status: jyc_types::TopicStatus::Idle,
            queued: 0,
            queue_capacity: 0,
            parent: None,
            model: None,
            mode: None,
            context_input_tokens: input,
            context_output_tokens: context_output,
            max_tokens: max,
            context_composition: Vec::new(),
            output_tokens: output,
            turn_output_tokens: None,
            total_input_tokens: total_input,
            total_cache_hit_tokens: cache_hit,
            total_cache_creation_tokens: cache_creation,
            total_reasoning_tokens: None,
            turn_reasoning_tokens: None,
            turn_cache_hit_tokens: None,
            turn_input_tokens: None,
            today_all: Vec::new(),
            avg_call_ms: None,
            call_count: None,
            avg_turn_call_ms: None,
            turn_call_count: None,
            activity: vec![],
            last_active_at: None,
            skills: vec![],
            available_skills: vec![],
            recent_messages: vec![],
            thinking_text: None,
            topic_path: None,
            branch: None,
            changed_files: None,
            tasks: Default::default(),
            cost: None,
            commands: vec![],
            subagents: None,
        }
    }

    #[test]
    fn context_pct_basic() {
        let t = info_with(Some(5000), Some(10000), None, None, None, None);
        assert_eq!(context_pct(&t), Some(50));
    }

    /// The context size counts the last call's output too: the reply is
    /// part of the context the next call carries.
    #[test]
    fn context_pct_counts_the_last_call_output() {
        let t =
            info_with_context_output(Some(5000), Some(1000), Some(10000), None, None, None, None);
        assert_eq!(context_pct(&t), Some(60));
    }

    #[test]
    fn context_pct_zero_max_returns_none() {
        let t = info_with(Some(100), Some(0), None, None, None, None);
        assert_eq!(context_pct(&t), None);
    }

    #[test]
    fn context_pct_missing_input_returns_none() {
        let t = info_with(None, Some(10000), None, None, None, None);
        assert_eq!(context_pct(&t), None);
    }

    #[test]
    fn push_tokens_span_omits_when_max_missing() {
        let t = info_with(Some(100), None, None, None, None, None);
        let mut spans = Vec::new();
        let figures = push_tokens_span(&mut spans, &t, BAR);
        assert!(spans.is_empty());
        assert!(figures.is_none());
    }

    /// The row carries the label and the bar; the figures come back to the
    /// caller, which lays them out on the row under the bar.
    #[test]
    fn push_tokens_span_writes_label_bar_and_returns_the_figures() {
        let t = info_with(Some(4750), Some(10000), None, None, None, None);
        let mut spans = Vec::new();
        let figures = push_tokens_span(&mut spans, &t, BAR);
        assert_eq!(spans.len(), 3);
        // The label is padded to the pane's label width, so the bar
        // starts in the column every other value starts in.
        assert_eq!(spans[0].content, padded(TOKENS_LABEL));
        assert_eq!(spans[1].content, "━".repeat(9));
        assert_eq!(spans[2].content, "─".repeat(11));
        // The figures have a row of their own, under the bar.
        assert_eq!(figures.unwrap().content, "4.64K / 9.77K (47%)");
    }

    /// A pane too narrow for an honest bar gets the figures alone — no
    /// bar, and none of the spans a bar would have added. The label is
    /// still padded: the pane's values share a column whether or not a
    /// bar stands in front of them. The figures stay in the row, and
    /// there is no second row to hand them to.
    #[test]
    fn push_tokens_span_without_a_bar() {
        let t = info_with(Some(4750), Some(10000), None, None, None, None);
        let mut spans = Vec::new();
        let figures = push_tokens_span(&mut spans, &t, 0);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, padded(TOKENS_LABEL));
        assert_eq!(spans[1].content, "4.64K / 9.77K (47%)");
        assert!(figures.is_none());
    }

    /// The status row carries the queue's backlog: the blocks say how much
    /// of the queue is taken, the figures carry the exact counts, and the
    /// word `queue` says what the blocks count — so they cannot be read as
    /// the progress of the turn the status word names.
    #[test]
    fn the_status_row_carries_the_queue_as_blocks() {
        let mut t = info_with(None, None, None, None, None, None);
        t.queued = 3;
        t.queue_capacity = 10;
        let mut spans = Vec::new();
        // A value column of twenty-five cells: the status word, the
        // separator, the word, the figures and five cells of blocks.
        let figures = push_status_span(&mut spans, &t, 25);
        assert_eq!(spans.len(), 8, "{spans:?}");
        assert_eq!(spans[0].content, padded("Status: "));
        assert_eq!(spans[1].content, "Idle");
        assert_eq!(spans[2].content, QUEUE_SEPARATOR);
        assert_eq!(spans[3].content, QUEUE_WORD);
        // Three of ten messages waiting in five cells is a tenth and a
        // half of the row, which rounds to two cells.
        assert_eq!(spans[4].content, QUEUE_BLOCK.repeat(2));
        assert_eq!(spans[4].style.fg, Some(Color::Yellow));
        assert_eq!(spans[5].content, QUEUE_BLOCK.repeat(3));
        assert_eq!(spans[5].style.fg, Some(Color::DarkGray));
        assert_eq!(spans[6].content, " ");
        assert_eq!(spans[7].content, "3 / 10");
        assert!(figures.is_none(), "the figures share the row");
    }

    /// A row too narrow for the figures beside the blocks gives them the
    /// row under it — nothing is dropped, and the blocks keep their word.
    #[test]
    fn the_status_row_gives_the_figures_the_row_under_it() {
        let mut t = info_with(None, None, None, None, None, None);
        t.queued = 3;
        t.queue_capacity = 10;
        let mut spans = Vec::new();
        // One cell short of the figures fitting beside the blocks: the
        // blocks take the room the figures would have had.
        let figures = push_status_span(&mut spans, &t, 24);
        assert_eq!(spans.len(), 6, "{spans:?}");
        assert_eq!(spans[4].content, QUEUE_BLOCK.repeat(3));
        assert_eq!(spans[4].style.fg, Some(Color::Yellow));
        assert_eq!(spans[5].content, QUEUE_BLOCK.repeat(6));
        assert_eq!(spans[5].style.fg, Some(Color::DarkGray));
        assert_eq!(figures.unwrap().content, "3 / 10");
    }

    /// A row too narrow for honest blocks drops them — and the word that
    /// named them — and keeps the figures.
    #[test]
    fn the_status_row_drops_the_blocks_when_the_row_cannot_carry_them() {
        let mut t = info_with(None, None, None, None, None, None);
        t.queued = 3;
        t.queue_capacity = 10;
        let mut spans = Vec::new();
        // Nineteen cells: too narrow for five blocks beside the word,
        // wide enough for the figures.
        let figures = push_status_span(&mut spans, &t, 19);
        assert_eq!(spans.len(), 4, "{spans:?}");
        assert_eq!(spans[0].content, padded("Status: "));
        assert_eq!(spans[1].content, "Idle");
        assert_eq!(spans[2].content, QUEUE_SEPARATOR);
        assert_eq!(spans[3].content, "3 / 10");
        assert!(figures.is_none(), "the figures stay in the row");
    }

    /// A row too narrow for the figures as well puts them under the status
    /// word: the count still reads.
    #[test]
    fn the_status_row_puts_the_figures_under_a_narrow_status() {
        let mut t = info_with(None, None, None, None, None, None);
        t.queued = 3;
        t.queue_capacity = 10;
        let mut spans = Vec::new();
        // Ten cells: the status word alone, the figures under it.
        let figures = push_status_span(&mut spans, &t, 10);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(spans[1].content, "Idle");
        assert_eq!(figures.unwrap().content, "3 / 10");
    }

    /// A queue that holds more than its capacity fills the row without
    /// covering it: the blocks stop at the row's width, and the figures
    /// carry the real count.
    #[test]
    fn a_queue_over_its_capacity_fills_the_row_and_no_more() {
        let mut t = info_with(None, None, None, None, None, None);
        t.queued = 30;
        t.queue_capacity = 10;
        let mut spans = Vec::new();
        // Twenty-six cells: the status word, the separator, the word, a
        // space and the figures — `30 / 10` is seven cells, one more than
        // a single-digit count — leave five for the blocks.
        let figures = push_status_span(&mut spans, &t, 26);
        assert_eq!(spans.len(), 7, "{spans:?}");
        assert_eq!(spans[4].content, QUEUE_BLOCK.repeat(5));
        assert_eq!(spans[4].style.fg, Some(Color::Yellow));
        assert_eq!(spans[6].content, "30 / 10");
        assert!(figures.is_none());
    }

    /// An empty queue of a wide one draws the row in free slots only.
    #[test]
    fn an_empty_queue_wider_than_the_row_draws_free_slots_only() {
        let mut t = info_with(None, None, None, None, None, None);
        t.queued = 0;
        t.queue_capacity = 10;
        let mut spans = Vec::new();
        let figures = push_status_span(&mut spans, &t, 25);
        assert_eq!(spans.len(), 7, "{spans:?}");
        assert_eq!(spans[4].content, QUEUE_BLOCK.repeat(5));
        assert_eq!(spans[4].style.fg, Some(Color::DarkGray));
        assert_eq!(spans[6].content, "0 / 10");
        assert!(figures.is_none());
    }

    /// A topic whose source reports no capacity has no queue to draw: the
    /// row is the status word alone.
    #[test]
    fn the_status_row_without_a_capacity_is_the_status_alone() {
        let mut t = info_with(None, None, None, None, None, None);
        t.queued = 0;
        t.queue_capacity = 0;
        let mut spans = Vec::new();
        let figures = push_status_span(&mut spans, &t, 25);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(spans[0].content, padded("Status: "));
        assert_eq!(spans[1].content, "Idle");
        assert!(figures.is_none());
    }

    /// The two cache rows share one bar width and one label width, so
    /// their bars start in the same column and their shares can be compared
    /// by eye.
    #[test]
    fn the_two_bars_start_in_the_same_column() {
        let t = info_with(Some(4750), Some(10000), None, Some(1000), Some(880), None);
        let mut tokens = Vec::new();
        push_tokens_span(&mut tokens, &t, BAR);
        let mut cache = Vec::new();
        push_session_cache_span(&mut cache, &t, BAR);
        // The labels are the same width, and the bars follow them directly.
        assert_eq!(tokens[0].content.len(), cache[0].content.len());
        assert_eq!(tokens[0].content.len(), LABEL_WIDTH);
        assert_eq!(cache[0].content, padded(CACHE_SESSION_LABEL));
    }

    /// Every label of the pane is padded to the same width, so every
    /// value starts in the same column — the pane reads as one table
    /// rather than a ragged list.
    #[test]
    fn every_label_is_padded_to_the_same_width() {
        for label in [
            "Topic: ",
            "Pattern: ",
            "Model: ",
            "Mode: ",
            "Branch: ",
            "Status: ",
            TOKENS_LABEL,
            "Output: ",
            CACHE_TURN_LABEL,
            CACHE_SESSION_LABEL,
            CACHE_CREATE_LABEL,
            "Reasoning: ",
            "Cost: ",
            TODAY_ALL_LABEL,
            "Latency: ",
        ] {
            let span = label_span(label);
            assert_eq!(span.content, padded(label), "label {label:?}");
            assert_eq!(span.content.len(), LABEL_WIDTH, "label {label:?}");
            assert!(
                span.style.add_modifier.contains(Modifier::BOLD),
                "the label {label:?} must be bold"
            );
        }
    }

    /// The bar is as wide as the pane allows: a wide pane gets the full
    /// bar, a narrow one a shorter bar, and a pane too narrow for an
    /// honest bar gets none.
    #[test]
    fn bar_width_follows_the_pane() {
        // The table layout: the longest label ("Today (all): ", 13 cells)
        // plus the two cells the bar keeps to the pane's edge.
        assert_eq!(bar_width(38, PaneLayout::Table), 23);
        assert_eq!(bar_width(33, PaneLayout::Table), 18);
        assert_eq!(bar_width(32, PaneLayout::Table), 17);
        assert_eq!(bar_width(20, PaneLayout::Table), MIN_BAR_WIDTH);
        // Below that a cell would carry too much of the share to be honest.
        assert_eq!(bar_width(19, PaneLayout::Table), 0);
        assert_eq!(bar_width(14, PaneLayout::Table), 0);
        assert_eq!(bar_width(0, PaneLayout::Table), 0);
    }

    /// The stacked layout gives the bar a row of its own, so it is never
    /// shortened by a label beside it: it takes the pane's whole width,
    /// less the two cells it keeps to the pane's edge.
    #[test]
    fn bar_width_in_the_stacked_layout_uses_the_whole_row() {
        assert_eq!(bar_width(39, PaneLayout::Stacked), 37);
        assert_eq!(bar_width(23, PaneLayout::Stacked), 21);
        // The pane's whole width less the edge's cells: the label takes
        // no cell of it, where the table layout's bar gets what the
        // label's room leaves.
        assert_eq!(bar_width(21, PaneLayout::Stacked), 19);
        assert_eq!(bar_width(20, PaneLayout::Stacked), 18);
        assert_eq!(bar_width(23, PaneLayout::Table), 8);
        // The narrowest bar still worth drawing.
        assert_eq!(bar_width(7, PaneLayout::Stacked), MIN_BAR_WIDTH);
        // Below that a cell would carry too much of the share to be honest.
        assert_eq!(bar_width(6, PaneLayout::Stacked), 0);
        assert_eq!(bar_width(0, PaneLayout::Stacked), 0);
    }

    /// The layout switches at one width, and the whole pane switches with
    /// it: a pane that can hold a label column and a value column lays
    /// its rows out as a table, a narrower one stacks them.
    #[test]
    fn the_layout_switches_at_the_table_threshold() {
        assert_eq!(layout_for(TABLE_MIN_INNER_WIDTH), PaneLayout::Table);
        assert_eq!(layout_for(TABLE_MIN_INNER_WIDTH - 1), PaneLayout::Stacked);
        assert_eq!(layout_for(31), PaneLayout::Table);
        assert_eq!(layout_for(30), PaneLayout::Stacked);
        // A pane of any width at all still gets a layout.
        assert_eq!(layout_for(0), PaneLayout::Stacked);
    }

    /// The bar rounds to the nearest cell, and its filled part is the heavier
    /// glyph in bold yellow while the rest keeps the light one in dim gray. A
    /// share too small to fill a cell draws no filled part at all: the bar
    /// never claims more than the share it stands for.
    #[test]
    fn push_bar_spans_fills_and_pads() {
        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 0, BAR);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "─".repeat(BAR));

        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 5, BAR);
        assert_eq!(spans[0].content, "━");
        assert_eq!(spans[1].content, "─".repeat(BAR - 1));
        assert_eq!(spans[0].style.fg, Some(Color::Yellow));
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[1].style.fg, Some(Color::DarkGray));

        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 50, BAR);
        assert_eq!(spans[0].content, "━".repeat(BAR / 2));
        assert_eq!(spans[1].content, "─".repeat(BAR / 2));

        // A full bar has no padding left to draw.
        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 100, BAR);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "━".repeat(BAR));

        // Above 100% fills the bar without overflowing it.
        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 140, BAR);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "━".repeat(BAR));

        // A share too small for a cell draws no filled part: the bar stays
        // empty rather than overstating the share.
        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 2, BAR);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "─".repeat(BAR));

        // A zero width draws no bar at all.
        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 50, 0);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_output_span_omits_when_missing() {
        let t = info_with(Some(100), Some(10000), None, None, None, None);
        let mut spans = Vec::new();
        push_output_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_output_span_writes_label_and_value() {
        let mut t = info_with(Some(100), Some(10000), Some(420), None, None, None);
        t.turn_output_tokens = Some(128);
        let mut spans = Vec::new();
        push_output_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, padded("Output: "));
        assert_eq!(spans[1].content, "128 turn · 420 session");
    }

    /// A turn that has not called yet still gets its zero — the row reads
    /// as the session total plus a genuine zero, like the cache rows.
    #[test]
    fn push_output_span_counts_a_turn_without_calls_as_zero() {
        let t = info_with(Some(100), Some(10000), Some(420), None, None, None);
        let mut spans = Vec::new();
        push_output_span(&mut spans, &t);
        assert_eq!(spans[1].content, "0 turn · 420 session");
    }

    #[test]
    fn push_reasoning_span_omits_when_missing() {
        let t = info_with(Some(100), Some(10000), None, None, None, None);
        let mut spans = Vec::new();
        push_reasoning_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    /// The figures are turn first, session second -- the same order the
    /// Output row prints, from the same formatter.
    #[test]
    fn push_reasoning_span_writes_turn_then_session() {
        let mut t = info_with(Some(100), Some(10000), Some(420), None, None, None);
        t.total_reasoning_tokens = Some(4096);
        t.turn_reasoning_tokens = Some(512);
        let mut spans = Vec::new();
        push_reasoning_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, padded("Reasoning: "));
        assert_eq!(spans[1].content, "512 turn · 4.00K session");
    }

    /// A turn that has not called yet still gets its zero, and a topic
    /// whose provider reports no reasoning stays rowless.
    #[test]
    fn push_reasoning_span_counts_a_turn_without_calls_as_zero() {
        let mut t = info_with(Some(100), Some(10000), Some(420), None, None, None);
        t.total_reasoning_tokens = Some(4096);
        let mut spans = Vec::new();
        push_reasoning_span(&mut spans, &t);
        assert_eq!(spans[1].content, "0 turn · 4.00K session");

        let mut t = info_with(Some(100), Some(10000), Some(420), None, None, None);
        t.turn_reasoning_tokens = Some(512);
        let mut spans = Vec::new();
        push_reasoning_span(&mut spans, &t);
        assert!(spans.is_empty(), "no session total, no row");
    }

    /// The session's cache row: the cache-hit tokens, the input tokens
    /// they are a share of, and that share — with the share as a bar and
    /// the figures handed back for the row under it.
    #[test]
    fn push_session_cache_span_writes_label_bar_and_returns_the_figures() {
        let t = info_with(None, None, None, Some(720), Some(640), None);
        let mut spans = Vec::new();
        let figures = push_session_cache_span(&mut spans, &t, BAR);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, padded(CACHE_SESSION_LABEL));
        assert_eq!(spans[1].content, "━".repeat(18));
        assert_eq!(spans[2].content, "─".repeat(2));
        assert_eq!(figures.unwrap().content, "640 / 720 (88%)");
    }

    /// A session without input tokens has nothing to be a share of: no
    /// row, rather than a misleading 0%.
    #[test]
    fn push_session_cache_span_omits_when_missing() {
        let t = info_with(None, None, None, None, None, None);
        let mut spans = Vec::new();
        let figures = push_session_cache_span(&mut spans, &t, BAR);
        assert!(spans.is_empty());
        assert!(figures.is_none());
    }

    /// The turn's cache row carries the turn's own figures, so a long
    /// turn's rate stands next to the session's.
    #[test]
    fn push_turn_cache_span_writes_the_turns_figures() {
        let mut t = info_with(None, None, None, Some(720), Some(640), None);
        t.turn_cache_hit_tokens = Some(300);
        t.turn_input_tokens = Some(1000);
        let mut spans = Vec::new();
        let figures = push_turn_cache_span(&mut spans, &t, BAR);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, padded(CACHE_TURN_LABEL));
        assert_eq!(figures.unwrap().content, "300 / 1000 (30%)");
    }

    /// A turn that has made no call yet still gets its row, at zero: the
    /// turn's counters are zeroed at every turn start, and a row that
    /// vanishes until the first call lands reads as missing data.
    #[test]
    fn push_turn_cache_span_shows_zero_before_the_first_call() {
        let t = info_with(None, None, None, Some(720), Some(640), None);
        let mut spans = Vec::new();
        let figures = push_turn_cache_span(&mut spans, &t, BAR);
        assert_eq!(spans.len(), 2, "the row is drawn at zero: {spans:?}");
        assert_eq!(spans[0].content, padded(CACHE_TURN_LABEL));
        assert_eq!(spans[1].content, "─".repeat(BAR));
        assert_eq!(figures.unwrap().content, "0 / 0 (0%)");
    }

    /// A turn with input tokens but no cache hits shows its row at 0%:
    /// the hits are a share of the input, and a provider without prompt
    /// caching has none to report. Pinned here so the two kinds of zero
    /// — no call yet, and no hits — stay distinguishable.
    #[test]
    fn push_turn_cache_span_shows_zero_when_only_the_hits_are_missing() {
        let mut t = info_with(None, None, None, Some(720), Some(640), None);
        t.turn_cache_hit_tokens = None;
        t.turn_input_tokens = Some(1000);
        let mut spans = Vec::new();
        let figures = push_turn_cache_span(&mut spans, &t, BAR);
        // Two spans: the label and the bar (at 0% there is no filled part,
        // so the whole bar keeps the light glyph). The figures have a row of
        // their own.
        assert_eq!(spans.len(), 2, "the row is drawn at zero: {spans:?}");
        assert_eq!(spans[0].content, padded(CACHE_TURN_LABEL));
        assert_eq!(spans[1].content, "─".repeat(BAR));
        assert_eq!(figures.unwrap().content, "0 / 1000 (0%)");
    }

    /// No cost data (unpriced model) → no row, rather than `$0.00`.
    #[test]
    fn push_cost_span_omits_when_no_cost() {
        let t = info_with(Some(100), Some(10000), Some(50), Some(720), Some(640), None);
        let mut spans = Vec::new();
        push_cost_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    /// Both figures render, the day first, and USD uses the `$` symbol.
    #[test]
    fn push_cost_span_writes_today_and_session() {
        let mut t = info_with(Some(100), Some(10000), Some(50), Some(720), Some(640), None);
        t.cost = Some(jyc_types::TopicCost {
            session: 0.0521,
            today: 1.3057,
            currency: "USD".into(),
        });
        let mut spans = Vec::new();
        push_cost_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, padded("Cost: "));
        assert_eq!(spans[1].content, "$1.31 today · $0.05 session");
    }

    /// CNY renders with `¥`, not a dollar sign.
    #[test]
    fn push_cost_span_uses_cny_symbol() {
        let mut t = info_with(None, None, None, None, None, None);
        t.cost = Some(jyc_types::TopicCost {
            session: 2.5,
            today: 10.0,
            currency: "CNY".into(),
        });
        let mut spans = Vec::new();
        push_cost_span(&mut spans, &t);
        assert_eq!(spans[1].content, "¥10.00 today · ¥2.50 session");
    }

    /// An unrecognised currency is suffixed rather than given the wrong
    /// symbol — including the "mixed" marker for multi-currency days.
    #[test]
    fn push_cost_span_suffixes_unknown_currency() {
        let mut t = info_with(None, None, None, None, None, None);
        t.cost = Some(jyc_types::TopicCost {
            session: 1.0,
            today: 2.0,
            currency: "mixed".into(),
        });
        let mut spans = Vec::new();
        push_cost_span(&mut spans, &t);
        assert_eq!(spans[1].content, "2.00 mixed today · 1.00 mixed session");
    }

    /// The day's spend across every topic, per currency: one figure for a
    /// single-currency day, one per currency when the day spans two.
    #[test]
    fn push_today_all_span_writes_the_days_totals() {
        let mut t = info_with(None, None, None, None, None, None);
        t.today_all = vec![("USD".to_string(), 12.34)];
        let mut spans = Vec::new();
        push_today_all_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, padded("Today (all): "));
        assert_eq!(spans[1].content, "$12.34");

        t.today_all = vec![("USD".to_string(), 12.34), ("CNY".to_string(), 3.2)];
        let mut spans = Vec::new();
        push_today_all_span(&mut spans, &t);
        assert_eq!(spans[1].content, "$12.34 · ¥3.20");
    }

    /// A day with no ledger entry shows no row, rather than a zero.
    #[test]
    fn push_today_all_span_omits_when_the_day_is_empty() {
        let t = info_with(None, None, None, None, None, None);
        let mut spans = Vec::new();
        push_today_all_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    /// `push_cache_creation_span` emits nothing when the field is
    /// absent — non-Anthropic providers (where `total_cache_creation_tokens`
    /// is always `None`) see no row, so the dashboard stays clean.
    #[test]
    fn push_cache_creation_span_omits_when_missing() {
        let t = info_with(Some(100), Some(10000), Some(50), Some(720), Some(640), None);
        let mut spans = Vec::new();
        push_cache_creation_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    /// Anthropic sessions render the cache-create row alongside the
    /// cache rows. The label matches Anthropic's wire field
    /// (`cache_creation_input_tokens`) so users can map what they see
    /// back to the API.
    #[test]
    fn push_cache_creation_span_writes_label_and_value() {
        let t = info_with(
            Some(100),
            Some(10000),
            Some(50),
            Some(720),
            Some(640),
            Some(1280),
        );
        let mut spans = Vec::new();
        push_cache_creation_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, padded(CACHE_CREATE_LABEL));
        assert_eq!(spans[1].content, "1.25K");
    }

    /// No measured call → no row at all, rather than a misleading `0ms`.
    #[test]
    fn push_latency_span_omits_when_missing() {
        let t = info_with(None, None, None, None, None, None);
        let mut spans = Vec::new();
        push_latency_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    /// A sub-second average keeps its milliseconds, and the number of
    /// calls behind it travels along — an average over one call must not
    /// read as a settled figure.
    #[test]
    fn push_latency_span_writes_millis_and_call_count() {
        let mut t = info_with(None, None, None, None, None, None);
        t.avg_call_ms = Some(820);
        t.call_count = Some(37);
        let mut spans = Vec::new();
        push_latency_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, padded("Latency: "));
        assert_eq!(spans[1].content, "820ms/call (37)");
    }

    /// Both scopes present: the latest turn leads, the session follows.
    #[test]
    fn push_latency_span_puts_latest_turn_first() {
        let mut t = info_with(None, None, None, None, None, None);
        t.avg_call_ms = Some(820);
        t.call_count = Some(37);
        t.avg_turn_call_ms = Some(1100);
        t.turn_call_count = Some(3);
        let mut spans = Vec::new();
        push_latency_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].content, "1.10s/call (3) · 820ms/call (37)");
    }

    /// A turn with no measured call yet leaves the session figure alone.
    #[test]
    fn push_latency_span_falls_back_to_session_only() {
        let mut t = info_with(None, None, None, None, None, None);
        t.avg_call_ms = Some(820);
        t.call_count = Some(37);
        t.avg_turn_call_ms = Some(1100);
        let mut spans = Vec::new();
        push_latency_span(&mut spans, &t);
        assert_eq!(spans[1].content, "820ms/call (37)");
    }

    /// A slow average switches to seconds; an average without its count
    /// (impossible from `list_topics`, which fills both or neither) is
    /// dropped rather than rendered as "avg of 0".
    #[test]
    fn push_latency_span_writes_seconds_and_needs_the_count() {
        let mut t = info_with(None, None, None, None, None, None);
        t.avg_call_ms = Some(4230);
        t.call_count = Some(2);
        let mut spans = Vec::new();
        push_latency_span(&mut spans, &t);
        assert_eq!(spans[1].content, "4.23s/call (2)");

        let mut half = info_with(None, None, None, None, None, None);
        half.avg_call_ms = Some(4230);
        let mut spans = Vec::new();
        push_latency_span(&mut spans, &half);
        assert!(spans.is_empty());
    }

    /// A composition bar too narrow to carry a share is refused, the same
    /// rule the token bar follows; the legend figures stand alone.
    #[test]
    fn composition_bar_needs_the_minimum_width() {
        let mut t = info_with(None, None, None, None, None, None);
        t.context_composition = vec![
            region(ContextRegion::History, 1, 10, 1),
            region(ContextRegion::Current, 1, 10, 1),
        ];
        let mut spans = Vec::new();
        assert!(!push_context_composition_bar_spans(
            &mut spans,
            &t,
            MIN_BAR_WIDTH - 1
        ));
        assert!(spans.is_empty());

        assert!(push_context_composition_bar_spans(
            &mut spans,
            &t,
            MIN_BAR_WIDTH
        ));
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>(),
            MIN_BAR_WIDTH
        );
    }

    /// The composition bar fills the width exactly, and every region with
    /// weight keeps a cell even when its share would round it away.
    #[test]
    fn composition_widths_fill_the_bar_and_keep_small_regions_visible() {
        let regions = [
            region(ContextRegion::History, 31, 100, 100),
            region(ContextRegion::Checkpoint, 17, 50, 1),
            region(ContextRegion::Current, 9, 50, 50),
        ];
        let widths = composition_widths(&regions, 151, 20);
        assert_eq!(widths.iter().sum::<usize>(), 20);
        assert!(widths.iter().all(|cells| *cells >= 1));
        assert!(widths[0] > widths[2] && widths[2] > widths[1]);
    }

    /// A bar narrower than the number of regions still fills the width;
    /// a region with no estimated weight stays out of it.
    #[test]
    fn composition_widths_fit_a_narrow_bar_and_skip_zero_weight() {
        let regions = [
            region(ContextRegion::History, 1, 10, 0),
            region(ContextRegion::Checkpoint, 1, 10, 50),
            region(ContextRegion::Current, 1, 10, 50),
        ];
        let widths = composition_widths(&regions, 100, 1);
        assert_eq!(widths.iter().sum::<usize>(), 1);
        assert_eq!(widths[0], 0);
    }
}
