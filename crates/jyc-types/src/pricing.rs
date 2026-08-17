//! Per-call LLM cost computation.
//!
//! Cost is computed **per LLM call** from that call's own usage payload,
//! not from session-level accumulated totals. This matters for three
//! reasons:
//!
//! 1. **Mid-round model switches** bill each call at the rate that was
//!    actually in effect for it.
//! 2. **Cancelled or failed rounds** keep the cost of the calls that did
//!    complete — nothing is lost because the round didn't finish.
//! 3. **`input_tokens` and the cache buckets come from the same `usage`
//!    payload**, so the math is exactly that call's breakdown rather
//!    than an approximation across calls.

use chrono::{DateTime, FixedOffset, NaiveTime, Utc};

use crate::config::{AppConfig, ModelPricing};

/// Tokens in one million — the denominator for every configured rate.
const TOKENS_PER_MILLION: f64 = 1_000_000.0;

/// The four rates that apply to one LLM call, resolved from
/// [`ModelPricing`] at a specific instant.
struct Rates {
    input: f64,
    output: f64,
    cache_hit: f64,
    cache_creation: Option<f64>,
}

/// Parse a `"HH:MM"` or `"HH:MM:SS"` time-of-day string.
fn parse_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
        .ok()
}

/// Resolve the rates in effect at `now`: the first `time_windows` entry
/// whose `[start, end)` contains the local time at `now` (interpreted in
/// `pricing.timezone`, a fixed UTC offset defaulting to UTC) wins;
/// otherwise the flat rates on `pricing` apply. A window with
/// `start > end` wraps past midnight. A window with an unparseable
/// start/end is skipped.
fn effective_rates(pricing: &ModelPricing, now: DateTime<Utc>) -> Rates {
    let offset = match pricing.timezone.as_deref() {
        Some(tz) => match tz.parse::<FixedOffset>() {
            Ok(off) => off,
            Err(e) => {
                tracing::warn!(timezone = tz, error = %e, "invalid pricing timezone, using UTC");
                FixedOffset::east_opt(0).expect("zero offset is always valid")
            }
        },
        None => FixedOffset::east_opt(0).expect("zero offset is always valid"),
    };
    let local = now.with_timezone(&offset).time();

    for w in &pricing.time_windows {
        let (Some(start), Some(end)) = (parse_time(&w.start), parse_time(&w.end)) else {
            continue;
        };
        // start-inclusive / end-exclusive; start > end wraps past midnight.
        let in_window = if start <= end {
            local >= start && local < end
        } else {
            local >= start || local < end
        };
        if in_window {
            return Rates {
                input: w.input_per_million,
                output: w.output_per_million,
                cache_hit: w.cache_hit_per_million,
                cache_creation: w.cache_creation_per_million,
            };
        }
    }

    Rates {
        input: pricing.input_per_million,
        output: pricing.output_per_million,
        cache_hit: pricing.cache_hit_per_million,
        cache_creation: pricing.cache_creation_per_million,
    }
}

/// Compute the cost of a **single** LLM call with read+write cache
/// tokens billed separately.
///
/// `input_tokens` is the provider-reported prompt size, which *includes*
/// any tokens served from the prompt cache. The cached portion is split
/// into read and write buckets, each billed at its own rate, while the
/// remainder is billed at the full `input_per_million`:
///
/// ```text
/// (input - cache_read - cache_creation) * input_rate
/// + output * output_rate
/// + cache_read * cache_hit_rate
/// + cache_creation * cache_creation_rate   (defaults to cache_hit_rate)
/// ```
///
/// `saturating_sub` guards the subtraction: a provider that reports more
/// cache hits than total input (observed with some OpenAI-compatible
/// gateways that count them separately, and Anthropic which counts
/// cached tokens in `cache_read_input_tokens`/`cache_creation_input_tokens`
/// *separately* from `input_tokens`) yields `0` uncached input rather
/// than underflowing to a huge number and producing an absurd bill.
///
/// `cache_creation_tokens` is `0` for every provider except Anthropic —
/// callers that only have a single bucket should pass it through
/// [`compute_cost`] (which forwards as `0` for the creation bucket).
///
/// When `pricing.time_windows` is set, the rates in effect at the moment
/// of the call are used: the first window containing the current local
/// time (in `pricing.timezone`) wins, falling back to the flat rates
/// outside all windows. The clock is read once per call at billing time
/// — a call straddling a window boundary bills at the end-of-call rate,
/// the same instant the ledger `ts` is stamped.
pub fn compute_cost_split(
    pricing: &ModelPricing,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
) -> f64 {
    compute_cost_split_at(
        pricing,
        Utc::now(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
    )
}

/// [`compute_cost_split`] at an explicit instant — lets tests exercise
/// time-of-day windows deterministically instead of reading the clock.
fn compute_cost_split_at(
    pricing: &ModelPricing,
    now: DateTime<Utc>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
) -> f64 {
    let r = effective_rates(pricing, now);
    let uncached_input = input_tokens
        .saturating_sub(cache_read_tokens)
        .saturating_sub(cache_creation_tokens);

    let input_cost = uncached_input as f64 * r.input;
    let output_cost = output_tokens as f64 * r.output;
    let read_cost = cache_read_tokens as f64 * r.cache_hit;
    let write_cost = cache_creation_tokens as f64 * r.cache_creation.unwrap_or(r.cache_hit);

    (input_cost + output_cost + read_cost + write_cost) / TOKENS_PER_MILLION
}

/// Compute the cost of a **single** LLM call.
///
/// Convenience wrapper around [`compute_cost_split`] for callers that
/// have a single cache bucket — non-Anthropic providers (OpenAI /
/// DeepSeek / Kimi / 火山引擎 / MiniMax) and Anthropic users who have
/// not set `cache_creation_per_million`. `cache_hit_tokens` is treated
/// as the **read** bucket; the write bucket is `0`.
///
/// See [`compute_cost_split`] for the full formula and the reasoning
/// behind per-call computation.
pub fn compute_cost(
    pricing: &ModelPricing,
    input_tokens: u64,
    output_tokens: u64,
    cache_hit_tokens: u64,
) -> f64 {
    compute_cost_split(pricing, input_tokens, output_tokens, cache_hit_tokens, 0)
}

/// Resolve the effective pricing for a `"provider/model"` identifier.
///
/// Resolution order (first match wins):
/// 1. Model-level `pricing` under `[agent.providers.<p>.models.<m>]`
/// 2. Provider-level `pricing` under `[agent.providers.<p>]`
/// 3. `None` — no pricing configured, so no cost is tracked
///
/// Mirrors how `context_window` resolves in `jyc-agent::service`, so a
/// user who configures one the same way gets the other for free.
/// Returns `None` for a malformed identifier (no `/` separator) or an
/// unknown provider.
pub fn lookup_pricing(config: &AppConfig, model: &str) -> Option<ModelPricing> {
    let (provider_name, model_id) = model.split_once('/')?;
    let provider = config.ai.providers.get(provider_name)?;

    provider
        .models
        .get(model_id)
        .and_then(|m| m.pricing.clone())
        .or_else(|| provider.pricing.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing(input: f64, output: f64, cache: f64) -> ModelPricing {
        ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cache_hit_per_million: cache,
            cache_creation_per_million: None,
            currency: None,
            time_windows: Vec::new(),
            timezone: None,
        }
    }

    /// Build a pricing with a separate cache-creation (write) rate.
    /// Used by the split-pricing tests below.
    fn pricing_split(input: f64, output: f64, cache_read: f64, cache_create: f64) -> ModelPricing {
        ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cache_hit_per_million: cache_read,
            cache_creation_per_million: Some(cache_create),
            currency: None,
            time_windows: Vec::new(),
            timezone: None,
        }
    }

    /// The example from the feature request: $3/M input, $4/M output,
    /// $1/M cache hits. With 1M input of which 0 cached and 1M output:
    /// 3.0 + 4.0 = 7.0.
    #[test]
    fn computes_documented_example() {
        let p = pricing(3.0, 4.0, 1.0);
        let cost = compute_cost(&p, 1_000_000, 1_000_000, 0);
        assert!((cost - 7.0).abs() < f64::EPSILON, "got {cost}");
    }

    /// Cache hits are billed at the cache rate and excluded from the
    /// input rate — not billed twice.
    #[test]
    fn cache_hits_billed_at_cache_rate_only() {
        let p = pricing(3.0, 4.0, 1.0);
        // 1M input of which 500K cached → 500K @ $3/M + 500K @ $1/M = 1.5 + 0.5
        let cost = compute_cost(&p, 1_000_000, 0, 500_000);
        assert!((cost - 2.0).abs() < 1e-9, "got {cost}");
    }

    /// Each of the three rates contributes independently.
    #[test]
    fn each_rate_contributes_independently() {
        // Only input.
        let cost = compute_cost(&pricing(3.0, 0.0, 0.0), 1_000_000, 0, 0);
        assert!((cost - 3.0).abs() < 1e-9);
        // Only output.
        let cost = compute_cost(&pricing(0.0, 4.0, 0.0), 0, 1_000_000, 0);
        assert!((cost - 4.0).abs() < 1e-9);
        // Only cache hits (input must cover them).
        let cost = compute_cost(&pricing(0.0, 0.0, 1.0), 1_000_000, 0, 1_000_000);
        assert!((cost - 1.0).abs() < 1e-9);
    }

    /// A CNY-denominated example from the feature request: 3 yuan/M in,
    /// 4 yuan/M out, 0.5 yuan/M cache.
    #[test]
    fn computes_cny_example() {
        let p = pricing(3.0, 4.0, 0.5);
        // 2M input (1M cached) + 1M output
        // = 1M @ 3 + 1M @ 4 + 1M @ 0.5 = 7.5
        let cost = compute_cost(&p, 2_000_000, 1_000_000, 1_000_000);
        assert!((cost - 7.5).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn zero_tokens_is_zero_cost() {
        assert_eq!(compute_cost(&pricing(3.0, 4.0, 1.0), 0, 0, 0), 0.0);
    }

    /// Defensive: cache_hit > input must not underflow. Uncached input
    /// clamps to 0, and only the cache rate applies.
    #[test]
    fn cache_hit_exceeding_input_clamps_to_zero() {
        let p = pricing(3.0, 4.0, 1.0);
        let cost = compute_cost(&p, 100, 0, 1_000_000);
        // uncached = 0, so cost is purely the cache-hit component.
        assert!((cost - 1.0).abs() < 1e-9, "got {cost}");
        assert!(cost > 0.0, "must not underflow to a huge or negative value");
    }

    /// Realistic small call — verifies no precision surprises at the
    /// magnitudes actually seen in practice.
    #[test]
    fn realistic_small_call() {
        // Claude Opus-ish rates: $15/M in, $75/M out, $1.50/M cache.
        let p = pricing(15.0, 75.0, 1.5);
        // 41,200 input of which 38,400 cached; 1,830 output.
        // uncached = 2,800 → 2800*15 + 1830*75 + 38400*1.5 = 42000 + 137250 + 57600
        // = 236,850 / 1e6 = 0.23685
        let cost = compute_cost(&p, 41_200, 1_830, 38_400);
        assert!((cost - 0.23685).abs() < 1e-9, "got {cost}");
    }

    /// Zero rates (unconfigured) produce zero cost, never NaN.
    #[test]
    fn zero_rates_produce_zero_not_nan() {
        let cost = compute_cost(&pricing(0.0, 0.0, 0.0), 999_999, 999_999, 999);
        assert_eq!(cost, 0.0);
        assert!(cost.is_finite());
    }

    mod lookup {
        use super::*;

        fn config_from(toml: &str) -> AppConfig {
            toml::from_str(toml).unwrap()
        }

        const BOTH_LEVELS: &str = r#"
            [agent]
            [agent.providers.anthropic]
            type = "anthropic"
            pricing = { input_per_million = 3.0, output_per_million = 15.0 }
            [agent.providers.anthropic.models."claude-opus-4-7"]
            pricing = { input_per_million = 15.0, output_per_million = 75.0, cache_hit_per_million = 1.5 }
        "#;

        /// Model-level pricing wins over provider-level.
        #[test]
        fn model_level_overrides_provider() {
            let cfg = config_from(BOTH_LEVELS);
            let p = lookup_pricing(&cfg, "anthropic/claude-opus-4-7").unwrap();
            assert_eq!(p.input_per_million, 15.0);
            assert_eq!(p.cache_hit_per_million, 1.5);
        }

        /// A model without its own pricing falls back to the provider's.
        #[test]
        fn falls_back_to_provider_pricing() {
            let cfg = config_from(BOTH_LEVELS);
            let p = lookup_pricing(&cfg, "anthropic/claude-haiku-9").unwrap();
            assert_eq!(p.input_per_million, 3.0);
        }

        /// No pricing configured anywhere → None (cost tracking off).
        #[test]
        fn unpriced_provider_returns_none() {
            let cfg = config_from(
                r#"
                [agent]
                [agent.providers.ollama]
                type = "openai-compatible"
            "#,
            );
            assert!(lookup_pricing(&cfg, "ollama/llama4").is_none());
        }

        #[test]
        fn unknown_provider_returns_none() {
            let cfg = config_from(BOTH_LEVELS);
            assert!(lookup_pricing(&cfg, "nonexistent/model").is_none());
        }

        /// A bare model name with no `provider/` prefix is not resolvable.
        #[test]
        fn malformed_identifier_returns_none() {
            let cfg = config_from(BOTH_LEVELS);
            assert!(lookup_pricing(&cfg, "claude-opus-4-7").is_none());
            assert!(lookup_pricing(&cfg, "").is_none());
        }
    }

    mod split_pricing {
        //! Tests for `compute_cost_split` — the two-bucket cache
        //! formula used by Anthropic (reads vs. writes) and other
        //! providers that distinguish cache write rates from cache
        //! read rates.
        use super::*;

        /// Backwards-compat: with `cache_creation_per_million` unset,
        /// the split function must produce the same numbers as the
        /// legacy `compute_cost` — proves existing configs and ledger
        /// entries are not re-priced after upgrade.
        #[test]
        fn split_matches_legacy_when_creation_unset_and_zero() {
            let p = pricing(15.0, 75.0, 1.5);
            let legacy = compute_cost(&p, 41_200, 1_830, 38_400);
            let split = compute_cost_split(&p, 41_200, 1_830, 38_400, 0);
            assert!(
                (legacy - split).abs() < 1e-12,
                "legacy={legacy} split={split}"
            );
        }

        /// `compute_cost` is exactly `compute_cost_split` with a zero
        /// creation bucket — the wrapper must produce identical output
        /// for the same inputs.
        #[test]
        fn compute_cost_equals_split_with_zero_creation() {
            let p = pricing(3.0, 4.0, 1.0);
            assert_eq!(
                compute_cost(&p, 1_000_000, 1_000_000, 500_000),
                compute_cost_split(&p, 1_000_000, 1_000_000, 500_000, 0)
            );
        }

        /// Anthropic Opus-ish rates: $15/M in, $75/M out, $1.50/M cache
        /// **read**, $18.75/M cache **write** (1.25× input). Mix of
        /// reads and writes bills each at its own rate.
        #[test]
        fn cache_creation_rate_used_when_set() {
            let p = pricing_split(15.0, 75.0, 1.5, 18.75);
            // 41,200 input, of which 38,400 read + 1,000 write. 1,830 output.
            // uncached = 1,800 → 1800*15 = 27000
            // read  = 38400 * 1.5  = 57600
            // write = 1000  * 18.75 = 18750
            // out   = 1830 * 75  = 137250
            // sum   = 240600 / 1e6 = 0.2406
            let cost = compute_cost_split(&p, 41_200, 1_830, 38_400, 1_000);
            assert!((cost - 0.2406).abs() < 1e-9, "got {cost}");
        }

        /// Writes are billed at the **read** rate when no separate
        /// write rate is configured — existing Anthropic users without
        /// the new field see no behavior change.
        #[test]
        fn cache_creation_falls_back_to_cache_read_rate() {
            let p = pricing(15.0, 75.0, 1.5);
            // 1100 read + 1000 write, both at 1.5/M
            let cost = compute_cost_split(&p, 3_000, 0, 1_100, 1_000);
            // uncached = 900 → 900*15 = 13500
            // read = 1100 * 1.5 = 1650
            // write = 1000 * 1.5 = 1500  (falls back)
            // sum = 16650 / 1e6 = 0.01665
            assert!((cost - 0.01665).abs() < 1e-9, "got {cost}");
        }

        /// `saturating_sub` must clamp the uncached input to zero when
        /// both cache buckets together exceed `input_tokens` (Anthropic
        /// reports them as disjoint additive numbers, so the math is
        /// safe; this test guards against a regression).
        #[test]
        fn split_under_saturating_sub_guard() {
            let p = pricing_split(15.0, 75.0, 1.5, 18.75);
            // input=100, but read=1000 and write=2000 → uncached=0.
            let cost = compute_cost_split(&p, 100, 0, 1_000, 2_000);
            // uncached=0; read=1000*1.5 + write=2000*18.75 = 1500 + 37500 = 39000 / 1e6
            assert!((cost - 0.039).abs() < 1e-9, "got {cost}");
            assert!(cost.is_finite());
        }

        /// With zero rates and zero tokens, the split function
        /// produces zero — never NaN.
        #[test]
        fn split_zero_rates_zero_tokens_is_zero() {
            let p = ModelPricing {
                input_per_million: 0.0,
                output_per_million: 0.0,
                cache_hit_per_million: 0.0,
                cache_creation_per_million: None,
                currency: None,
                time_windows: Vec::new(),
                timezone: None,
            };
            assert_eq!(compute_cost_split(&p, 0, 0, 0, 0), 0.0);
        }

        /// Writes contribute independently of reads — proves no
        /// double-counting when both buckets are non-zero.
        #[test]
        fn split_read_and_write_contribute_independently() {
            let p = pricing_split(0.0, 0.0, 1.0, 5.0);
            // Zero input/output: only cache buckets matter.
            // need input >= read+write to avoid saturating_sub clamp.
            let cost = compute_cost_split(&p, 100, 0, 30, 20);
            // 30*1 + 20*5 = 30 + 100 = 130 / 1e6
            assert!((cost - 130e-6).abs() < 1e-12, "got {cost}");
        }
    }

    mod time_windows {
        //! Tests for time-of-day pricing windows.
        use super::*;
        use crate::config::TimeWindowPricing;
        use chrono::TimeZone;

        fn utc(h: u32, m: u32) -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 8, 17, h, m, 0).unwrap()
        }

        fn window(start: &str, end: &str, input: f64, output: f64) -> TimeWindowPricing {
            TimeWindowPricing {
                start: start.to_string(),
                end: end.to_string(),
                input_per_million: input,
                output_per_million: output,
                cache_hit_per_million: 0.0,
                cache_creation_per_million: None,
            }
        }

        /// DeepSeek-style schedule (UTC here for determinism): flat
        /// standard rates, a cheaper off-peak window 00:30–08:30, and a
        /// discounted evening window 16:30–00:30 that wraps midnight.
        fn deepseek_pricing() -> ModelPricing {
            ModelPricing {
                input_per_million: 2.0,
                output_per_million: 8.0,
                cache_hit_per_million: 0.5,
                cache_creation_per_million: None,
                currency: Some("CNY".to_string()),
                time_windows: vec![
                    window("00:30", "08:30", 1.0, 4.0),
                    window("16:30", "00:30", 1.5, 6.0),
                ],
                timezone: None,
            }
        }

        /// A call inside a window bills at that window's rates.
        #[test]
        fn window_rates_apply_inside_the_window() {
            let p = deepseek_pricing();
            let r = effective_rates(&p, utc(3, 0));
            assert_eq!(r.input, 1.0);
            assert_eq!(r.output, 4.0);
            // And through the full cost path: 1M input at ¥1/M = ¥1.0.
            let cost = compute_cost_split_at(&p, utc(3, 0), 1_000_000, 0, 0, 0);
            assert!((cost - 1.0).abs() < 1e-9, "got {cost}");
        }

        /// Outside every window the flat rates apply — no window matches
        /// in the middle of the day.
        #[test]
        fn flat_rates_apply_outside_all_windows() {
            let p = deepseek_pricing();
            let r = effective_rates(&p, utc(12, 0));
            assert_eq!(r.input, 2.0);
            assert_eq!(r.output, 8.0);
            let cost = compute_cost_split_at(&p, utc(12, 0), 1_000_000, 0, 0, 0);
            assert!((cost - 2.0).abs() < 1e-9, "got {cost}");
        }

        /// A window with `start > end` wraps past midnight: both late
        /// evening and the early hours before its end are in it.
        #[test]
        fn midnight_wrapping_window_covers_both_sides() {
            let p = deepseek_pricing();
            // 20:00 and 00:15 are both inside [16:30, 00:30).
            assert_eq!(effective_rates(&p, utc(20, 0)).input, 1.5);
            assert_eq!(effective_rates(&p, utc(0, 15)).input, 1.5);
            // 00:30 is the wrap window's end (exclusive) → off-peak window
            // [00:30, 08:30) takes over.
            assert_eq!(effective_rates(&p, utc(0, 30)).input, 1.0);
        }

        /// Boundaries are start-inclusive / end-exclusive.
        #[test]
        fn boundaries_are_start_inclusive_end_exclusive() {
            let p = deepseek_pricing();
            // 00:30 is the off-peak start → in.
            assert_eq!(effective_rates(&p, utc(0, 30)).input, 1.0);
            // 08:30 is the off-peak end → out (flat standard applies).
            assert_eq!(effective_rates(&p, utc(8, 30)).input, 2.0);
        }

        /// `timezone` shifts the window clock: a Beijing-time window
        /// 00:30–08:30 is 16:30–00:30 UTC, so 20:00 UTC (04:00 Beijing)
        /// is in the window but 10:00 UTC (18:00 Beijing) is not.
        #[test]
        fn timezone_offset_shifts_windows() {
            let mut p = deepseek_pricing();
            p.timezone = Some("+08:00".to_string());
            // Window times are now Beijing local.
            p.time_windows = vec![window("00:30", "08:30", 1.0, 4.0)];
            assert_eq!(effective_rates(&p, utc(20, 0)).input, 1.0);
            assert_eq!(effective_rates(&p, utc(10, 0)).input, 2.0);
        }

        /// The first matching window wins when windows overlap.
        #[test]
        fn first_matching_window_wins() {
            let mut p = deepseek_pricing();
            p.time_windows = vec![
                window("00:00", "12:00", 1.0, 1.0),
                window("06:00", "18:00", 5.0, 5.0),
            ];
            assert_eq!(effective_rates(&p, utc(8, 0)).input, 1.0);
        }

        /// An unparseable window time skips that window; an unparseable
        /// `timezone` falls back to UTC rather than failing the call.
        #[test]
        fn unparseable_window_or_timezone_degrades_to_flat() {
            let mut p = deepseek_pricing();
            p.time_windows = vec![
                TimeWindowPricing {
                    start: "bogus".to_string(),
                    end: "08:30".to_string(),
                    input_per_million: 1.0,
                    output_per_million: 4.0,
                    cache_hit_per_million: 0.0,
                    cache_creation_per_million: None,
                },
                window("00:30", "08:30", 1.0, 4.0),
            ];
            // The bogus window is skipped; the valid one still matches.
            assert_eq!(effective_rates(&p, utc(3, 0)).input, 1.0);
            // Bad timezone → UTC, so the UTC-time window still matches.
            p.timezone = Some("not-a-timezone".to_string());
            assert_eq!(effective_rates(&p, utc(3, 0)).input, 1.0);
        }

        /// A window's cache buckets bill at the window's cache rates.
        #[test]
        fn window_supplies_cache_and_creation_rates() {
            let p = ModelPricing {
                input_per_million: 2.0,
                output_per_million: 8.0,
                cache_hit_per_million: 0.5,
                cache_creation_per_million: None,
                currency: None,
                time_windows: vec![TimeWindowPricing {
                    start: "00:00".to_string(),
                    end: "12:00".to_string(),
                    input_per_million: 2.0,
                    output_per_million: 8.0,
                    cache_hit_per_million: 0.25,
                    cache_creation_per_million: Some(0.5),
                }],
                timezone: None,
            };
            // 100 input, all cached-read → 100 * 0.25 / 1e6.
            let read = compute_cost_split_at(&p, utc(6, 0), 100, 0, 100, 0);
            assert!((read - 25e-6).abs() < 1e-12, "got {read}");
            // 100 input, all cache-creation → 100 * 0.5 / 1e6.
            let write = compute_cost_split_at(&p, utc(6, 0), 100, 0, 0, 100);
            assert!((write - 50e-6).abs() < 1e-12, "got {write}");
        }

        /// `"HH:MM"` and `"HH:MM:SS"` both parse.
        #[test]
        fn parse_time_accepts_both_formats() {
            assert_eq!(
                parse_time("00:30"),
                Some(NaiveTime::from_hms_opt(0, 30, 0).unwrap())
            );
            assert_eq!(
                parse_time("16:30:00"),
                Some(NaiveTime::from_hms_opt(16, 30, 0).unwrap())
            );
            assert_eq!(parse_time("nope"), None);
        }
    }
}
