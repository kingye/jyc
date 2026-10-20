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

use jyc_core::duration::format_latency;
use jyc_types::{TopicInfo, format_amount};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

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

/// Append the "Tokens: X / Y (Z%)" row to `spans` — the context size
/// (the last call's input plus its output) against the context window.
/// Pushes nothing when `context_input_tokens` or `max_tokens` is missing.
pub(super) fn push_tokens_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let (Some(cur), Some(max)) = (t.context_input_tokens, t.max_tokens) {
        let used = jyc_types::context_tokens(Some(cur), t.context_output_tokens).unwrap_or(cur);
        let pct = context_pct(t).unwrap_or(0);
        spans.push(Span::styled(
            "Tokens: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!("{used} / {max} ({pct}%)")));
    }
}

/// Append the "Output: N" row to `spans`. Pushes nothing when
/// `output_tokens` is missing.
pub(super) fn push_output_span(spans: &mut Vec<Span>, t: &TopicInfo) {
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
pub(super) fn push_total_input_span(spans: &mut Vec<Span>, t: &TopicInfo) {
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
pub(super) fn push_cache_hit_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(cache_hit) = t.total_cache_hit_tokens {
        spans.push(Span::styled(
            "Cache hits: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
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
        spans.push(Span::styled(
            "Cache create: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!("{cache_creation}")));
    }
}

/// Compute the cache-utilization percentage for a topic: cache-hit
/// tokens as a share of total input tokens (`total_input_tokens`
/// already includes the cache-hit portion, so it is the correct
/// denominator). Returns `None` when either bound is missing or `total`
/// is zero. The figure itself comes from the shared helper, so `/info`
/// prints the same percentage.
pub(super) fn cache_util_pct(t: &TopicInfo) -> Option<u32> {
    jyc_types::cache_util_pct(t.total_cache_hit_tokens, t.total_input_tokens)
}

/// Append the "Cache util: Z%" row to `spans`. Pushes nothing when
/// cache-hit or total-input data is missing (e.g. providers without
/// prompt caching), so such topics show no row rather than a
/// misleading 0%. Sits next to [`push_cache_hit_span`] /
/// [`push_cache_creation_span`] in the chat info pane and dashboard
/// topic info area.
pub(super) fn push_cache_utilization_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(pct) = cache_util_pct(t) {
        spans.push(Span::styled(
            "Cache util: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!("{pct}%")));
    }
}

/// Append the "Reasoning: N" row to `spans`. Pushes nothing when the
/// provider reports no reasoning tokens — the hidden chain-of-thought
/// share of the output, already counted in the output tokens.
pub(super) fn push_reasoning_span(spans: &mut Vec<Span>, t: &TopicInfo) {
    if let Some(reasoning) = t.total_reasoning_tokens
        && reasoning > 0
    {
        spans.push(Span::styled(
            "Reasoning: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
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
        spans.push(Span::styled(
            "Latency: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(latency));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        push_tokens_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_tokens_span_writes_label_and_value() {
        let t = info_with(Some(4750), Some(10000), None, None, None, None);
        let mut spans = Vec::new();
        push_tokens_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Tokens: ");
        assert_eq!(spans[1].content, "4750 / 10000 (47%)");
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
        assert_eq!(spans[0].content, "Output: ");
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
        assert_eq!(spans[0].content, "Total input: ");
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
        assert_eq!(spans[0].content, "Cache hits: ");
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
        assert_eq!(spans[0].content, "Cost: ");
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
        assert_eq!(spans[0].content, "Cache create: ");
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
        push_cache_utilization_span(&mut spans, &t);
        assert!(spans.is_empty());
    }

    #[test]
    fn push_cache_utilization_span_writes_label_and_value() {
        let t = info_with(None, None, None, Some(720), Some(640), None);
        let mut spans = Vec::new();
        push_cache_utilization_span(&mut spans, &t);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content, "Cache util: ");
        assert_eq!(spans[1].content, "88%");
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
        assert_eq!(spans[0].content, "Latency: ");
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
