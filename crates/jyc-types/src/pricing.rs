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

use crate::config::{AppConfig, ModelPricing};

/// Tokens in one million — the denominator for every configured rate.
const TOKENS_PER_MILLION: f64 = 1_000_000.0;

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
pub fn compute_cost_split(
    pricing: &ModelPricing,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
) -> f64 {
    let uncached_input = input_tokens
        .saturating_sub(cache_read_tokens)
        .saturating_sub(cache_creation_tokens);

    let input_cost = uncached_input as f64 * pricing.input_per_million;
    let output_cost = output_tokens as f64 * pricing.output_per_million;
    let read_cost = cache_read_tokens as f64 * pricing.cache_hit_per_million;
    let write_cost = cache_creation_tokens as f64
        * pricing
            .cache_creation_per_million
            .unwrap_or(pricing.cache_hit_per_million);

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
}
