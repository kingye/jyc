//! Guards the `pricing` example in `config.example.toml`.
//!
//! The TOML below mirrors that file's `[agent.providers.anthropic]` block
//! (comment markers stripped; field shape preserved). If someone edits the
//! example into something that no longer parses or resolves, this fails —
//! documentation that silently stops working is worse than none.
//!
//! Uses the legacy `api_key_env` form on purpose: this test exercises the
//! pricing resolution path and is indifferent to which credential field
//! the provider carries.
//!
//! Inlined rather than read from disk so the test has no filesystem
//! dependency (see AGENTS.md test-isolation rules).

// Verbatim from config.example.toml, comment markers stripped.
const EXAMPLE_PRICING: &str = r#"
[agent.providers.anthropic]
type = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key_env = "ANTHROPIC_API_KEY"
pricing = { input_per_million = 3.0, output_per_million = 15.0, cache_hit_per_million = 0.3, currency = "USD" }

[agent.providers.anthropic.models."claude-opus-4-7"]
supports_images = true
pricing = { input_per_million = 15.0, output_per_million = 75.0, cache_hit_per_million = 1.5, currency = "USD" }
"#;

#[test]
fn example_config_pricing_parses_and_resolves() {
    let cfg: jyc_types::AppConfig = toml::from_str(EXAMPLE_PRICING).unwrap();

    // Model-level rates override the provider default.
    let model = jyc_types::pricing::lookup_pricing(&cfg, "anthropic/claude-opus-4-7")
        .expect("model pricing must resolve");
    assert_eq!(model.input_per_million, 15.0);
    assert_eq!(model.output_per_million, 75.0);
    assert_eq!(model.cache_hit_per_million, 1.5);
    // The example declares USD explicitly, because DEFAULT_CURRENCY is CNY
    // and an Anthropic provider must not be relabelled as yuan.
    assert_eq!(model.currency_label(), "USD");

    // A model not listed under the provider inherits the provider rates.
    let fallback = jyc_types::pricing::lookup_pricing(&cfg, "anthropic/claude-haiku-9")
        .expect("provider pricing must resolve as fallback");
    assert_eq!(fallback.input_per_million, 3.0);
    assert_eq!(fallback.cache_hit_per_million, 0.3);
}

/// The formula documented in config.example.toml, checked against the
/// example's own rates at realistic token volumes.
#[test]
fn documented_formula_matches_implementation() {
    let cfg: jyc_types::AppConfig = toml::from_str(EXAMPLE_PRICING).unwrap();
    let p = jyc_types::pricing::lookup_pricing(&cfg, "anthropic/claude-opus-4-7").unwrap();

    // 41,200 prompt tokens of which 38,400 were cache hits; 1,830 output.
    let cost = jyc_types::pricing::compute_cost(&p, 41_200, 1_830, 38_400);

    // (input - cache_hit) * in + output * out + cache_hit * cache, per 1M.
    let expected = (2_800.0 * 15.0 + 1_830.0 * 75.0 + 38_400.0 * 1.5) / 1_000_000.0;
    assert!((cost - expected).abs() < 1e-12, "{cost} vs {expected}");

    // Cache hits must be cheaper than billing them as fresh input —
    // the whole reason the third rate exists.
    let as_fresh_input = jyc_types::pricing::compute_cost(&p, 41_200, 1_830, 0);
    assert!(cost < as_fresh_input, "cache hits must reduce the bill");
}
