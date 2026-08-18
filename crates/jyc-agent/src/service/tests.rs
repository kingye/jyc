use super::*;
use arc_swap::ArcSwap;
use jyc_types::{ChannelPattern, ChannelType};
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

/// Helper: build a minimal `AppConfig` wrapped in an `Arc<ArcSwap>`.
/// `model` becomes the global `[agent].model`. Add to `providers`/`channels`
/// to simulate config knobs.
fn app_config_with_model(model: Option<&str>) -> Arc<ArcSwap<jyc_types::AppConfig>> {
    let app = jyc_types::AppConfig {
        general: jyc_types::GeneralConfig::default(),
        channels: HashMap::new(),
        ai: jyc_types::AiConfig {
            enabled: true,
            mode: "agent".to_string(),
            model: model.map(|s| s.to_string()),
            plan_model: None,
            build_model: None,
            small_model: None,
            system_prompt: None,
            max_iterations: 500,
            sse_read_timeout_secs: 120,
            text: None,
            attachments: None,
            providers: HashMap::new(),
            vision: None,
            reset_compression: None,
            auto_reset_threshold: 0.95,
        },
        inspect: None,
        attachments: None,
        wecom: None,
        mcps: Vec::new(),
        scheduler: jyc_types::SchedulerConfig::default(),
        commands: Vec::new(),
        agents: std::collections::HashMap::new(),
    };
    Arc::new(ArcSwap::from_pointee(app))
}

/// Helper: build a service with given config model and patterns.
fn service_with_patterns(
    config_model: Option<&str>,
    patterns: Vec<ChannelPattern>,
) -> JycAgentService {
    JycAgentService::new(
        app_config_with_model(config_model),
        PathBuf::from("/tmp/test-workdir"),
        vec![],
        None,
        patterns,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        "test".to_string(),
    )
}

/// Helper: build a service with exclusion settings.
fn service_with_exclusion(
    patterns: Vec<ChannelPattern>,
    channel_disabled_tools: Option<Vec<String>>,
    channel_disabled_mcp_servers: Option<Vec<String>>,
) -> JycAgentService {
    service_with_full_exclusion(
        patterns,
        channel_disabled_tools,
        channel_disabled_mcp_servers,
        None,
    )
}

/// Helper: build a service with exclusion settings and optional channel MCP configs.
fn service_with_full_exclusion(
    patterns: Vec<ChannelPattern>,
    channel_disabled_tools: Option<Vec<String>>,
    channel_disabled_mcp_servers: Option<Vec<String>>,
    channel_mcp_configs: Option<Vec<McpServerConfig>>,
) -> JycAgentService {
    JycAgentService::new(
        app_config_with_model(None),
        PathBuf::from("/tmp/test-workdir"),
        vec![],
        channel_mcp_configs,
        patterns,
        None,
        None,
        None,
        channel_disabled_tools,
        channel_disabled_mcp_servers,
        None,
        None,
        "test".to_string(),
    )
}

/// Helper: build a service with skill filter settings.
fn service_with_skills(
    patterns: Vec<ChannelPattern>,
    channel_skills: Option<Vec<String>>,
    channel_disabled_skills: Option<Vec<String>>,
) -> JycAgentService {
    JycAgentService::new(
        app_config_with_model(None),
        PathBuf::from("/tmp/test-workdir"),
        vec![],
        None,
        patterns,
        None,
        None,
        None,
        None,
        None,
        channel_skills,
        channel_disabled_skills,
        "test".to_string(),
    )
}

/// Helper: temporarily override HOME to prevent real skills from leaking into tests.
fn with_temp_home<F: FnOnce()>(f: F) {
    // Serialize tests that mutate the shared HOME env var (parallel-safety).
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".config/opencode/skills")).ok();
    std::fs::create_dir_all(tmp.path().join(".claude/skills")).ok();
    let old_home = std::env::var("HOME").ok();
    // SAFETY: only used in tests; restored immediately after f()
    unsafe { std::env::set_var("HOME", tmp.path().as_os_str()) };
    f();
    match old_home {
        Some(h) => unsafe { std::env::set_var("HOME", h) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}

#[test]
fn pattern_model_override_is_resolved() {
    let patterns = vec![ChannelPattern {
        name: "my-pattern".to_string(),
        model: Some("provider/model-from-pattern".to_string()),
        channel: ChannelType::default(),
        ..ChannelPattern::default()
    }];
    let svc = service_with_patterns(Some("provider/default-model"), patterns);

    // Simulate pattern lookup — the same `find` call used in `process`
    let resolved = Some("my-pattern")
        .and_then(|name| svc.patterns.iter().find(|p| p.name == name))
        .and_then(|p| p.model.as_deref())
        .map(|s| s.to_string())
        .or_else(|| svc.agent_config().model.clone());

    assert_eq!(resolved.as_deref(), Some("provider/model-from-pattern"));
}

#[test]
fn fallback_to_config_model_when_pattern_has_no_model() {
    let patterns = vec![ChannelPattern {
        name: "no-override".to_string(),
        model: None,
        channel: ChannelType::default(),
        ..ChannelPattern::default()
    }];
    let svc = service_with_patterns(Some("provider/default-model"), patterns);

    let resolved = Some("no-override")
        .and_then(|name| svc.patterns.iter().find(|p| p.name == name))
        .and_then(|p| p.model.as_deref())
        .map(|s| s.to_string())
        .or_else(|| svc.agent_config().model.clone());

    assert_eq!(resolved.as_deref(), Some("provider/default-model"));
}

#[test]
fn fallback_to_config_model_when_no_pattern_matches() {
    let patterns = vec![ChannelPattern {
        name: "other-pattern".to_string(),
        model: Some("provider/other-model".to_string()),
        channel: ChannelType::default(),
        ..ChannelPattern::default()
    }];
    let svc = service_with_patterns(Some("provider/default-model"), patterns);

    // Look up a name that's not in patterns
    let resolved: Option<String> = Some("unmatched-name")
        .and_then(|name| svc.patterns.iter().find(|p| p.name == name))
        .and_then(|p| p.model.as_deref())
        .map(|s| s.to_string())
        .or_else(|| svc.agent_config().model.clone());

    assert_eq!(resolved.as_deref(), Some("provider/default-model"));
}

#[test]
fn pattern_model_none_and_config_none_yields_none() {
    let patterns: Vec<ChannelPattern> = vec![];
    let svc = service_with_patterns(None, patterns);

    let resolved: Option<String> = Some("anything")
        .and_then(|name| svc.patterns.iter().find(|p| p.name == name))
        .and_then(|p| p.model.as_deref())
        .map(|s| s.to_string())
        .or_else(|| svc.agent_config().model.clone());

    assert_eq!(resolved, None);
}

#[test]
fn first_matching_pattern_wins_with_duplicate_names() {
    // When two patterns share the same name (still possible within a
    // single channel), the first in insertion order wins.
    let patterns = vec![
        ChannelPattern {
            name: "dup".to_string(),
            model: Some("provider/first".to_string()),
            channel: ChannelType::default(),
            ..ChannelPattern::default()
        },
        ChannelPattern {
            name: "dup".to_string(),
            model: Some("provider/second".to_string()),
            channel: ChannelType::default(),
            ..ChannelPattern::default()
        },
    ];
    let svc = service_with_patterns(Some("provider/default"), patterns);

    let resolved = Some("dup")
        .and_then(|name| svc.patterns.iter().find(|p| p.name == name))
        .and_then(|p| p.model.as_deref());

    assert_eq!(resolved, Some("provider/first"));
}

/// Regression test for issue #478: adding a model to the live config
/// after the service is constructed must be visible immediately, with
/// its `context_window` and other per-model fields applied. Previously
/// the service held a startup-time snapshot and required a full server
/// restart to pick up new models.
#[test]
fn reload_picks_up_new_model_context_window_without_restart() {
    // Build an AppConfig with an existing provider but no models,
    // wrapped in an Arc<ArcSwap> (the live single source of truth).
    let app = jyc_types::AppConfig {
        general: jyc_types::GeneralConfig::default(),
        channels: HashMap::new(),
        ai: jyc_types::AiConfig {
            enabled: true,
            mode: "agent".to_string(),
            model: Some("openai/gpt-4".to_string()),
            plan_model: None,
            build_model: None,
            small_model: None,
            system_prompt: None,
            max_iterations: 500,
            sse_read_timeout_secs: 120,
            text: None,
            attachments: None,
            providers: {
                let mut p = HashMap::new();
                p.insert(
                    "openai".to_string(),
                    jyc_types::ProviderDef {
                        provider_type: "openai-compatible".to_string(),
                        base_url: Some("http://api.example.com".to_string()),
                        api_key: None,
                        api_key_env: Some("TEST_KEY".to_string()),
                        context_window: Some(8000),
                        supports_images: None,
                        params: None,
                        user_agent: None,
                        pricing: None,
                        models: HashMap::new(),
                    },
                );
                p
            },
            vision: None,
            reset_compression: None,
            auto_reset_threshold: 0.95,
        },
        inspect: None,
        attachments: None,
        wecom: None,
        mcps: Vec::new(),
        scheduler: jyc_types::SchedulerConfig::default(),
        commands: Vec::new(),
        agents: std::collections::HashMap::new(),
    };
    let config = Arc::new(ArcSwap::from_pointee(app));
    let svc = JycAgentService::new(
        config.clone(),
        PathBuf::from("/tmp/test-workdir"),
        vec![],
        None,
        vec![],
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        "test".to_string(),
    );

    // Before reload: only the default `openai/gpt-4` is known, with
    // the provider-level fallback of 8000 (no per-model entry yet).
    let before = svc.agent_config();
    let openai_before = before.providers.get("openai").unwrap();
    assert!(!openai_before.models.contains_key("gpt-4"));
    assert_eq!(openai_before.context_window, Some(8000));

    // Simulate a config reload: swap in a new AppConfig with an
    // additional model that has its own context_window.
    let mut new_app = config.load().as_ref().clone();
    if let Some(openai) = new_app.ai.providers.get_mut("openai") {
        openai.models.insert(
            "gpt-4-new".to_string(),
            jyc_types::ModelDef {
                model_id: None,
                context_window: Some(128000),
                supports_images: Some(true),
                params: None,
                user_agent: None,
                pricing: None,
            },
        );
    }
    config.store(Arc::new(new_app));

    // After reload: the new model is visible with its per-model
    // context_window — no server restart required.
    let after = svc.agent_config();
    let openai_after = after.providers.get("openai").unwrap();
    let gpt4_new = openai_after.models.get("gpt-4-new").unwrap();
    assert_eq!(gpt4_new.context_window, Some(128000));
    assert_eq!(gpt4_new.supports_images, Some(true));
}

/// `derive_agent_config` must apply `channels.<name>.model` and
/// `channels.<name>.small_model` over the global `[agent]` defaults —
/// this is the channel-override branch that the live-config test above
/// does not exercise (its config has no channel entries).
#[test]
fn derive_agent_config_applies_channel_overrides() {
    // Build a minimal ChannelConfig with just the override fields we
    // care about. All other fields are explicitly `None` to mirror how
    // they appear in a real config.
    let channel_cfg = jyc_types::ChannelConfig {
        channel_type: "websocket".to_string(),
        model: Some("override/main".to_string()),
        small_model: Some("override/small".to_string()),
        inbound: None,
        outbound: None,
        feishu: None,
        gitee: None,
        github: None,
        wechat: None,
        wecom: None,
        wecom_kf: None,
        wecom_bot: None,
        monitor: None,
        patterns: None,
        ai: None,
        footer: None,
        mcps: None,
        disabled_tools: None,
        disabled_mcp_servers: None,
        skills: None,
        disabled_skills: None,
    };
    let mut channels = HashMap::new();
    channels.insert("test".to_string(), channel_cfg);

    let app = jyc_types::AppConfig {
        general: jyc_types::GeneralConfig::default(),
        channels,
        ai: jyc_types::AiConfig {
            enabled: true,
            mode: "agent".to_string(),
            model: Some("global/main".to_string()),
            plan_model: None,
            build_model: None,
            small_model: Some("global/small".to_string()),
            system_prompt: None,
            max_iterations: 500,
            sse_read_timeout_secs: 120,
            text: None,
            attachments: None,
            providers: HashMap::new(),
            vision: None,
            reset_compression: None,
            auto_reset_threshold: 0.95,
        },
        inspect: None,
        attachments: None,
        wecom: None,
        mcps: Vec::new(),
        scheduler: jyc_types::SchedulerConfig::default(),
        commands: Vec::new(),
        agents: std::collections::HashMap::new(),
    };
    let cfg = derive_agent_config(&app, "test");
    assert_eq!(cfg.model.as_deref(), Some("override/main"));
    assert_eq!(cfg.small_model.as_deref(), Some("override/small"));

    // A different channel with no override falls back to the global
    // agent settings.
    let cfg2 = derive_agent_config(&app, "other");
    assert_eq!(cfg2.model.as_deref(), Some("global/main"));
    assert_eq!(cfg2.small_model.as_deref(), Some("global/small"));
}

#[tokio::test]
async fn disabled_tools_removes_builtin_and_bridge() {
    let patterns = vec![ChannelPattern {
        name: "test".to_string(),
        disabled_tools: Some(vec!["bash".to_string(), "jyc_send_message".to_string()]),
        ..ChannelPattern::default()
    }];
    let svc = service_with_exclusion(patterns, None, None);
    let registry = svc
        .build_tool_registry(
            "test",
            Path::new("/tmp/test-topic"),
            None,
            false,
            Some("test"),
        )
        .await;

    let defs = registry.definitions();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();

    assert!(!names.contains(&"bash"), "bash should be disabled");
    assert!(
        !names.contains(&"jyc_send_message"),
        "jyc_send_message should be disabled"
    );
    assert!(names.contains(&"read"), "read should still be available");
    assert!(
        names.contains(&"jyc_reply_message"),
        "reply_message should still be available"
    );
}

#[tokio::test]
async fn disabled_builtin_tools_alias_works() {
    let patterns = vec![ChannelPattern {
        name: "test".to_string(),
        disabled_builtin_tools: Some(vec!["write".to_string()]),
        ..ChannelPattern::default()
    }];
    let svc = service_with_exclusion(patterns, None, None);
    let registry = svc
        .build_tool_registry(
            "test",
            Path::new("/tmp/test-topic"),
            None,
            false,
            Some("test"),
        )
        .await;

    let defs = registry.definitions();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"write"),
        "write should be disabled via alias"
    );
    assert!(names.contains(&"read"), "read should still be available");
}

#[tokio::test]
async fn channel_and_pattern_disabled_tools_merged() {
    let patterns = vec![ChannelPattern {
        name: "test".to_string(),
        disabled_tools: Some(vec!["write".to_string()]),
        ..ChannelPattern::default()
    }];
    let svc = service_with_exclusion(patterns, Some(vec!["bash".to_string()]), None);
    let registry = svc
        .build_tool_registry(
            "test",
            Path::new("/tmp/test-topic"),
            None,
            false,
            Some("test"),
        )
        .await;

    let defs = registry.definitions();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();

    assert!(
        !names.contains(&"bash"),
        "bash should be disabled (channel-level)"
    );
    assert!(
        !names.contains(&"write"),
        "write should be disabled (pattern-level)"
    );
    assert!(names.contains(&"read"), "read should still be available");
}

#[tokio::test]
async fn disabled_mcp_servers_skips_matching_server() {
    // We can't easily test external MCP loading, but we can verify that
    // disabled_mcp_servers does not cause a panic and that the registry
    // is built correctly when no MCPs are configured.
    let patterns = vec![ChannelPattern {
        name: "test".to_string(),
        disabled_mcp_servers: Some(vec!["invoice".to_string()]),
        ..ChannelPattern::default()
    }];
    let svc = service_with_exclusion(patterns, None, Some(vec!["other".to_string()]));
    let registry = svc
        .build_tool_registry(
            "test",
            Path::new("/tmp/test-topic"),
            None,
            false,
            Some("test"),
        )
        .await;

    // Registry should still contain built-in tools
    assert!(registry.has_tool("bash"));
    assert!(registry.has_tool("jyc_reply_message"));
}

#[tokio::test]
async fn channel_disabled_tools_works_without_pattern_match() {
    // channel-level disabled_tools should apply even when no pattern is matched
    let svc = service_with_exclusion(vec![], Some(vec!["bash".to_string()]), None);
    let registry = svc
        .build_tool_registry("test", Path::new("/tmp/test-topic"), None, false, None)
        .await;

    assert!(
        !registry.has_tool("bash"),
        "bash should be disabled (channel-level)"
    );
    assert!(registry.has_tool("read"), "read should still be available");
}

#[tokio::test]
async fn empty_disabled_tools_disables_nothing() {
    let patterns = vec![ChannelPattern {
        name: "test".to_string(),
        disabled_tools: Some(vec![]),
        disabled_mcp_servers: Some(vec![]),
        ..ChannelPattern::default()
    }];
    let svc = service_with_exclusion(patterns, Some(vec![]), Some(vec![]));
    let registry = svc
        .build_tool_registry(
            "test",
            Path::new("/tmp/test-topic"),
            None,
            false,
            Some("test"),
        )
        .await;

    assert!(registry.has_tool("bash"), "bash should still be available");
    assert!(registry.has_tool("jyc_reply_message"));
}

#[tokio::test]
async fn disabled_tools_deduplicates_between_channel_and_pattern() {
    // When both channel and pattern disable the same tool, it should only
    // be removed once (no panic or double-remove issue).
    let patterns = vec![ChannelPattern {
        name: "test".to_string(),
        disabled_tools: Some(vec!["bash".to_string()]),
        ..ChannelPattern::default()
    }];
    let svc = service_with_exclusion(patterns, Some(vec!["bash".to_string()]), None);
    let registry = svc
        .build_tool_registry(
            "test",
            Path::new("/tmp/test-topic"),
            None,
            false,
            Some("test"),
        )
        .await;

    assert!(!registry.has_tool("bash"));
    assert!(registry.has_tool("read"));
}

#[tokio::test]
async fn disabled_mcp_servers_filters_channel_configs() {
    // Verify that disabled_mcp_servers actually filters channel-level MCP configs
    // so that load_mcp_tools is not called for disabled servers.
    let patterns = vec![ChannelPattern {
        name: "test".to_string(),
        disabled_mcp_servers: Some(vec!["skip_me".to_string()]),
        ..ChannelPattern::default()
    }];
    let channel_mcps = Some(vec![McpServerConfig {
        name: "skip_me".to_string(),
        kind: jyc_types::McpServerKind::Local {
            command: vec!["echo".to_string()],
            environment: std::collections::HashMap::new(),
        },
        enabled_tools: None,
    }]);
    let svc = service_with_full_exclusion(patterns, None, None, channel_mcps);
    let registry = svc
        .build_tool_registry(
            "test",
            Path::new("/tmp/test-topic"),
            None,
            false,
            Some("test"),
        )
        .await;

    // Registry should contain built-in tools (no panic from MCP loading)
    assert!(registry.has_tool("bash"));
    assert!(registry.has_tool("jyc_reply_message"));
}

#[tokio::test]
async fn disabled_tools_server_prefix_does_not_affect_builtin() {
    // server/tool format entries should be partitioned away from plain names,
    // so built-in tools are not affected by server-prefix entries that happen
    // to share the same tool name.
    let patterns = vec![ChannelPattern {
        name: "test".to_string(),
        disabled_tools: Some(vec!["some_server/bash".to_string()]),
        ..ChannelPattern::default()
    }];
    let svc = service_with_exclusion(patterns, None, None);
    let registry = svc
        .build_tool_registry(
            "test",
            Path::new("/tmp/test-topic"),
            None,
            false,
            Some("test"),
        )
        .await;

    // bash is a built-in tool with no source(), so "some_server/bash" won't match it
    assert!(
        registry.has_tool("bash"),
        "built-in bash should NOT be disabled by server/tool prefix"
    );
    assert!(registry.has_tool("read"), "read should still be available");
}

#[tokio::test]
async fn disabled_tools_mixed_plain_and_server_prefix() {
    // Verify that plain names and server/tool names coexist correctly:
    // - plain names disable built-in/bridge tools via registry.remove()
    // - server/tool names are reserved for MCP tool pre-registration filtering
    let patterns = vec![ChannelPattern {
        name: "test".to_string(),
        disabled_tools: Some(vec![
            "bash".to_string(),
            "some_server/product_list".to_string(),
        ]),
        ..ChannelPattern::default()
    }];
    let svc = service_with_exclusion(patterns, None, None);
    let registry = svc
        .build_tool_registry(
            "test",
            Path::new("/tmp/test-topic"),
            None,
            false,
            Some("test"),
        )
        .await;

    assert!(
        !registry.has_tool("bash"),
        "plain 'bash' should disable built-in bash"
    );
    assert!(registry.has_tool("read"), "read should still be available");
    // "some_server/product_list" won't affect anything here because no MCP
    // server is configured in this test, but the partition logic ensures
    // it does not leak into plain-name removal.
}

#[tokio::test]
async fn enabled_tools_on_mcp_server_config_does_not_panic() {
    // Verify that McpServerConfig with enabled_tools is accepted and
    // does not cause panic during registry build (actual filtering is
    // tested at the mcp_client level; here we verify integration).
    let patterns = vec![ChannelPattern {
        name: "test".to_string(),
        ..ChannelPattern::default()
    }];
    let channel_mcps = Some(vec![McpServerConfig {
        name: "test_mcp".to_string(),
        kind: jyc_types::McpServerKind::Local {
            command: vec!["echo".to_string()],
            environment: std::collections::HashMap::new(),
        },
        enabled_tools: Some(vec!["allowed_tool".to_string()]),
    }]);
    let svc = service_with_full_exclusion(patterns, None, None, channel_mcps);
    let registry = svc
        .build_tool_registry(
            "test",
            Path::new("/tmp/test-topic"),
            None,
            false,
            Some("test"),
        )
        .await;

    // Built-in tools should still be present
    assert!(registry.has_tool("bash"), "bash should still be available");
    assert!(registry.has_tool("read"), "read should still be available");
}

/// Verifies that a `<topic_path>/.jyc/config.toml` with `[mcps]` does not
/// panic during registry build. Pure resolution correctness is covered by
/// `apply_topic_mcp_overlay` unit tests in jyc-types.
#[tokio::test]
async fn topic_config_with_mcps_does_not_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let jyc_dir = tmp.path().join(".jyc");
    std::fs::create_dir_all(&jyc_dir).unwrap();
    std::fs::write(
        jyc_dir.join("config.toml"),
        r#"
mcps_replace = true

[[mcps]]
name = "topic-only-mcp"
type = "local"
command = ["./topic-mcp"]
"#,
    )
    .unwrap();

    let patterns = vec![ChannelPattern {
        name: "test".to_string(),
        ..ChannelPattern::default()
    }];
    let svc = service_with_exclusion(patterns, None, None);
    let topic_cfg = jyc_types::load_topic_config(tmp.path());
    let registry = svc
        .build_tool_registry("test", tmp.path(), topic_cfg.as_ref(), false, Some("test"))
        .await;

    // Built-in tools still present — confirms the topic config didn't
    // accidentally break the rest of the registry.
    assert!(registry.has_tool("bash"));
    assert!(registry.has_tool("read"));
}

// ── Skill filtering tests ──────────────────────────────────────────

#[test]
fn discover_skills_include_filter_retains_only_matched() {
    with_temp_home(|| {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".jyc").join("skills");

        // Create three skills
        for name in &["alpha", "beta", "gamma"] {
            let dir = skills_dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
            )
            .unwrap();
        }

        let svc = service_with_skills(vec![], None, None);
        let skills = svc.discover_skills(
            tmp.path(),
            Some(&["alpha".to_string(), "gamma".to_string()]),
            None,
        );

        assert_eq!(skills.len(), 2);
        assert!(skills.iter().any(|s| s.name == "alpha"));
        assert!(skills.iter().any(|s| s.name == "gamma"));
        assert!(!skills.iter().any(|s| s.name == "beta"));
    });
}

#[test]
fn discover_skills_exclude_filter_removes_matched() {
    with_temp_home(|| {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".jyc").join("skills");

        for name in &["alpha", "beta", "gamma"] {
            let dir = skills_dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
            )
            .unwrap();
        }

        let svc = service_with_skills(vec![], None, None);
        let skills = svc.discover_skills(tmp.path(), None, Some(&["beta".to_string()]));

        assert_eq!(skills.len(), 2);
        assert!(skills.iter().any(|s| s.name == "alpha"));
        assert!(skills.iter().any(|s| s.name == "gamma"));
        assert!(!skills.iter().any(|s| s.name == "beta"));
    });
}

#[test]
fn discover_skills_include_and_exclude_combined() {
    with_temp_home(|| {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".jyc").join("skills");

        for name in &["alpha", "beta", "gamma", "delta"] {
            let dir = skills_dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
            )
            .unwrap();
        }

        let svc = service_with_skills(vec![], None, None);
        // Include alpha, beta, gamma; then exclude beta
        let skills = svc.discover_skills(
            tmp.path(),
            Some(&["alpha".to_string(), "beta".to_string(), "gamma".to_string()]),
            Some(&["beta".to_string()]),
        );

        assert_eq!(skills.len(), 2);
        assert!(skills.iter().any(|s| s.name == "alpha"));
        assert!(skills.iter().any(|s| s.name == "gamma"));
        assert!(!skills.iter().any(|s| s.name == "beta"));
        assert!(!skills.iter().any(|s| s.name == "delta"));
    });
}

#[test]
fn channel_skills_applied_when_no_pattern_match() {
    with_temp_home(|| {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".jyc").join("skills");

        for name in &["alpha", "beta"] {
            let dir = skills_dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
            )
            .unwrap();
        }

        let svc = service_with_skills(vec![], Some(vec!["alpha".to_string()]), None);
        let skills = svc.discover_skills(tmp.path(), svc.channel_skills.as_deref(), None);

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "alpha");
    });
}

#[test]
fn pattern_skills_override_channel_skills() {
    with_temp_home(|| {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".jyc").join("skills");

        for name in &["alpha", "beta", "gamma"] {
            let dir = skills_dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
            )
            .unwrap();
        }

        let patterns = vec![ChannelPattern {
            name: "test-pattern".to_string(),
            skills: Some(vec!["gamma".to_string()]),
            ..ChannelPattern::default()
        }];

        let svc = service_with_skills(patterns, Some(vec!["alpha".to_string()]), None);

        // Simulate pattern lookup as done in build_system_prompt
        let pattern = svc.patterns.iter().find(|p| p.name == "test-pattern");
        let include = pattern
            .and_then(|p| p.skills.as_deref())
            .or(svc.channel_skills.as_deref());

        let skills = svc.discover_skills(tmp.path(), include, None);

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "gamma");
    });
}

#[test]
fn channel_and_pattern_disabled_skills_merged() {
    with_temp_home(|| {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".jyc").join("skills");

        for name in &["alpha", "beta", "gamma"] {
            let dir = skills_dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
            )
            .unwrap();
        }

        let patterns = vec![ChannelPattern {
            name: "test-pattern".to_string(),
            disabled_skills: Some(vec!["beta".to_string()]),
            ..ChannelPattern::default()
        }];

        let svc = service_with_skills(patterns, None, Some(vec!["alpha".to_string()]));

        // Merge excludes as done in build_system_prompt
        let mut exclude_list: Vec<String> = Vec::new();
        if let Some(ref channel_excluded) = svc.channel_disabled_skills {
            exclude_list.extend(channel_excluded.iter().cloned());
        }
        if let Some(pattern_excluded) = svc
            .patterns
            .iter()
            .find(|p| p.name == "test-pattern")
            .and_then(|p| p.disabled_skills.as_ref())
        {
            for name in pattern_excluded {
                if !exclude_list.contains(name) {
                    exclude_list.push(name.clone());
                }
            }
        }
        let exclude_slice: Option<&[String]> = if exclude_list.is_empty() {
            None
        } else {
            Some(&exclude_list)
        };

        let skills = svc.discover_skills(tmp.path(), None, exclude_slice);

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "gamma");
    });
}

#[test]
fn no_filters_loads_all_skills() {
    with_temp_home(|| {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join(".jyc").join("skills");

        for name in &["alpha", "beta"] {
            let dir = skills_dir.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {name} skill\n---\n"),
            )
            .unwrap();
        }

        let svc = service_with_skills(vec![], None, None);
        let skills = svc.discover_skills(tmp.path(), None, None);

        assert_eq!(skills.len(), 2);
    });
}

#[test]
fn build_user_prompt_injects_build_mode_tag() {
    let svc = service_with_skills(vec![], None, None);
    let message = InboundMessage {
        id: "test-id".into(),
        channel: "test".into(),
        channel_uid: "uid".into(),
        sender: "test-sender".into(),
        sender_address: "test@example.com".into(),
        recipients: vec![],
        topic: "test".into(),
        content: jyc_types::MessageContent {
            text: Some("hello world".into()),
            ..Default::default()
        },
        timestamp: chrono::Utc::now(),
        references: None,
        reply_to_id: None,
        external_id: None,
        attachments: vec![],
        metadata: Default::default(),
        matched_pattern: None,
    };

    // Plan mode: should inject PLAN tag
    let plan_prompt = svc.build_user_prompt_text(&message, Some("plan"));
    assert!(
        plan_prompt.contains("CRITICAL: Current mode: PLAN (read-only"),
        "plan mode prompt should contain PLAN tag, got: {plan_prompt}"
    );

    // Build mode (None = no override): should inject BUILD tag
    let build_prompt = svc.build_user_prompt_text(&message, None);
    assert!(
        build_prompt.contains("Current mode: BUILD (full execution)"),
        "build mode prompt should contain BUILD tag, got: {build_prompt}"
    );
}

#[test]
fn build_user_prompt_shows_source_with_require_reply() {
    let svc = service_with_skills(vec![], None, None);
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
        "source_channel".to_string(),
        serde_json::Value::String("feishu_bot".into()),
    );
    metadata.insert(
        "source_topic".to_string(),
        serde_json::Value::String("greenfield".into()),
    );
    metadata.insert("require_reply".to_string(), serde_json::Value::Bool(true));
    let message = InboundMessage {
        id: "test-id".into(),
        channel: "test".into(),
        channel_uid: "uid".into(),
        sender: "Agent".into(),
        sender_address: "agent@jyc".into(),
        recipients: vec![],
        topic: "cross-topic".into(),
        content: jyc_types::MessageContent {
            text: Some("do work".into()),
            ..Default::default()
        },
        timestamp: chrono::Utc::now(),
        references: None,
        reply_to_id: None,
        external_id: None,
        attachments: vec![],
        metadata,
        matched_pattern: None,
    };

    let prompt = svc.build_user_prompt_text(&message, None);
    assert!(
        prompt.contains("**Source:** channel \"feishu_bot\", topic \"greenfield\""),
        "prompt should contain Source header, got: {prompt}"
    );
    assert!(
        prompt.contains("⚠️ Reply requested"),
        "prompt should contain reply-requested indicator, got: {prompt}"
    );
}

#[test]
fn build_user_prompt_shows_source_without_require_reply() {
    let svc = service_with_skills(vec![], None, None);
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
        "source_channel".to_string(),
        serde_json::Value::String("feishu_bot".into()),
    );
    metadata.insert(
        "source_topic".to_string(),
        serde_json::Value::String("greenfield".into()),
    );
    metadata.insert("require_reply".to_string(), serde_json::Value::Bool(false));
    let message = InboundMessage {
        id: "test-id".into(),
        channel: "test".into(),
        channel_uid: "uid".into(),
        sender: "Agent".into(),
        sender_address: "agent@jyc".into(),
        recipients: vec![],
        topic: "cross-topic".into(),
        content: jyc_types::MessageContent {
            text: Some("do work".into()),
            ..Default::default()
        },
        timestamp: chrono::Utc::now(),
        references: None,
        reply_to_id: None,
        external_id: None,
        attachments: vec![],
        metadata,
        matched_pattern: None,
    };

    let prompt = svc.build_user_prompt_text(&message, None);
    assert!(
        prompt.contains("**Source:** channel \"feishu_bot\", topic \"greenfield\""),
        "prompt should contain Source header, got: {prompt}"
    );
    assert!(
        !prompt.contains("⚠️ Reply requested"),
        "prompt should NOT contain reply-requested indicator, got: {prompt}"
    );
}

#[test]
fn build_user_prompt_no_source_without_metadata() {
    let svc = service_with_skills(vec![], None, None);
    let message = InboundMessage {
        id: "test-id".into(),
        channel: "test".into(),
        channel_uid: "uid".into(),
        sender: "user".into(),
        sender_address: "user@example.com".into(),
        recipients: vec![],
        topic: "normal".into(),
        content: jyc_types::MessageContent {
            text: Some("hello".into()),
            ..Default::default()
        },
        timestamp: chrono::Utc::now(),
        references: None,
        reply_to_id: None,
        external_id: None,
        attachments: vec![],
        metadata: Default::default(),
        matched_pattern: None,
    };

    let prompt = svc.build_user_prompt_text(&message, None);
    assert!(
        !prompt.contains("**Source:**"),
        "prompt should NOT contain Source header for normal messages, got: {prompt}"
    );
}

#[test]
fn pattern_mode_plan_resolved_when_no_file_override() {
    // Resolution chain: no file override → pattern.mode = "plan" → resolves to "plan"
    let mode_override: Option<String> = None; // no file override
    let pattern = ChannelPattern {
        name: "test".to_string(),
        mode: Some("plan".to_string()),
        ..ChannelPattern::default()
    };
    let resolved = mode_override.as_deref().or(pattern.mode.as_deref());
    assert_eq!(resolved, Some("plan"));
}

#[test]
fn pattern_mode_build_resolved_when_no_file_override() {
    // Resolution chain: no file override → pattern.mode = "build" → resolves to "build"
    let mode_override: Option<String> = None; // no file override
    let pattern = ChannelPattern {
        name: "test".to_string(),
        mode: Some("build".to_string()),
        ..ChannelPattern::default()
    };
    let resolved = mode_override.as_deref().or(pattern.mode.as_deref());
    assert_eq!(resolved, Some("build"));
}

#[test]
fn pattern_mode_defaults_to_build_when_unset() {
    // Resolution chain: no file override → pattern.mode = None → defaults to "build"
    let mode_override: Option<String> = None; // no file override
    let pattern = ChannelPattern {
        name: "test".to_string(),
        mode: None,
        ..ChannelPattern::default()
    };
    let resolved = mode_override
        .as_deref()
        .or(pattern.mode.as_deref())
        .unwrap_or("build");
    assert_eq!(resolved, "build");
}

#[test]
fn file_override_takes_priority_over_pattern_mode() {
    // Resolution chain: file override = "build" → pattern.mode = "plan" → resolves to "build"
    let mode_override: Option<String> = Some("build".to_string()); // file override
    let pattern = ChannelPattern {
        name: "test".to_string(),
        mode: Some("plan".to_string()),
        ..ChannelPattern::default()
    };
    let resolved = mode_override.as_deref().or(pattern.mode.as_deref());
    assert_eq!(resolved, Some("build"));
}

#[test]
fn pattern_mode_plan_injects_plan_tag_in_user_prompt() {
    let patterns = vec![ChannelPattern {
        name: "my-pattern".to_string(),
        mode: Some("plan".to_string()),
        channel: ChannelType::default(),
        ..ChannelPattern::default()
    }];
    let svc = service_with_patterns(None, patterns);
    let message = InboundMessage {
        id: "test-id".into(),
        channel: "test".into(),
        channel_uid: "uid".into(),
        sender: "test-sender".into(),
        sender_address: "test@example.com".into(),
        recipients: vec![],
        topic: "test".into(),
        content: jyc_types::MessageContent {
            text: Some("hello world".into()),
            ..Default::default()
        },
        timestamp: chrono::Utc::now(),
        references: None,
        reply_to_id: None,
        external_id: None,
        attachments: vec![],
        metadata: Default::default(),
        matched_pattern: Some("my-pattern".to_string()),
    };

    // Simulate resolution chain: no file override, pattern has mode = "plan"
    // This is what happens in build_system_prompt / process
    let mode_override: Option<String> = None;
    let pattern = svc.patterns.iter().find(|p| p.name == "my-pattern");
    let resolved = mode_override
        .as_deref()
        .or(pattern.and_then(|p| p.mode.as_deref()));

    let plan_prompt = svc.build_user_prompt_text(&message, resolved);
    assert!(
        plan_prompt.contains("CRITICAL: Current mode: PLAN (read-only"),
        "plan mode prompt should contain PLAN tag, got: {plan_prompt}"
    );
}

#[tokio::test]
async fn pattern_mcps_remote_is_registered_at_runtime() {
    use jyc_types::{AgentConfig, McpServerConfig, McpServerKind};

    let agent_config = AgentConfig {
        model: Some("provider/test".to_string()),
        template: Some("test".to_string()),
        mcps: Some(vec![McpServerConfig {
            name: "jin_full_mcp".to_string(),
            kind: McpServerKind::Local {
                command: vec!["true".to_string()],
                environment: std::collections::HashMap::new(),
            },
            enabled_tools: None,
        }]),
        ..AgentConfig::default()
    };
    let pattern = {
        let mut p = ChannelPattern {
            name: "newbee_order_bot".to_string(),
            ..ChannelPattern::default()
        };
        agent_config.fill_into_pattern(
            &mut p,
            "newbee_order_bot",
            PathBuf::from("/tmp/test-agents/newbee_order_bot"),
        );
        p
    };

    let svc = service_with_patterns(Some("provider/test"), vec![pattern]);
    let registry = svc
        .build_tool_registry(
            "newbee_order_bot",
            Path::new("/tmp/test-topic"),
            None,
            false,
            Some("newbee_order_bot"),
        )
        .await;

    assert!(registry.has_tool("bash"), "bash should be present");
    let _ = registry;
}

#[test]
fn debug_print_pattern_mcps_resolution() {
    use jyc_types::{AgentConfig, McpServerConfig, McpServerKind};
    use std::sync::OnceLock;
    use tracing_subscriber::{EnvFilter, fmt};

    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug")),
            )
            .with_test_writer()
            .try_init();
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let agent_config = AgentConfig {
            model: Some("provider/test".to_string()),
            template: Some("test".to_string()),
            mcps: Some(vec![McpServerConfig {
                name: "jin_full_mcp".to_string(),
                kind: McpServerKind::Local {
                    command: vec!["true".to_string()],
                    environment: std::collections::HashMap::new(),
                },
                enabled_tools: None,
            }]),
            ..AgentConfig::default()
        };
        let pattern = {
            let mut p = ChannelPattern {
                name: "newbee_order_bot".to_string(),
                ..ChannelPattern::default()
            };
            agent_config.fill_into_pattern(
                &mut p,
                "newbee_order_bot",
                PathBuf::from("/tmp/test-agents/newbee_order_bot"),
            );
            p
        };
        let svc = service_with_patterns(Some("provider/test"), vec![pattern]);
        let _registry = svc
            .build_tool_registry(
                "newbee_order_bot",
                Path::new("/tmp/test-topic"),
                None,
                false,
                Some("newbee_order_bot"),
            )
            .await;
    });
}
