//! Per-provider prompt-cache hit extraction.
//!
//! Different vendors report prompt-cache hits under different field
//! names; this helper tries each known shape and returns the first
//! non-zero match. Returning `0` (not `Option`) lets the caller treat
//! "unknown" and "explicitly zero" identically — both mean the
//! provider didn't surface cache hits for this call.
//!
//! Anthropic is the **only** vendor that distinguishes cache reads
//! from cache writes: cache hits served from an existing entry vs.
//! tokens that *wrote* a new cache entry. Writes bill at ~1.25× the
//! input rate on Anthropic; reads are cheap. Use
//! [`extract_anthropic_cache_split`] to recover both buckets for
//! cost computation. [`extract_cache_hit_tokens`] returns the
//! per-vendor total across all reported cache buckets — it
//! tries each known shape in order and returns the first match
//! (Anthropic: read + creation; every other provider: the single
//! bucket). It's still useful for the single-rate cost path and
//! for legacy code paths that don't split reads from writes.
//!
//! ## Vendor coverage
//!
//! | Vendor        | Field(s)                                       | Location                          |
//! |---------------|------------------------------------------------|-----------------------------------|
//! | DeepSeek      | `prompt_cache_hit_tokens`                      | `usage` root                      |
//! | Kimi          | `cached_tokens`                                | `usage` root                      |
//! | OpenAI        | `cached_tokens`                                | `usage.prompt_tokens_details`     |
//! | 火山引擎      | `cached_tokens`                                | `usage.prompt_tokens_details`     |
//! | MiniMax       | `cached_tokens`                                | `usage.prompt_tokens_details`     |
//! | Anthropic     | `cache_read_input_tokens` (read)               | `usage` root                      |
//! |               | `cache_creation_input_tokens` (write)          | `usage` root                      |
//!
//! ## Search order (first non-zero match wins)
//!
//! 1. `usage.prompt_cache_hit_tokens` — DeepSeek (root)
//! 2. `usage.cached_tokens` — Kimi (root)
//! 3. `usage.prompt_tokens_details.cached_tokens` — OpenAI / 火山引擎 / MiniMax
//! 4. `usage.cache_read_input_tokens + usage.cache_creation_input_tokens`
//!    — Anthropic (sum of the two buckets)

use serde_json::Value;

/// Extract the per-call prompt-cache-hit token count from a provider's
/// `usage` JSON object. Returns `0` when none of the known fields are
/// present or all are explicitly zero (i.e. no cache hit was served).
///
/// See the module-level docs for the full vendor table.
pub fn extract_cache_hit_tokens(usage: &Value) -> u64 {
    // 1. DeepSeek: `prompt_cache_hit_tokens` at root.
    if let Some(v) = usage
        .get("prompt_cache_hit_tokens")
        .and_then(|v| v.as_u64())
        .filter(|&v| v > 0)
    {
        return v;
    }

    // 2. Kimi: `cached_tokens` at root.
    if let Some(v) = usage
        .get("cached_tokens")
        .and_then(|v| v.as_u64())
        .filter(|&v| v > 0)
    {
        return v;
    }

    // 3. OpenAI / 火山引擎 / MiniMax: `cached_tokens` under
    //    `prompt_tokens_details`.
    if let Some(v) = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .filter(|&v| v > 0)
    {
        return v;
    }

    // 4. Anthropic: sum of `cache_read_input_tokens` and
    //    `cache_creation_input_tokens` (both at root). Anthropic
    //    distinguishes reads from writes, but both represent tokens
    //    served from cache rather than re-billed as fresh input.
    let read = usage
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let creation = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let anthropic = read.saturating_add(creation);
    if anthropic > 0 {
        return anthropic;
    }

    0
}

/// Total prompt size for an Anthropic call, including cached tokens.
///
/// Anthropic's `input_tokens` counts **only the uncached** portion of the
/// prompt; `cache_read_input_tokens` and `cache_creation_input_tokens` are
/// reported separately and are *additive*, not a subset. Every other vendor
/// here does the opposite — OpenAI's `prompt_tokens` already contains
/// `cached_tokens`.
///
/// [`jyc_types::pricing::compute_cost_split`] expects the OpenAI shape (it
/// derives uncached input as `input - cache_read - cache_creation`), so
/// the Anthropic numbers have to be summed back into a total before they
/// reach it. Without this, a cache-heavy call reports less input than
/// cache hits, `saturating_sub` clamps the uncached remainder to zero,
/// and the genuinely-uncached tokens are billed at nothing.
pub fn anthropic_total_input_tokens(usage: &Value) -> u64 {
    let field = |name: &str| usage.get(name).and_then(|v| v.as_u64()).unwrap_or(0);

    field("input_tokens")
        .saturating_add(field("cache_read_input_tokens"))
        .saturating_add(field("cache_creation_input_tokens"))
}

/// Anthropic-specific: extract the read and write cache buckets
/// separately. Returns `(cache_read_tokens, cache_creation_tokens)`.
///
/// Both buckets default to `0` when the corresponding field is absent
/// or non-numeric. Every non-Anthropic provider has no second bucket
/// and is expected to use the `extract_cache_hit_tokens` (single
/// bucket) path; this helper exists specifically so Anthropic's two
/// fields can be billed at different rates by
/// [`jyc_types::pricing::compute_cost_split`].
pub fn extract_anthropic_cache_split(usage: &Value) -> (u64, u64) {
    let field = |name: &str| usage.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
    (
        field("cache_read_input_tokens"),
        field("cache_creation_input_tokens"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deepseek_root_field() {
        let usage = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_cache_hit_tokens": 800,
        });
        assert_eq!(extract_cache_hit_tokens(&usage), 800);
    }

    #[test]
    fn kimi_root_cached_tokens() {
        let usage = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "cached_tokens": 256,
        });
        assert_eq!(extract_cache_hit_tokens(&usage), 256);
    }

    #[test]
    fn openai_prompt_tokens_details_cached_tokens() {
        let usage = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": { "cached_tokens": 512 },
        });
        assert_eq!(extract_cache_hit_tokens(&usage), 512);
    }

    #[test]
    fn volcengine_same_shape_as_openai() {
        // 火山引擎 wire format matches OpenAI's `prompt_tokens_details.cached_tokens`.
        let usage = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": { "cached_tokens": 128 },
        });
        assert_eq!(extract_cache_hit_tokens(&usage), 128);
    }

    #[test]
    fn minimax_same_shape_as_openai() {
        // MiniMax (via OpenAI-compatible provider) uses
        // `usage.prompt_tokens_details.cached_tokens` — same branch as OpenAI
        // and 火山引擎, but named here to document the vendor.
        let usage = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_tokens_details": { "cached_tokens": 42 },
        });
        assert_eq!(extract_cache_hit_tokens(&usage), 42);
    }

    #[test]
    fn anthropic_sum_read_and_creation() {
        let usage = json!({
            "input_tokens": 1000,
            "output_tokens": 50,
            "cache_read_input_tokens": 700,
            "cache_creation_input_tokens": 100,
        });
        // Both buckets count toward "cache hit" for session accounting.
        assert_eq!(extract_cache_hit_tokens(&usage), 800);
    }

    #[test]
    fn anthropic_read_only_zero_creation() {
        let usage = json!({
            "input_tokens": 1000,
            "output_tokens": 50,
            "cache_read_input_tokens": 500,
        });
        assert_eq!(extract_cache_hit_tokens(&usage), 500);
    }

    #[test]
    fn anthropic_creation_only_zero_read() {
        let usage = json!({
            "input_tokens": 1000,
            "output_tokens": 50,
            "cache_creation_input_tokens": 300,
        });
        assert_eq!(extract_cache_hit_tokens(&usage), 300);
    }

    #[test]
    fn empty_usage_returns_zero() {
        let usage = json!({});
        assert_eq!(extract_cache_hit_tokens(&usage), 0);
    }

    #[test]
    fn missing_all_known_fields_returns_zero() {
        let usage = json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
        });
        assert_eq!(extract_cache_hit_tokens(&usage), 0);
    }

    #[test]
    fn zero_values_are_skipped() {
        // Explicit zeros at every known branch — fall through to 0.
        let usage = json!({
            "prompt_cache_hit_tokens": 0,
            "cached_tokens": 0,
            "prompt_tokens_details": { "cached_tokens": 0 },
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
        });
        assert_eq!(extract_cache_hit_tokens(&usage), 0);
    }

    #[test]
    fn non_numeric_values_are_skipped() {
        // Vendor sends the field but with a string value — skip it
        // rather than panic; treat as no cache hit.
        let usage = json!({
            "prompt_cache_hit_tokens": "n/a",
            "cached_tokens": "0",
        });
        assert_eq!(extract_cache_hit_tokens(&usage), 0);
    }

    #[test]
    fn first_non_zero_branch_wins() {
        // Realistic malformed usage that mentions multiple fields
        // (e.g. a proxy that re-emits upstream fields without
        // deduping). DeepSeek's root field should win; we don't sum
        // across branches.
        let usage = json!({
            "prompt_cache_hit_tokens": 100,
            "cached_tokens": 200,
            "prompt_tokens_details": { "cached_tokens": 300 },
        });
        assert_eq!(extract_cache_hit_tokens(&usage), 100);
    }

    /// Anthropic reports uncached input and the two cache buckets as
    /// disjoint, additive numbers — the total prompt is their sum.
    #[test]
    fn anthropic_total_input_sums_uncached_and_cache_buckets() {
        let usage = json!({
            "input_tokens": 2_800,
            "cache_read_input_tokens": 38_400,
            "cache_creation_input_tokens": 0,
        });
        assert_eq!(anthropic_total_input_tokens(&usage), 41_200);
    }

    /// A first call writes the cache instead of reading it; the written
    /// tokens still count toward the prompt total.
    #[test]
    fn anthropic_total_input_counts_cache_creation() {
        let usage = json!({
            "input_tokens": 500,
            "cache_creation_input_tokens": 10_000,
        });
        assert_eq!(anthropic_total_input_tokens(&usage), 10_500);
    }

    /// With caching off (every call before this feature landed) the total is
    /// just `input_tokens` — proving the change is a no-op without cache.
    #[test]
    fn anthropic_total_input_without_cache_is_unchanged() {
        let usage = json!({ "input_tokens": 1_234, "output_tokens": 99 });
        assert_eq!(anthropic_total_input_tokens(&usage), 1_234);
    }

    /// The bug this normalization exists to prevent, asserted end-to-end
    /// through the real cost function.
    ///
    /// Raw Anthropic numbers make `input < cache_hit`, so `compute_cost`'s
    /// `saturating_sub` clamps uncached input to zero and the 2,800 genuinely
    /// uncached tokens are billed at $0. Normalizing to the total restores
    /// them.
    #[test]
    fn anthropic_normalization_prevents_undercounting_cost() {
        let pricing = jyc_types::config::ModelPricing {
            input_per_million: 15.0,
            output_per_million: 75.0,
            cache_hit_per_million: 1.5,
            cache_creation_per_million: None,
            currency: None,
        };
        let usage = json!({
            "input_tokens": 2_800,
            "output_tokens": 1_830,
            "cache_read_input_tokens": 38_400,
        });
        let cache_hit = extract_cache_hit_tokens(&usage);

        // Raw (buggy): uncached clamps to 0, so input contributes nothing.
        let raw = jyc_types::pricing::compute_cost(&pricing, 2_800, 1_830, cache_hit);
        // Normalized: uncached = 41_200 - 38_400 = 2_800, billed correctly.
        let fixed = jyc_types::pricing::compute_cost(
            &pricing,
            anthropic_total_input_tokens(&usage),
            1_830,
            cache_hit,
        );

        assert!(
            fixed > raw,
            "normalized cost must exceed the undercounted one (raw={raw}, fixed={fixed})"
        );
        // 2800*15 + 1830*75 + 38400*1.5 = 236,850 / 1e6
        assert!(
            (fixed - 0.23685).abs() < 1e-9,
            "expected 0.23685, got {fixed}"
        );
        // The undercount is exactly the 2,800 uncached tokens at $15/M.
        assert!(
            ((fixed - raw) - 0.042).abs() < 1e-9,
            "expected a $0.042 undercount, got {}",
            fixed - raw
        );
    }

    /// Anthropic reports both buckets separately. The split helper
    /// returns both so `compute_cost_split` can bill each at its own
    /// rate.
    #[test]
    fn anthropic_cache_split_returns_both_buckets() {
        let usage = json!({
            "cache_read_input_tokens": 700,
            "cache_creation_input_tokens": 100,
        });
        assert_eq!(extract_anthropic_cache_split(&usage), (700, 100));
    }

    /// Cache reads without any writes (steady-state after the first
    /// call) — creation bucket is zero.
    #[test]
    fn anthropic_cache_split_read_only() {
        let usage = json!({
            "cache_read_input_tokens": 500,
        });
        assert_eq!(extract_anthropic_cache_split(&usage), (500, 0));
    }

    /// A first call writes the cache instead of reading it — read
    /// bucket is zero.
    #[test]
    fn anthropic_cache_split_write_only() {
        let usage = json!({
            "cache_creation_input_tokens": 300,
        });
        assert_eq!(extract_anthropic_cache_split(&usage), (0, 300));
    }

    /// Empty / non-Anthropic usage returns `(0, 0)` — the helper is
    /// safe to call on every provider's usage JSON regardless of
    /// shape.
    #[test]
    fn anthropic_cache_split_empty_usage() {
        assert_eq!(extract_anthropic_cache_split(&json!({})), (0, 0));
    }

    /// Non-numeric fields are skipped, not panicked on — same
    /// robustness guarantee as `extract_cache_hit_tokens`.
    #[test]
    fn anthropic_cache_split_skips_non_numeric() {
        let usage = json!({
            "cache_read_input_tokens": "n/a",
            "cache_creation_input_tokens": null,
        });
        assert_eq!(extract_anthropic_cache_split(&usage), (0, 0));
    }
}
