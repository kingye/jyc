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
use jyc_types::{TopicInfo, format_amount};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// The label of the token-usage row.
const TOKENS_LABEL: &str = "Tokens: ";

/// The label of the cache-hit-rate row — the longest label of the pane,
/// so it sets the width every other label is padded to.
const CACHE_HIT_RATE_LABEL: &str = "Cache hit rate: ";

/// The width every label of the info pane is padded to, so that every
/// value in the pane starts in the same column and the pane reads as one
/// table instead of a ragged list. The longest label sets it: a longer
/// one would leave less room for the values than [`BAR_LABEL_ROOM`]
/// allows, and the wrapping would cut them.
const LABEL_WIDTH: usize = CACHE_HIT_RATE_LABEL.len();

/// The room a bar leaves for its row: the padded label, plus one cell so
/// the bar never touches the pane's edge. The space between the bar and
/// the figures needs no room here — it sits after the bar, and the figures
/// wrap onto the next line when they do not fit.
const BAR_LABEL_ROOM: usize = LABEL_WIDTH + 1;

/// The widest bar the info pane draws, in cells.
const MAX_BAR_WIDTH: usize = 20;

/// The narrowest bar worth drawing. The fill rounds to the nearest cell, so
/// a bar of five cells is within ten points of the share; below that the
/// rounding error grows past it and the bar misreports the figure it stands
/// for — the percentage printed beside it says everything anyway, so a
/// narrow pane gets its figures alone.
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

/// The bar width for a pane of the given inner width, laid out as
/// `layout` says: as wide as the pane allows, up to [`MAX_BAR_WIDTH`]. A
/// bar wider than the room its row leaves would be cut in half by the
/// paragraph's wrapping — half a bar reads worse than a short one — and a
/// bar narrower than [`MIN_BAR_WIDTH`] would misreport the share, so both
/// ends are refused. Zero means the row carries its figures alone.
///
/// In the table layout the bar shares its row with the padded label, so
/// the room is what the label leaves; in the stacked layout the bar has
/// the row to itself and can use the pane's full width.
pub(super) fn bar_width(inner_width: u16, layout: PaneLayout) -> usize {
    let width = inner_width as usize;
    let room = match layout {
        PaneLayout::Table => width.saturating_sub(BAR_LABEL_ROOM),
        PaneLayout::Stacked => width,
    };
    if room < MIN_BAR_WIDTH {
        return 0;
    }
    room.min(MAX_BAR_WIDTH)
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

/// Append a `width`-cell bar for `pct` (0..=100): `─` for the filled
/// part (bold yellow), `·` for the rest (dim gray). The filled part rounds
/// to the nearest cell, so the bar claims at most half a cell of share it
/// does not have. A zero width draws no bar at all.
fn push_bar_spans(spans: &mut Vec<Span>, pct: u32, width: usize) {
    if width == 0 {
        return;
    }
    let filled = (((pct.min(100) as usize) * width) + 50) / 100;
    if filled > 0 {
        spans.push(Span::styled(
            "─".repeat(filled),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if filled < width {
        spans.push(Span::styled(
            "·".repeat(width - filled),
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

/// Append the "Tokens: <bar> X / Y (Z%)" row to `spans` — the context
/// size (the last call's input plus its output) against the context
/// window, with the same share drawn as a bar of `bar_cells` cells. The
/// label is padded to the pane's label width, so the bar starts in the
/// column every other value starts in. Pushes nothing when
/// `context_input_tokens` or `max_tokens` is missing.
pub(super) fn push_tokens_span(spans: &mut Vec<Span>, t: &TopicInfo, bar_cells: usize) {
    if let (Some(cur), Some(max)) = (t.context_input_tokens, t.max_tokens) {
        let used = jyc_types::context_tokens(Some(cur), t.context_output_tokens).unwrap_or(cur);
        let pct = context_pct(t).unwrap_or(0);
        spans.push(label_span(TOKENS_LABEL));
        push_bar_spans(spans, pct, bar_cells);
        let figures = format!("{used} / {max} ({pct}%)");
        spans.push(Span::raw(if bar_cells > 0 {
            format!(" {figures}")
        } else {
            figures
        }));
    }
}

/// Append the "Output: N" row to `spans`. Pushes nothing when
/// `output_tokens` is missing.
pub(super) fn push_output_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(out) = t.output_tokens {
        spans.push(label_span("Output: "));
        spans.push(Span::raw(format!("{out}")));
    }
}

/// Append the "Total input: N" row to `spans`. Pushes nothing when
/// `total_input_tokens` is missing. Distinct from `push_tokens_span`
/// (which shows the current context size); this shows the lifetime sum
/// across all LLM calls in the session.
pub(super) fn push_total_input_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(total) = t.total_input_tokens {
        spans.push(label_span("Total input: "));
        spans.push(Span::raw(format!("{total}")));
    }
}

/// Append the "Cache hits: N" row to `spans`. Pushes nothing when
/// `total_cache_hit_tokens` is missing. Mirrors `push_total_input_span`
/// but for prompt-cache hits — the running sum of every LLM call's
/// cache-hit tokens (= tokens served from the provider's prompt cache
/// rather than re-billed as fresh input). Not shown in the dashboard
/// overview list — only the chat info pane and dashboard topic info
/// area call this.
pub(super) fn push_cache_hit_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(cache_hit) = t.total_cache_hit_tokens {
        spans.push(label_span("Cache hits: "));
        spans.push(Span::raw(format!("{cache_hit}")));
    }
}

/// Append the "Cache create: N" row to `spans`. Pushes nothing when
/// `total_cache_creation_tokens` is missing. Only Anthropic and GPT-5.6
/// (`cache_write_tokens`) report writes separately from reads; for every
/// other vendor this stays `None` and the row never renders. Sits next to
/// [`push_cache_hit_span`] in the chat info pane and dashboard topic
/// info area so users with cache-heavy Anthropic workflows can see
/// the write volume that the cache-creation premium rate applies to.
pub(super) fn push_cache_creation_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(cache_creation) = t.total_cache_creation_tokens {
        spans.push(label_span("Cache create: "));
        spans.push(Span::raw(format!("{cache_creation}")));
    }
}

/// Compute the cache hit rate for a topic: cache-hit tokens as a share of
/// total input tokens (`total_input_tokens` already includes the cache-hit
/// portion, so it is the correct denominator). Returns `None` when either
/// bound is missing or `total` is zero. The figure itself comes from the
/// shared helper, so `/info` prints the same percentage.
///
/// The helper is named after the older "cache util" wording; cache util
/// and cache hit rate are the same number.
pub(super) fn cache_util_pct(t: &TopicInfo) -> Option<u32> {
    jyc_types::cache_util_pct(t.total_cache_hit_tokens, t.total_input_tokens)
}

/// Append the "Cache hit rate: <bar> Z%" row to `spans`, the bar being
/// `bar_cells` cells wide. The label is padded to the pane's label
/// width, so the bar starts in the column every other value starts in.
/// Pushes nothing when cache-hit or total-input data is missing (e.g.
/// providers without prompt caching), so such topics show no row rather
/// than a misleading 0%. Sits next to [`push_cache_hit_span`] /
/// [`push_cache_creation_span`] in the chat info pane and dashboard topic
/// info area.
///
/// The row used to read "Cache util"; "cache util" and "cache hit rate"
/// are the same figure — see [`cache_util_pct`] and
/// `jyc_types::cache_util_pct`.
pub(super) fn push_cache_utilization_span(spans: &mut Vec<Span>, t: &TopicInfo, bar_cells: usize) {
    if let Some(pct) = cache_util_pct(t) {
        spans.push(label_span(CACHE_HIT_RATE_LABEL));
        push_bar_spans(spans, pct, bar_cells);
        let figures = format!("{pct}%");
        spans.push(Span::raw(if bar_cells > 0 {
            format!(" {figures}")
        } else {
            figures
        }));
    }
}

/// Append the "Reasoning: N" row to `spans`. Pushes nothing when the
/// provider reports no reasoning tokens — the hidden chain-of-thought
/// share of the output, already counted in the output tokens.
pub(super) fn push_reasoning_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(reasoning) = t.total_reasoning_tokens
        && reasoning > 0
    {
        spans.push(label_span("Reasoning: "));
        spans.push(Span::raw(format!("{reasoning}")));
    }
}

/// Append the "Cost: $X session · $Y today" row to `spans`. Pushes
/// nothing when the topic has no cost data (model without configured
/// `pricing`), so an unpriced topic shows no row at all rather than a
/// misleading zero.
///
/// `session` resets with the agent session; `today` is the durable
/// per-day total from the billing ledger — they differ after a reset by
/// design.
pub(super) fn push_cost_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(ref c) = t.cost {
        spans.push(label_span("Cost: "));
        spans.push(Span::raw(format!(
            "{} session · {} today",
            format_amount(c.session, &c.currency),
            format_amount(c.today, &c.currency),
        )));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A pane label as the pane draws it: padded to the label width, so
    /// that every value of the pane starts in the same column.
    fn padded(label: &str) -> String {
        format!("{label:<width$}", width = LABEL_WIDTH)
    }

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
            model: None,
            mode: None,
            context_input_tokens: input,
            context_output_tokens: context_output,
            max_tokens: max,
            output_tokens: output,
            total_input_tokens: total_input,
            total_cache_hit_tokens: cache_hit,
            total_cache_creation_tokens: cache_creation,
            total_reasoning_tokens: None,
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
        push_tokens_span(&mut spans, &t, MAX_BAR_WIDTH);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_tokens_span_writes_label_bar_and_value() {
        let t = info_with(Some(4750), Some(10000), None, None, None, None);
        let mut spans = Vec::new();
        push_tokens_span(&mut spans, &t, MAX_BAR_WIDTH);
        assert_eq!(spans.len(), 4);
        // The label is padded to the pane's label width, so the bar
        // starts in the column every other value starts in.
        assert_eq!(spans[0].content, padded(TOKENS_LABEL));
        assert_eq!(spans[1].content, "─".repeat(9));
        assert_eq!(spans[2].content, "·".repeat(11));
        assert_eq!(spans[3].content, " 4750 / 10000 (47%)");
    }

    /// A pane too narrow for an honest bar gets the figures alone — no
    /// bar, and none of the spans a bar would have added. The label is
    /// still padded: the pane's values share a column whether or not a
    /// bar stands in front of them.
    #[test]
    fn push_tokens_span_without_a_bar() {
        let t = info_with(Some(4750), Some(10000), None, None, None, None);
        let mut spans = Vec::new();
        push_tokens_span(&mut spans, &t, 0);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, padded(TOKENS_LABEL));
        assert_eq!(spans[1].content, "4750 / 10000 (47%)");
    }

    /// The two percentage rows share one bar width and one label width, so
    /// their bars start in the same column and their shares can be compared
    /// by eye.
    #[test]
    fn the_two_bars_start_in_the_same_column() {
        let t = info_with(Some(4750), Some(10000), None, Some(1000), Some(880), None);
        let mut tokens = Vec::new();
        push_tokens_span(&mut tokens, &t, MAX_BAR_WIDTH);
        let mut cache = Vec::new();
        push_cache_utilization_span(&mut cache, &t, MAX_BAR_WIDTH);
        // The labels are the same width, and the bars follow them directly.
        assert_eq!(tokens[0].content.len(), cache[0].content.len());
        assert_eq!(tokens[0].content.len(), CACHE_HIT_RATE_LABEL.len());
        assert_eq!(cache[0].content, CACHE_HIT_RATE_LABEL);
    }

    /// Every label of the pane is padded to the same width, so every
    /// value starts in the same column — the pane reads as one table
    /// rather than a ragged list.
    #[test]
    fn every_label_is_padded_to_the_same_width() {
        for label in [
            "Topic: ",
            "Channel: ",
            "Pattern: ",
            "Model: ",
            "Mode: ",
            "Branch: ",
            "Status: ",
            TOKENS_LABEL,
            "Output: ",
            "Total input: ",
            "Cache hits: ",
            CACHE_HIT_RATE_LABEL,
            "Cache create: ",
            "Reasoning: ",
            "Cost: ",
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
        // The table layout: the longest label ("Cache hit rate: ", 16
        // cells) plus the one cell the bar keeps to itself at the pane's
        // edge.
        assert_eq!(bar_width(39, PaneLayout::Table), MAX_BAR_WIDTH);
        assert_eq!(bar_width(37, PaneLayout::Table), MAX_BAR_WIDTH);
        assert_eq!(bar_width(36, PaneLayout::Table), MAX_BAR_WIDTH - 1);
        assert_eq!(bar_width(23, PaneLayout::Table), 6);
        // The narrowest bar still worth drawing.
        assert_eq!(bar_width(22, PaneLayout::Table), MIN_BAR_WIDTH);
        // Below that a cell would carry too much of the share to be honest.
        assert_eq!(bar_width(21, PaneLayout::Table), 0);
        assert_eq!(bar_width(17, PaneLayout::Table), 0);
        assert_eq!(bar_width(16, PaneLayout::Table), 0);
        assert_eq!(bar_width(0, PaneLayout::Table), 0);
    }

    /// The stacked layout gives the bar a row of its own, so it can use
    /// the pane's full width — it is never cut by the label beside it,
    /// because there is no label beside it.
    #[test]
    fn bar_width_in_the_stacked_layout_uses_the_whole_row() {
        assert_eq!(bar_width(39, PaneLayout::Stacked), MAX_BAR_WIDTH);
        assert_eq!(bar_width(23, PaneLayout::Stacked), MAX_BAR_WIDTH);
        assert_eq!(bar_width(21, PaneLayout::Stacked), 21);
        assert_eq!(bar_width(20, PaneLayout::Stacked), 20);
        // The narrowest bar still worth drawing.
        assert_eq!(bar_width(5, PaneLayout::Stacked), MIN_BAR_WIDTH);
        // Below that a cell would carry too much of the share to be honest.
        assert_eq!(bar_width(4, PaneLayout::Stacked), 0);
        assert_eq!(bar_width(0, PaneLayout::Stacked), 0);
    }

    /// The layout switches at one width, and the whole pane switches with
    /// it: a pane that can hold a label column and a value column lays
    /// its rows out as a table, a narrower one stacks them.
    #[test]
    fn the_layout_switches_at_the_table_threshold() {
        assert_eq!(layout_for(TABLE_MIN_INNER_WIDTH), PaneLayout::Table);
        assert_eq!(layout_for(TABLE_MIN_INNER_WIDTH - 1), PaneLayout::Stacked);
        assert_eq!(layout_for(34), PaneLayout::Table);
        assert_eq!(layout_for(33), PaneLayout::Stacked);
        // A pane of any width at all still gets a layout.
        assert_eq!(layout_for(0), PaneLayout::Stacked);
    }

    /// The bar rounds to the nearest cell and the filled part is the bold
    /// yellow one. A share too small to fill a cell draws no filled part
    /// at all: the bar never claims more than the share it stands for.
    #[test]
    fn push_bar_spans_fills_and_pads() {
        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 0, MAX_BAR_WIDTH);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "·".repeat(MAX_BAR_WIDTH));

        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 5, MAX_BAR_WIDTH);
        assert_eq!(spans[0].content, "─");
        assert_eq!(spans[1].content, "·".repeat(MAX_BAR_WIDTH - 1));
        assert_eq!(spans[0].style.fg, Some(Color::Yellow));
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[1].style.fg, Some(Color::DarkGray));

        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 50, MAX_BAR_WIDTH);
        assert_eq!(spans[0].content, "─".repeat(MAX_BAR_WIDTH / 2));
        assert_eq!(spans[1].content, "·".repeat(MAX_BAR_WIDTH / 2));

        // A full bar has no padding left to draw.
        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 100, MAX_BAR_WIDTH);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "─".repeat(MAX_BAR_WIDTH));

        // Above 100% fills the bar without overflowing it.
        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 140, MAX_BAR_WIDTH);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "─".repeat(MAX_BAR_WIDTH));

        // A share too small for a cell draws no filled part: the bar stays
        // empty rather than overstating the share.
        let mut spans = Vec::new();
        push_bar_spans(&mut spans, 2, MAX_BAR_WIDTH);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "·".repeat(MAX_BAR_WIDTH));

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
        let t = info_with(Some(100), Some(10000), Some(420), None, None, None);
        let mut spans = Vec::new();
        push_output_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, padded("Output: "));
        assert_eq!(spans[1].content, "420");
    }

    #[test]
    fn push_total_input_span_omits_when_missing() {
        let t = info_with(Some(100), Some(10000), Some(50), None, None, None);
        let mut spans = Vec::new();
        push_total_input_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_total_input_span_writes_label_and_value() {
        let t = info_with(Some(100), Some(10000), Some(50), Some(720), None, None);
        let mut spans = Vec::new();
        push_total_input_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, padded("Total input: "));
        assert_eq!(spans[1].content, "720");
    }

    #[test]
    fn push_cache_hit_span_omits_when_missing() {
        let t = info_with(Some(100), Some(10000), Some(50), Some(720), None, None);
        let mut spans = Vec::new();
        push_cache_hit_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_cache_hit_span_writes_label_and_value() {
        let t = info_with(Some(100), Some(10000), Some(50), Some(720), Some(640), None);
        let mut spans = Vec::new();
        push_cache_hit_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, padded("Cache hits: "));
        assert_eq!(spans[1].content, "640");
    }

    /// No cost data (unpriced model) → no row, rather than `$0.0000`.
    #[test]
    fn push_cost_span_omits_when_no_cost() {
        let t = info_with(Some(100), Some(10000), Some(50), Some(720), Some(640), None);
        let mut spans = Vec::new();
        push_cost_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    /// Both figures render, and USD uses the `$` symbol.
    #[test]
    fn push_cost_span_writes_session_and_today() {
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
        assert_eq!(spans[1].content, "$0.0521 session · $1.3057 today");
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
        assert_eq!(spans[1].content, "¥2.5000 session · ¥10.0000 today");
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
        assert_eq!(
            spans[1].content,
            "1.0000 mixed session · 2.0000 mixed today"
        );
    }

    /// Sub-cent costs must not round away to zero — the reason for four
    /// decimal places.
    #[test]
    fn push_cost_span_keeps_sub_cent_precision() {
        let mut t = info_with(None, None, None, None, None, None);
        t.cost = Some(jyc_types::TopicCost {
            session: 0.0003,
            today: 0.0007,
            currency: "USD".into(),
        });
        let mut spans = Vec::new();
        push_cost_span(&mut spans, &t);
        assert_eq!(spans[1].content, "$0.0003 session · $0.0007 today");
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
    /// existing cache-hit row. The label matches Anthropic's wire
    /// field (`cache_creation_input_tokens`) so users can map what
    /// they see back to the API.
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
        assert_eq!(spans[0].content, padded("Cache create: "));
        assert_eq!(spans[1].content, "1280");
    }

    #[test]
    fn cache_util_pct_basic() {
        let t = info_with(None, None, None, Some(720), Some(640), None);
        assert_eq!(cache_util_pct(&t), Some(88));
    }

    #[test]
    fn cache_util_pct_zero_total_returns_none() {
        let t = info_with(None, None, None, Some(0), Some(0), None);
        assert_eq!(cache_util_pct(&t), None);
    }

    #[test]
    fn cache_util_pct_missing_hit_returns_none() {
        let t = info_with(None, None, None, Some(720), None, None);
        assert_eq!(cache_util_pct(&t), None);
    }

    #[test]
    fn push_cache_utilization_span_omits_when_missing() {
        let t = info_with(None, None, None, None, None, None);
        let mut spans = Vec::new();
        push_cache_utilization_span(&mut spans, &t, MAX_BAR_WIDTH);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_cache_utilization_span_writes_label_bar_and_value() {
        let t = info_with(None, None, None, Some(720), Some(640), None);
        let mut spans = Vec::new();
        push_cache_utilization_span(&mut spans, &t, MAX_BAR_WIDTH);
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].content, padded(CACHE_HIT_RATE_LABEL));
        assert_eq!(spans[1].content, "─".repeat(18));
        assert_eq!(spans[2].content, "·".repeat(2));
        assert_eq!(spans[3].content, " 88%");
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
}
