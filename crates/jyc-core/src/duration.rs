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
    }
}

/// Format a duration given in whole seconds per `style`.
pub fn format_duration_secs(secs: u64, style: DurationStyle) -> String {
    format_duration_ms(secs.saturating_mul(1000), style)
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
}
