//! Integration tests for `jyc_types::AgentConfig` defaults and
//! deserialization. Moved here from `jyc-agent` (see #477): the agent
//! service no longer defines its own `AgentConfig` view, it consumes
//! `jyc_types::AgentConfig` directly, so these tests belong with the
//! type they test.

mod max_iterations {
    use jyc_types::AgentConfig;

    #[test]
    fn default_is_500() {
        // Raised from 200 in v0.3.6 — in-loop summarization at the cycle
        // boundary now keeps the request size bounded regardless of
        // iteration count. See jyc-types default_max_iterations().
        let cfg = AgentConfig::default();
        assert_eq!(cfg.max_iterations, 500);
    }

    #[test]
    fn deserializes_from_toml_default() {
        // No max_iterations in TOML → default 500.
        let toml = r#"
            model = "anthropic/claude-3"
        "#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.max_iterations, 500);
    }

    #[test]
    fn deserializes_explicit_value() {
        let toml = r#"
            model = "anthropic/claude-3"
            max_iterations = 1000
        "#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.max_iterations, 1000);
    }
}

mod small_model {
    use jyc_types::AgentConfig;

    #[test]
    fn default_is_none() {
        let cfg = AgentConfig::default();
        assert!(cfg.small_model.is_none());
    }

    #[test]
    fn deserializes_when_absent() {
        // No small_model in TOML → None.
        let toml = r#"
            model = "anthropic/claude-3"
        "#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        assert!(cfg.small_model.is_none());
    }

    #[test]
    fn deserializes_when_present() {
        let toml = r#"
            model = "deepseek/deepseek-v4-pro"
            small_model = "deepseek/deepseek-v4-flash"
        "#;
        let cfg: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.small_model.as_deref(),
            Some("deepseek/deepseek-v4-flash"),
        );
    }
}
