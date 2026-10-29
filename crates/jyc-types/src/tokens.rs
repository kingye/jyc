//! Shared token figures.
//!
//! The topic info pane (TUI) and `/info` (the chat command) report the same
//! figures from the same data; the percentages they print come from here, so
//! the two surfaces cannot drift apart.

/// The context size in tokens: the most recent LLM call's input plus that
/// same call's output.
///
/// The input tokens are the context the call was sent; its output tokens are
/// the reply that gets appended to the context. Together they are the size of
/// the context the *next* call will carry — the honest "how full is the
/// window" figure.
///
/// `None` when the input tokens are unknown (no session data yet). A missing
/// output count is zero: a call that reported no output tokens contributed
/// none.
pub fn context_tokens(input: Option<u64>, output: Option<u64>) -> Option<u64> {
    let input = input?;
    Some(input.saturating_add(output.unwrap_or(0)))
}

/// The context size as a share of the context window, in percent.
///
/// `window` is the model's raw context window — the whole budget, not a
/// fraction of it. `None` when the window is missing or zero. Checked
/// arithmetic, so a very large count cannot wrap.
pub fn context_token_pct(
    input: Option<u64>,
    output: Option<u64>,
    window: Option<u64>,
) -> Option<u32> {
    let used = context_tokens(input, output)?;
    let window = window?;
    if window == 0 {
        return None;
    }
    Some(
        used.checked_mul(100)
            .and_then(|v| v.checked_div(window))
            .unwrap_or(0) as u32,
    )
}

/// The prompt-cache hits as a share of the total input tokens, in percent —
/// the **cache hit rate**.
///
/// `total_input_tokens` already includes the cache-hit portion, so it is the
/// right denominator. `None` when either bound is missing or the total is
/// zero. Checked arithmetic, so a very large count cannot wrap.
///
/// The name follows the older "cache util" wording; cache util and cache
/// hit rate are the same figure.
pub fn cache_util_pct(cache_hits: Option<u64>, total_input: Option<u64>) -> Option<u32> {
    match (cache_hits, total_input) {
        (Some(hit), Some(total)) if total > 0 => Some(
            hit.checked_mul(100)
                .and_then(|v| v.checked_div(total))
                .unwrap_or(0) as u32,
        ),
        _ => None,
    }
}

/// Format a token count for display: the raw count below 1024, then K
/// (1024) or M (1024²) with two decimals.
///
/// Both surfaces that report token counts — the topic info pane and
/// `/info` — print them this way, so the same count reads the same in
/// either place.
pub fn format_token_count(count: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if count >= MIB {
        format!("{:.2}M", count as f64 / MIB as f64)
    } else if count >= KIB {
        format!("{:.2}K", count as f64 / KIB as f64)
    } else {
        format!("{count}")
    }
}

/// The cache figures of one scope — a turn or a session — as the pane and
/// `/info` print them: the cache-hit tokens, the total input tokens they
/// are a share of, and that share in percent, e.g. `"964.51K / 1.18M
/// (80%)"`.
///
/// `None` when the scope has no input tokens yet — there is nothing for
/// the hits to be a share of — so the caller omits the row rather than
/// printing a misleading zero.
pub fn format_cache_rate(cache_hits: Option<u64>, total_input: Option<u64>) -> Option<String> {
    let total = total_input?;
    if total == 0 {
        return None;
    }
    let hits = cache_hits.unwrap_or(0);
    let pct = cache_util_pct(Some(hits), Some(total)).unwrap_or(0);
    Some(format!(
        "{} / {} ({pct}%)",
        format_token_count(hits),
        format_token_count(total)
    ))
}

/// The output figures of both scopes — the session total and the latest
/// turn — as the pane and `/info` print them, e.g. `"12.00K turn ·
/// 1.18M session"`. The turn comes first: it is the figure that moves, and
/// the session total is the anchor it is read against.
///
/// `None` when the session has produced no output at all: a session that
/// never completed a call has nothing to report, and the caller omits the
/// row. A turn that has not called yet reads as `0 turn` — it is a real
/// zero, like the pane's turn cache row, which is drawn at zero before the
/// turn's first call (`/info` omits that row instead: a 0/0 rate says
/// nothing, while a 0 output is a figure).
pub fn format_output_pair(session: Option<u64>, turn: Option<u64>) -> Option<String> {
    let session = session?;
    let turn = turn.unwrap_or(0);
    Some(format!(
        "{} turn · {} session",
        format_token_count(turn),
        format_token_count(session)
    ))
}

/// The estimated token weight of a context composition: the sum of every
/// region's heuristic. `0` for an empty composition.
pub fn context_composition_total(regions: &[crate::inspect::ContextRegionInfo]) -> u64 {
    regions.iter().fold(0u64, |sum, region| {
        sum.saturating_add(region.estimated_tokens)
    })
}

/// The width of the legend's label column: the widest [`ContextRegion::label`]
/// in the composition, so every share of the legend starts in one column.
///
/// Counted in characters, not bytes, because that is what the padding in
/// [`format_context_region_label`] counts — the two must agree for the
/// column to line up.
///
/// [`ContextRegion::label`]: crate::inspect::ContextRegion::label
pub fn context_composition_label_width(regions: &[crate::inspect::ContextRegionInfo]) -> usize {
    regions
        .iter()
        .map(|region| region.region.label().chars().count())
        .max()
        .unwrap_or(0)
}

/// The legend's label column for one region: `indent` spaces, the region's
/// [`label`] padded to `label_width`, and one trailing space, so the share
/// that follows starts in the same column on every row.
///
/// One owner for the column's shape: the TUI colours the result and `/info`
/// prints it plain, so both pad the label identically. The `indent` is a
/// caller parameter and deliberately differs — the pane lines the rows up
/// under the bar, the command under `Context:`.
///
/// [`label`]: crate::inspect::ContextRegion::label
pub fn format_context_region_label(
    region: crate::inspect::ContextRegion,
    label_width: usize,
    indent: usize,
) -> String {
    format!(
        "{}{:<width$} ",
        " ".repeat(indent),
        region.label(),
        width = label_width
    )
}

/// One region's share of the estimated context weight, in percent.
///
/// Rounded to the nearest whole percent: truncating instead left a
/// composition whose real shares are 60.0 / 13.3 / 26.7 printing as
/// 60 / 13 / 26 — a legend that visibly adds up to 99%. Independent
/// rounding can still leave a one-point gap when several regions tie,
/// so the rows are not forced to sum to 100.
///
/// `0` when the composition's total is zero: there is no share to report.
/// The arithmetic is in `u128`, so a very large estimate cannot wrap.
pub fn context_region_pct(region: &crate::inspect::ContextRegionInfo, total: u64) -> u32 {
    if total == 0 {
        return 0;
    }
    let tokens = u128::from(region.estimated_tokens);
    let total = u128::from(total);
    ((tokens * 100 + total / 2) / total) as u32
}

/// A region's size and share in the compact form the legend prints,
/// e.g. `"~17.77K · 52%"`. The region's label leads it, in its own column:
/// the TUI colours it, `/info` indents it.
pub fn format_context_region_share(
    region: &crate::inspect::ContextRegionInfo,
    total: u64,
) -> String {
    format!(
        "~{} · {}%",
        format_token_count(region.estimated_tokens),
        context_region_pct(region, total)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_count_is_raw_below_a_kibibyte() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1023), "1023");
    }

    #[test]
    fn token_count_is_two_decimals_from_a_kibibyte() {
        assert_eq!(format_token_count(1024), "1.00K");
        assert_eq!(format_token_count(12288), "12.00K");
        assert_eq!(format_token_count(987654), "964.51K");
        assert_eq!(format_token_count(1024 * 1024), "1.00M");
        assert_eq!(format_token_count(1_234_567), "1.18M");
    }

    #[test]
    fn cache_rate_prints_hits_over_input_and_the_share() {
        assert_eq!(
            format_cache_rate(Some(300), Some(1000)),
            Some("300 / 1000 (30%)".to_string())
        );
        assert_eq!(
            format_cache_rate(Some(12288), Some(15360)),
            Some("12.00K / 15.00K (80%)".to_string())
        );
    }

    /// A scope with input but no recorded hits still reports its share.
    #[test]
    fn cache_rate_counts_missing_hits_as_zero() {
        assert_eq!(
            format_cache_rate(None, Some(1000)),
            Some("0 / 1000 (0%)".to_string())
        );
    }

    #[test]
    fn cache_rate_needs_input_tokens() {
        assert_eq!(format_cache_rate(Some(300), None), None);
        assert_eq!(format_cache_rate(Some(300), Some(0)), None);
    }

    #[test]
    fn output_pair_prints_the_turn_then_the_session() {
        assert_eq!(
            format_output_pair(Some(1_234_567), Some(12288)),
            Some("12.00K turn · 1.18M session".to_string())
        );
        assert_eq!(
            format_output_pair(Some(500), Some(500)),
            Some("500 turn · 500 session".to_string())
        );
    }

    /// A turn that has not called yet is a real zero, printed as one.
    #[test]
    fn output_pair_counts_a_turn_without_calls_as_zero() {
        assert_eq!(
            format_output_pair(Some(500), None),
            Some("0 turn · 500 session".to_string())
        );
    }

    #[test]
    fn output_pair_needs_the_session_figure() {
        assert_eq!(format_output_pair(None, Some(500)), None);
    }

    #[test]
    fn context_tokens_is_input_plus_output() {
        assert_eq!(context_tokens(Some(4750), Some(250)), Some(5000));
        assert_eq!(context_tokens(Some(4750), None), Some(4750));
        assert_eq!(context_tokens(None, Some(250)), None);
    }

    #[test]
    fn context_pct_is_the_share_of_the_context_window() {
        assert_eq!(
            context_token_pct(Some(4750), Some(250), Some(10000)),
            Some(50)
        );
        assert_eq!(context_token_pct(Some(4750), None, Some(10000)), Some(47));
        assert_eq!(context_token_pct(Some(4750), Some(250), None), None);
        assert_eq!(context_token_pct(Some(4750), Some(250), Some(0)), None);
    }

    /// A call that reported no output tokens still counts as context.
    #[test]
    fn context_pct_counts_a_call_without_output() {
        assert_eq!(context_token_pct(Some(1000), None, Some(10000)), Some(10));
    }

    #[test]
    fn cache_util_is_the_share_of_the_total_input() {
        assert_eq!(cache_util_pct(Some(300), Some(1000)), Some(30));
        assert_eq!(cache_util_pct(Some(300), None), None);
        assert_eq!(cache_util_pct(Some(300), Some(0)), None);
    }

    fn region(
        region: crate::inspect::ContextRegion,
        messages: u64,
        bytes: u64,
        estimated_tokens: u64,
    ) -> crate::inspect::ContextRegionInfo {
        crate::inspect::ContextRegionInfo {
            region,
            messages,
            bytes,
            estimated_tokens,
        }
    }

    #[test]
    fn composition_total_adds_every_region_without_wrapping() {
        let regions = [
            region(crate::inspect::ContextRegion::History, 2, 100, 10),
            region(crate::inspect::ContextRegion::Current, 1, 50, 5),
        ];
        assert_eq!(context_composition_total(&regions), 15);
        assert_eq!(context_composition_total(&[]), 0);
        assert_eq!(
            context_composition_total(&[
                region(crate::inspect::ContextRegion::Full, 1, 1, u64::MAX),
                region(crate::inspect::ContextRegion::Full, 1, 1, 1),
            ]),
            u64::MAX
        );
    }

    #[test]
    fn region_share_is_the_estimated_share_of_the_composition() {
        let history = region(crate::inspect::ContextRegion::History, 2, 100, 52);
        let current = region(crate::inspect::ContextRegion::Current, 1, 50, 48);
        assert_eq!(context_region_pct(&history, 100), 52);
        assert_eq!(context_region_pct(&current, 100), 48);
        assert_eq!(context_region_pct(&current, 0), 0);
        // 60.0 / 13.3 / 26.7 percent rounds to a legend that still adds
        // up to 100%; truncation would print 60 / 13 / 26.
        let history_60 = region(crate::inspect::ContextRegion::History, 1, 50, 18);
        let checkpoint_13 = region(crate::inspect::ContextRegion::Checkpoint, 1, 50, 4);
        let current_27 = region(crate::inspect::ContextRegion::Current, 1, 50, 8);
        assert_eq!(context_region_pct(&history_60, 30), 60);
        assert_eq!(context_region_pct(&checkpoint_13, 30), 13);
        assert_eq!(context_region_pct(&current_27, 30), 27);
        assert_eq!(
            format_context_region_share(&history, 100),
            "~52 · 52%".to_string()
        );
    }

    #[test]
    fn composition_label_column_is_the_widest_label() {
        let regions = [
            region(crate::inspect::ContextRegion::History, 31, 132_000, 18_200),
            region(crate::inspect::ContextRegion::Checkpoint, 1, 12, 4),
            region(crate::inspect::ContextRegion::Current, 1, 12, 8),
        ];
        assert_eq!(
            context_composition_label_width(&regions),
            "Current turn".len()
        );
        assert_eq!(
            context_composition_label_width(&regions[..1]),
            "History".len()
        );
        assert_eq!(context_composition_label_width(&[]), 0);
    }

    /// One owner for the label column: indent, padded label, one space.
    /// The label is padded by character count, which is what the width
    /// helper counts — the label column the TUI and `/info` build is the
    /// same one. The indent is a caller parameter and deliberately differs
    /// (the pane lines the rows up under the bar, the command under
    /// `Context:`).
    #[test]
    fn region_label_column_pads_by_characters() {
        assert_eq!(
            format_context_region_label(crate::inspect::ContextRegion::History, 12, 9),
            "         History      "
        );
        assert_eq!(
            format_context_region_label(crate::inspect::ContextRegion::Current, 12, 2),
            "  Current turn "
        );
        // A label wider than the column is not truncated — the column
        // comes from the widest label, so this only happens on a caller
        // bug; the formatter still returns the label whole.
        assert_eq!(
            format_context_region_label(crate::inspect::ContextRegion::Checkpoint, 1, 0),
            "Checkpoint "
        );
    }
}
