//! Shared token-display helpers for the dashboard TUI.
//!
//! Two render sites show the same two fields (input token usage and
//! accumulated output tokens): the chat info pane pushes one `Line` per
//! row into a `Vec<Line>`, while the dashboard status line pushes
//! `Span`s into a single flat `Vec<Span>`. Both call these helpers so the
//! format and styling stay consistent.
//!
//! Kept module-private (`pub(super)`) — these are TUI internals, not part
//! of the dashboard's public surface.

use jyc_types::ThreadSummary;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// Compute the input-token percentage for a thread. Returns `None` when
/// either bound is missing or `max` is zero. Uses checked arithmetic to
/// avoid wrapping when `cur` is very large.
pub(super) fn input_token_pct(t: &ThreadSummary) -> Option<u32> {
    match (t.context_input_tokens, t.max_tokens) {
        (Some(cur), Some(max)) if max > 0 => Some(
            cur.checked_mul(100)
                .and_then(|v| v.checked_div(max))
                .unwrap_or(0) as u32,
        ),
        _ => None,
    }
}

/// Append the "Tokens: X / Y (Z%)" row to `spans`. Pushes nothing when
/// `context_input_tokens` or `max_tokens` is missing.
pub(super) fn push_tokens_span(spans: &mut Vec<Span>, t: &ThreadSummary) {
    if let (Some(cur), Some(max)) = (t.context_input_tokens, t.max_tokens) {
        let pct = input_token_pct(t).unwrap_or(0);
        spans.push(Span::styled(
            "Tokens: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!("{cur} / {max} ({pct}%)")));
    }
}

/// Append the "Output: N" row to `spans`. Pushes nothing when
/// `output_tokens` is missing.
pub(super) fn push_output_span(spans: &mut Vec<Span>, t: &ThreadSummary) {
    if let Some(out) = t.output_tokens {
        spans.push(Span::styled(
            "Output: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!("{out}")));
    }
}

/// Append the "Total input: N" row to `spans`. Pushes nothing when
/// `total_input_tokens` is missing. Distinct from `push_tokens_span`
/// (which shows the current context size); this shows the lifetime sum
/// across all LLM calls in the session.
pub(super) fn push_total_input_span(spans: &mut Vec<Span>, t: &ThreadSummary) {
    if let Some(total) = t.total_input_tokens {
        spans.push(Span::styled(
            "Total input: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!("{total}")));
    }
}

/// Two-space gap used by the dashboard status line between adjacent
/// chips. Callers on a flat status line prepend this manually before
/// each row (the chat info pane stacks rows on separate lines and
/// doesn't need it).
pub(super) const STATUS_SEP: &str = "  ";

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_with(
        input: Option<u64>,
        max: Option<u64>,
        output: Option<u64>,
        total_input: Option<u64>,
    ) -> ThreadSummary {
        ThreadSummary {
            name: "t".into(),
            channel: "c".into(),
            pattern: None,
            status: jyc_types::ThreadStatus::Idle,
            model: None,
            mode: None,
            context_input_tokens: input,
            max_tokens: max,
            output_tokens: output,
            total_input_tokens: total_input,
            last_active_at: None,
            skills: vec![],
            thread_path: None,
        }
    }

    #[test]
    fn input_token_pct_basic() {
        let t = summary_with(Some(5000), Some(10000), None, None);
        assert_eq!(input_token_pct(&t), Some(50));
    }

    #[test]
    fn input_token_pct_zero_max_returns_none() {
        let t = summary_with(Some(100), Some(0), None, None);
        assert_eq!(input_token_pct(&t), None);
    }

    #[test]
    fn input_token_pct_missing_input_returns_none() {
        let t = summary_with(None, Some(10000), None, None);
        assert_eq!(input_token_pct(&t), None);
    }

    #[test]
    fn push_tokens_span_omits_when_max_missing() {
        let t = summary_with(Some(100), None, None, None);
        let mut spans = Vec::new();
        push_tokens_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_tokens_span_writes_label_and_value() {
        let t = summary_with(Some(4750), Some(10000), None, None);
        let mut spans = Vec::new();
        push_tokens_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Tokens: ");
        assert_eq!(spans[1].content, "4750 / 10000 (47%)");
    }

    #[test]
    fn push_output_span_omits_when_missing() {
        let t = summary_with(Some(100), Some(10000), None, None);
        let mut spans = Vec::new();
        push_output_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_output_span_writes_label_and_value() {
        let t = summary_with(Some(100), Some(10000), Some(420), None);
        let mut spans = Vec::new();
        push_output_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Output: ");
        assert_eq!(spans[1].content, "420");
    }

    #[test]
    fn push_total_input_span_omits_when_missing() {
        let t = summary_with(Some(100), Some(10000), Some(50), None);
        let mut spans = Vec::new();
        push_total_input_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_total_input_span_writes_label_and_value() {
        let t = summary_with(Some(100), Some(10000), Some(50), Some(720));
        let mut spans = Vec::new();
        push_total_input_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Total input: ");
        assert_eq!(spans[1].content, "720");
    }
}
