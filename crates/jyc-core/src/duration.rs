//! Shared human-readable duration formatting.
//!
//! Single source of truth for how durations render across the dashboard,
//! chat screen, and channel-facing surfaces (Feishu status card, relayed
//! reply footers). Call sites keep their own timestamp parsing; this
//! module only turns a length of time into a string.

/// How much precision a duration display needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurationStyle {
    /// Live tickers — decimal sub-second liveness below one minute:
    /// `12.4s`, then `1m05s`, `1h02m03s`.
    Ticking,
    /// Progress and elapsed readouts — whole seconds: `42s`, `1m38s`;
    /// hours drop the seconds: `2h05m`.
    Precise,
    /// Coarse chips — whole minutes only: `38m`, `2h05m`.
    Coarse,
    /// Measured per-call latencies — milliseconds below a second, seconds
    /// with two decimals above it: `820ms`, `1.24s`. The unit follows the
    /// magnitude so a fast call is exact and a slow one stays short, and a
    /// sub-second call never collapses into `0s`.
    Millis,
}

/// Format a duration given in milliseconds per `style`.
pub fn format_duration_ms(ms: u64, style: DurationStyle) -> String {
    let s = ms / 1000;
    match style {
        DurationStyle::Ticking if ms < 60_000 => {
            format!("{}.{}s", s, (ms / 100) % 10)
        }
        DurationStyle::Ticking if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        DurationStyle::Ticking => format!("{}h{:02}m{:02}s", s / 3600, (s % 3600) / 60, s % 60),
        DurationStyle::Precise if s < 60 => format!("{s}s"),
        DurationStyle::Precise if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        DurationStyle::Precise => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        DurationStyle::Coarse if s < 3600 => format!("{}m", s / 60),
        DurationStyle::Coarse => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        DurationStyle::Millis if ms < 1000 => format!("{ms}ms"),
        DurationStyle::Millis => format!("{:.2}s", ms as f64 / 1000.0),
    }
}

/// Format a duration given in whole seconds per `style`.
pub fn format_duration_secs(secs: u64, style: DurationStyle) -> String {
    format_duration_ms(secs.saturating_mul(1000), style)
}

/// Format the value of the LLM call-latency row: `1.1s/call (3) ·
/// 820ms/call (37)` — the latest turn's average and the number of calls
/// it covers first, the session's second.
///
/// Each argument pairs an average with the count it was drawn from; a
/// pair is `None` when that scope has no measured call, and the row
/// collapses to whichever scope remains (no row at all when neither
/// has one — better than claiming `0ms`).
///
/// Shared by the dashboard status line / chat info pane and `/info` so
/// the wording cannot drift between them; each caller supplies its own
/// `Latency: ` label (one styles it, the other writes plain text).
pub fn format_latency(turn: Option<(u64, u64)>, session: Option<(u64, u64)>) -> Option<String> {
    let pair = |avg: u64, calls: u64| {
        format!(
            "{}/call ({calls})",
            format_duration_ms(avg, DurationStyle::Millis)
        )
    };
    match (turn, session) {
        (Some((turn_avg, turn_calls)), Some((avg, calls))) => Some(format!(
            "{} · {}",
            pair(turn_avg, turn_calls),
            pair(avg, calls)
        )),
        (Some((turn_avg, turn_calls)), None) => Some(pair(turn_avg, turn_calls)),
        (None, Some((avg, calls))) => Some(pair(avg, calls)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticking_style() {
        let f = |ms| format_duration_ms(ms, DurationStyle::Ticking);
        assert_eq!(f(0), "0.0s");
        assert_eq!(f(250), "0.2s");
        assert_eq!(f(12_400), "12.4s");
        assert_eq!(f(59_999), "59.9s");
        assert_eq!(f(60_000), "1m00s");
        assert_eq!(f(65_000), "1m05s");
        assert_eq!(f(125_000), "2m05s");
        assert_eq!(f(3_599_999), "59m59s");
        assert_eq!(f(3_600_000), "1h00m00s");
        assert_eq!(f(7_387_000), "2h03m07s");
    }

    #[test]
    fn precise_style() {
        let f = |secs| format_duration_secs(secs, DurationStyle::Precise);
        assert_eq!(f(0), "0s");
        assert_eq!(f(42), "42s");
        assert_eq!(f(59), "59s");
        assert_eq!(f(60), "1m00s");
        assert_eq!(f(98), "1m38s");
        assert_eq!(f(757), "12m37s");
        assert_eq!(f(3599), "59m59s");
        assert_eq!(f(3600), "1h00m");
        assert_eq!(f(7387), "2h03m");
    }

    #[test]
    fn coarse_style() {
        let f = |secs| format_duration_secs(secs, DurationStyle::Coarse);
        assert_eq!(f(0), "0m");
        assert_eq!(f(42), "0m");
        assert_eq!(f(98), "1m");
        assert_eq!(f(2280), "38m");
        assert_eq!(f(3599), "59m");
        assert_eq!(f(3600), "1h00m");
        assert_eq!(f(7500), "2h05m");
    }

    /// The per-call latency style: exact milliseconds below a second,
    /// seconds above it — a sub-second call must never read as `0s`.
    #[test]
    fn millis_style() {
        let f = |ms| format_duration_ms(ms, DurationStyle::Millis);
        assert_eq!(f(0), "0ms");
        assert_eq!(f(820), "820ms");
        assert_eq!(f(999), "999ms");
        assert_eq!(f(1000), "1.00s");
        assert_eq!(f(4230), "4.23s");
    }

    /// The latency row: latest turn first, session second, each figure
    /// followed by the number of calls it averages over; the row collapses
    /// to whichever scope has a measured call, and disappears when neither
    /// does.
    #[test]
    fn latency_row() {
        assert_eq!(
            format_latency(Some((1100, 3)), Some((820, 37))).as_deref(),
            Some("1.10s/call (3) · 820ms/call (37)")
        );
        assert_eq!(
            format_latency(Some((1100, 3)), None).as_deref(),
            Some("1.10s/call (3)")
        );
        assert_eq!(
            format_latency(None, Some((820, 37))).as_deref(),
            Some("820ms/call (37)")
        );
        assert_eq!(format_latency(None, None), None);
    }
}
