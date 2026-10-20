//! Shared token figures.
//!
//! The topic info pane (TUI) and `/info` (the chat command) report the same
//! figures from the same data; the percentages they print come from here, so
//! the two surfaces cannot drift apart.

/// The input tokens as a share of the context window, in percent.
///
/// `None` when either bound is missing or `max` is zero. Checked arithmetic,
/// so a very large count cannot wrap.
pub fn input_token_pct(current: Option<u64>, max: Option<u64>) -> Option<u32> {
    match (current, max) {
        (Some(cur), Some(max)) if max > 0 => Some(
            cur.checked_mul(100)
                .and_then(|v| v.checked_div(max))
                .unwrap_or(0) as u32,
        ),
        _ => None,
    }
}

/// The prompt-cache hits as a share of the total input tokens, in percent.
///
/// `total_input_tokens` already includes the cache-hit portion, so it is the
/// right denominator. `None` when either bound is missing or the total is
/// zero. Checked arithmetic, so a very large count cannot wrap.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_pct_is_the_share_of_the_context_window() {
        assert_eq!(input_token_pct(Some(4750), Some(10000)), Some(47));
        assert_eq!(input_token_pct(Some(4750), None), None);
        assert_eq!(input_token_pct(Some(4750), Some(0)), None);
    }

    #[test]
    fn cache_util_is_the_share_of_the_total_input() {
        assert_eq!(cache_util_pct(Some(300), Some(1000)), Some(30));
        assert_eq!(cache_util_pct(Some(300), None), None);
        assert_eq!(cache_util_pct(Some(300), Some(0)), None);
    }
}
