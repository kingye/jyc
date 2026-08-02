//! Per-provider prompt-cache hit extraction.
//!
//! Different vendors report prompt-cache hits under different field
//! names; this helper tries each known shape and returns the first
//! non-zero match. Returning `0` (not `Option`) lets the caller treat
//! "unknown" and "explicitly zero" identically — both mean the
//! provider didn't surface cache hits for this call.
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
//! | Anthropic     | `cache_read_input_tokens + cache_creation_input_tokens` | `usage` root             |
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
}
