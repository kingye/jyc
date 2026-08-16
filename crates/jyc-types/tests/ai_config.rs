//! Integration tests for `jyc_types::AiConfig` defaults and
//! deserialization. Moved here from `jyc-agent` (see #477): the agent
//! service no longer defines its own `AiConfig` view, it consumes
//! `jyc_types::AiConfig` directly, so these tests belong with the
//! type they test.

mod max_iterations {
    use jyc_types::AiConfig;

    #[test]
    fn default_is_500() {
        // Raised from 200 in v0.3.6 — in-loop summarization at the cycle
        // boundary now keeps the request size bounded regardless of
        // iteration count. See jyc-types default_max_iterations().
        let cfg = AiConfig::default();
        assert_eq!(cfg.max_iterations, 500);
    }

    #[test]
    fn deserializes_from_toml_default() {
        // No max_iterations in TOML → default 500.
        let toml = r#"
            model = "anthropic/claude-3"
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.max_iterations, 500);
    }

    #[test]
    fn deserializes_explicit_value() {
        let toml = r#"
            model = "anthropic/claude-3"
            max_iterations = 1000
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.max_iterations, 1000);
    }
}

mod small_model {
    use jyc_types::AiConfig;

    #[test]
    fn default_is_none() {
        let cfg = AiConfig::default();
        assert!(cfg.small_model.is_none());
    }

    #[test]
    fn deserializes_when_absent() {
        // No small_model in TOML → None.
        let toml = r#"
            model = "anthropic/claude-3"
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        assert!(cfg.small_model.is_none());
    }

    #[test]
    fn deserializes_when_present() {
        let toml = r#"
            model = "deepseek/deepseek-v4-pro"
            small_model = "deepseek/deepseek-v4-flash"
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.small_model.as_deref(),
            Some("deepseek/deepseek-v4-flash"),
        );
    }
}

mod pricing {
    use jyc_types::AiConfig;

    /// Provider-level pricing applies as the default for its models.
    #[test]
    fn provider_level_pricing_parses() {
        let toml = r#"
            model = "anthropic/claude-opus-4-7"

            [providers.anthropic]
            type = "anthropic"
            pricing = { input_per_million = 3.0, output_per_million = 15.0, cache_hit_per_million = 0.3 }
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        let p = cfg.providers["anthropic"].pricing.as_ref().unwrap();
        assert_eq!(p.input_per_million, 3.0);
        assert_eq!(p.output_per_million, 15.0);
        assert_eq!(p.cache_hit_per_million, 0.3);
    }

    /// `currency` defaults to CNY when omitted — jyc's primary
    /// deployments price in yuan, and no conversion is ever performed.
    #[test]
    fn currency_defaults_to_cny() {
        let toml = r#"
            [providers.siliconflow]
            type = "openai-compatible"
            pricing = { input_per_million = 3.0, output_per_million = 4.0 }
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        let p = cfg.providers["siliconflow"].pricing.as_ref().unwrap();
        assert_eq!(p.currency_label(), jyc_types::DEFAULT_CURRENCY);
        assert_eq!(p.currency_label(), "CNY");
    }

    /// A USD provider must declare `currency` explicitly, since the
    /// default is now CNY. Guards against a USD-billed provider being
    /// silently relabelled as yuan.
    #[test]
    fn explicit_currency_is_preserved() {
        let toml = r#"
            [providers.anthropic]
            type = "anthropic"
            pricing = { input_per_million = 3.0, output_per_million = 15.0, cache_hit_per_million = 0.3, currency = "USD" }
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        let p = cfg.providers["anthropic"].pricing.as_ref().unwrap();
        assert_eq!(p.currency_label(), "USD");
        assert_eq!(p.cache_hit_per_million, 0.3);
    }

    /// `cache_hit_per_million` defaults to 0.0 when omitted, so providers
    /// that don't surface cache hits need not configure it.
    #[test]
    fn cache_hit_rate_defaults_to_zero() {
        let toml = r#"
            [providers.deepseek]
            type = "openai-compatible"
            pricing = { input_per_million = 1.0, output_per_million = 2.0 }
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        let p = cfg.providers["deepseek"].pricing.as_ref().unwrap();
        assert_eq!(p.cache_hit_per_million, 0.0);
    }

    /// `cache_creation_per_million` is optional — non-Anthropic
    /// providers (or Anthropic users who don't want a separate write
    /// rate) omit it and `compute_cost_split` falls back to
    /// `cache_hit_per_million`.
    #[test]
    fn cache_creation_rate_is_optional() {
        let toml = r#"
            [providers.anthropic]
            type = "anthropic"
            pricing = { input_per_million = 15.0, output_per_million = 75.0, cache_hit_per_million = 1.5 }
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        let p = cfg.providers["anthropic"].pricing.as_ref().unwrap();
        assert!(p.cache_creation_per_million.is_none());
    }

    /// Anthropic pricing with a separate write rate parses both
    /// fields. Setting `cache_creation_per_million` lets
    /// `compute_cost_split` bill cache writes at their premium rate
    /// (typically 1.25× input).
    #[test]
    fn cache_creation_rate_parses_when_set() {
        let toml = r#"
            [providers.anthropic]
            type = "anthropic"
            pricing = { input_per_million = 15.0, output_per_million = 75.0, cache_hit_per_million = 1.5, cache_creation_per_million = 18.75, currency = "USD" }
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        let p = cfg.providers["anthropic"].pricing.as_ref().unwrap();
        assert_eq!(p.cache_creation_per_million, Some(18.75));
        assert_eq!(p.cache_hit_per_million, 1.5);
    }

    /// `cache_creation_per_million` round-trips through TOML and is
    /// exposed on model-level pricing overrides.
    #[test]
    fn cache_creation_rate_works_at_model_level() {
        let toml = r#"
            [providers.anthropic]
            type = "anthropic"

            [providers.anthropic.models."claude-opus-4-7"]
            pricing = { input_per_million = 15.0, output_per_million = 75.0, cache_hit_per_million = 1.5, cache_creation_per_million = 18.75 }
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        let model = &cfg.providers["anthropic"].models["claude-opus-4-7"];
        let p = model.pricing.as_ref().unwrap();
        assert_eq!(p.cache_creation_per_million, Some(18.75));
    }

    /// Model-level pricing coexists with provider-level; both are parsed
    /// and the resolution order is exercised in `pricing::lookup_pricing`.
    #[test]
    fn model_level_pricing_parses_alongside_provider() {
        let toml = r#"
            [providers.anthropic]
            type = "anthropic"
            pricing = { input_per_million = 3.0, output_per_million = 15.0 }

            [providers.anthropic.models."claude-opus-4-7"]
            pricing = { input_per_million = 15.0, output_per_million = 75.0, cache_hit_per_million = 1.5 }
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        let provider = &cfg.providers["anthropic"];
        assert_eq!(provider.pricing.as_ref().unwrap().input_per_million, 3.0);
        let model = &provider.models["claude-opus-4-7"];
        assert_eq!(model.pricing.as_ref().unwrap().input_per_million, 15.0);
        assert_eq!(model.pricing.as_ref().unwrap().cache_hit_per_million, 1.5);
    }

    /// Pricing is optional — configs without it still parse.
    #[test]
    fn pricing_absent_is_none() {
        let toml = r#"
            [providers.anthropic]
            type = "anthropic"
        "#;
        let cfg: AiConfig = toml::from_str(toml).unwrap();
        assert!(cfg.providers["anthropic"].pricing.is_none());
    }
}
