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

use jyc_types::TopicSummary;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// Compute the input-token percentage for a topic. Returns `None` when
/// either bound is missing or `max` is zero. Uses checked arithmetic to
/// avoid wrapping when `cur` is very large.
pub(super) fn input_token_pct(t: &TopicSummary) -> Option<u32> {
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
pub(super) fn push_tokens_span(spans: &mut Vec<Span>, t: &TopicSummary) {
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
pub(super) fn push_output_span(spans: &mut Vec<Span>, t: &TopicSummary) {
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
pub(super) fn push_total_input_span(spans: &mut Vec<Span>, t: &TopicSummary) {
    if let Some(total) = t.total_input_tokens {
        spans.push(Span::styled(
            "Total input: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
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
pub(super) fn push_cache_hit_span(spans: &mut Vec<Span>, t: &TopicSummary) {
    if let Some(cache_hit) = t.total_cache_hit_tokens {
        spans.push(Span::styled(
            "Cache hits: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!("{cache_hit}")));
    }
}

/// Append the "Cache create: N" row to `spans`. Pushes nothing when
/// `total_cache_creation_tokens` is missing. Anthropic is the only
/// provider that reports writes separately from reads; for every other
/// vendor this stays `None` and the row never renders. Sits next to
/// [`push_cache_hit_span`] in the chat info pane and dashboard topic
/// info area so users with cache-heavy Anthropic workflows can see
/// the write volume that the cache-creation premium rate applies to.
pub(super) fn push_cache_creation_span(spans: &mut Vec<Span>, t: &TopicSummary) {
    if let Some(cache_creation) = t.total_cache_creation_tokens {
        spans.push(Span::styled(
            "Cache create: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!("{cache_creation}")));
    }
}

/// Format a cost amount for display.
///
/// Renders `$` for USD and `¥` for CNY (the two currencies the feature
/// was specified against); any other label is shown as a suffix, so an
/// unrecognised currency still reads correctly rather than being
/// mislabelled with the wrong symbol.
///
/// Four decimals: a single cheap call can cost well under a cent, and
/// rounding to 2 places would display it as `0.00`.
fn format_amount(amount: f64, currency: &str) -> String {
    match currency {
        "USD" => format!("${amount:.4}"),
        "CNY" => format!("¥{amount:.4}"),
        other => format!("{amount:.4} {other}"),
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
pub(super) fn push_cost_span(spans: &mut Vec<Span>, t: &TopicSummary) {
    if let Some(ref c) = t.cost {
        spans.push(Span::styled(
            "Cost: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!(
            "{} session · {} today",
            format_amount(c.session, &c.currency),
            format_amount(c.today, &c.currency),
        )));
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
        cache_hit: Option<u64>,
        cache_creation: Option<u64>,
    ) -> TopicSummary {
        TopicSummary {
            name: "t".into(),
            channel: "c".into(),
            pattern: None,
            status: jyc_types::TopicStatus::Idle,
            model: None,
            mode: None,
            branch: None,
            changed_files: None,
            context_input_tokens: input,
            max_tokens: max,
            output_tokens: output,
            total_input_tokens: total_input,
            total_cache_hit_tokens: cache_hit,
            total_cache_creation_tokens: cache_creation,
            last_active_at: None,
            skills: vec![],
            topic_path: None,
            cost: None,
        }
    }

    #[test]
    fn input_token_pct_basic() {
        let t = summary_with(Some(5000), Some(10000), None, None, None, None);
        assert_eq!(input_token_pct(&t), Some(50));
    }

    #[test]
    fn input_token_pct_zero_max_returns_none() {
        let t = summary_with(Some(100), Some(0), None, None, None, None);
        assert_eq!(input_token_pct(&t), None);
    }

    #[test]
    fn input_token_pct_missing_input_returns_none() {
        let t = summary_with(None, Some(10000), None, None, None, None);
        assert_eq!(input_token_pct(&t), None);
    }

    #[test]
    fn push_tokens_span_omits_when_max_missing() {
        let t = summary_with(Some(100), None, None, None, None, None);
        let mut spans = Vec::new();
        push_tokens_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_tokens_span_writes_label_and_value() {
        let t = summary_with(Some(4750), Some(10000), None, None, None, None);
        let mut spans = Vec::new();
        push_tokens_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Tokens: ");
        assert_eq!(spans[1].content, "4750 / 10000 (47%)");
    }

    #[test]
    fn push_output_span_omits_when_missing() {
        let t = summary_with(Some(100), Some(10000), None, None, None, None);
        let mut spans = Vec::new();
        push_output_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_output_span_writes_label_and_value() {
        let t = summary_with(Some(100), Some(10000), Some(420), None, None, None);
        let mut spans = Vec::new();
        push_output_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Output: ");
        assert_eq!(spans[1].content, "420");
    }

    #[test]
    fn push_total_input_span_omits_when_missing() {
        let t = summary_with(Some(100), Some(10000), Some(50), None, None, None);
        let mut spans = Vec::new();
        push_total_input_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_total_input_span_writes_label_and_value() {
        let t = summary_with(Some(100), Some(10000), Some(50), Some(720), None, None);
        let mut spans = Vec::new();
        push_total_input_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Total input: ");
        assert_eq!(spans[1].content, "720");
    }

    #[test]
    fn push_cache_hit_span_omits_when_missing() {
        let t = summary_with(Some(100), Some(10000), Some(50), Some(720), None, None);
        let mut spans = Vec::new();
        push_cache_hit_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_cache_hit_span_writes_label_and_value() {
        let t = summary_with(Some(100), Some(10000), Some(50), Some(720), Some(640), None);
        let mut spans = Vec::new();
        push_cache_hit_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Cache hits: ");
        assert_eq!(spans[1].content, "640");
    }

    /// No cost data (unpriced model) → no row, rather than `$0.0000`.
    #[test]
    fn push_cost_span_omits_when_no_cost() {
        let t = summary_with(Some(100), Some(10000), Some(50), Some(720), Some(640), None);
        let mut spans = Vec::new();
        push_cost_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    /// Both figures render, and USD uses the `$` symbol.
    #[test]
    fn push_cost_span_writes_session_and_today() {
        let mut t = summary_with(Some(100), Some(10000), Some(50), Some(720), Some(640), None);
        t.cost = Some(jyc_types::TopicCost {
            session: 0.0521,
            today: 1.3057,
            currency: "USD".into(),
        });
        let mut spans = Vec::new();
        push_cost_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Cost: ");
        assert_eq!(spans[1].content, "$0.0521 session · $1.3057 today");
    }

    /// CNY renders with `¥`, not a dollar sign.
    #[test]
    fn push_cost_span_uses_cny_symbol() {
        let mut t = summary_with(None, None, None, None, None, None);
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
        let mut t = summary_with(None, None, None, None, None, None);
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
        let mut t = summary_with(None, None, None, None, None, None);
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
        let t = summary_with(Some(100), Some(10000), Some(50), Some(720), Some(640), None);
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
        let t = summary_with(
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
        assert_eq!(spans[0].content, "Cache create: ");
        assert_eq!(spans[1].content, "1280");
    }
}
