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
/// (93%)"`.
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
}
