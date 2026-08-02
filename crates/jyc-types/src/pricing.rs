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
//! 3. **`input_tokens` and `cache_hit_tokens` come from the same `usage`
//!    payload**, so `input - cache_hit` is exactly that call's uncached
//!    input rather than an approximation across calls.

use crate::config::{AppConfig, ModelPricing};

/// Tokens in one million — the denominator for every configured rate.
const TOKENS_PER_MILLION: f64 = 1_000_000.0;

/// Compute the cost of a **single** LLM call.
///
/// `input_tokens` is the provider-reported prompt size, which *includes*
/// any tokens served from the prompt cache. The cached portion is split
/// out and billed at `cache_hit_per_million`, while the remainder is
/// billed at the full `input_per_million`:
///
/// ```text
/// (input - cache_hit) * input_rate + output * output_rate + cache_hit * cache_rate
/// ```
///
/// `saturating_sub` guards the subtraction: a provider that reports more
/// cache hits than total input (observed with some OpenAI-compatible
/// gateways that count them separately) yields `0` uncached input rather
/// than underflowing to a huge number and producing an absurd bill.
pub fn compute_cost(
    pricing: &ModelPricing,
    input_tokens: u64,
    output_tokens: u64,
    cache_hit_tokens: u64,
) -> f64 {
    let uncached_input = input_tokens.saturating_sub(cache_hit_tokens);

    let input_cost = uncached_input as f64 * pricing.input_per_million;
    let output_cost = output_tokens as f64 * pricing.output_per_million;
    let cache_cost = cache_hit_tokens as f64 * pricing.cache_hit_per_million;

    (input_cost + output_cost + cache_cost) / TOKENS_PER_MILLION
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
    let provider = config.agent.providers.get(provider_name)?;

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
            currency: None,
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
}
