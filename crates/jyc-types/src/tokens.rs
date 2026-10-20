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
