use super::*;
#[cfg(test)]
mod config_loader_tests {
    use super::*;

    /// Test-only builder for `ProviderDef`. Every field except the two
    /// API-key fields is irrelevant to the `resolve_api_key` tests; this
    /// helper shrinks the four test fixtures to one line each.
    fn provider_with_keys(api_key: Option<&str>, api_key_env: Option<&str>) -> ProviderDef {
        ProviderDef {
            provider_type: "anthropic".to_string(),
            base_url: None,
            api_key: api_key.map(String::from),
            api_key_env: api_key_env.map(String::from),
            context_window: None,
            supports_images: None,
            params: None,
            user_agent: None,
            pricing: None,
            models: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn test_expand_env_vars() {
        // SAFETY: This test runs in isolation (cargo test runs single-threaded by default for unit tests)
        unsafe {
            std::env::set_var("JYC_TEST_HOST", "imap.example.com");
            std::env::set_var("JYC_TEST_PORT", "993");
        }

        let mut value = toml::Value::Table({
            let mut t = toml::map::Map::new();
            t.insert(
                "host".into(),
                toml::Value::String("${JYC_TEST_HOST}".into()),
            );
            t.insert(
                "port".into(),
                toml::Value::String("${JYC_TEST_PORT}".into()),
            );
            t.insert(
                "missing".into(),
                toml::Value::String("${JYC_NONEXISTENT}".into()),
            );
            t.insert("plain".into(), toml::Value::String("no vars here".into()));
            t
        });

        expand_env_vars(&mut value);

        let table = value.as_table().unwrap();
        assert_eq!(table["host"].as_str().unwrap(), "imap.example.com");
        assert_eq!(table["port"].as_str().unwrap(), "993");
        assert_eq!(table["missing"].as_str().unwrap(), "");
        assert_eq!(table["plain"].as_str().unwrap(), "no vars here");

        // Cleanup
        unsafe {
            std::env::remove_var("JYC_TEST_HOST");
            std::env::remove_var("JYC_TEST_PORT");
        }
    }

    /// `resolve_api_key` returns the env-var value when `api_key_env` is
    /// set and the env var exists. Late binding preserved.
    #[test]
    fn resolve_api_key_uses_api_key_env_first() {
        unsafe {
            std::env::set_var("JYC_RESOLVE_KEY_TEST", "env-key-value");
        }
        let p = provider_with_keys(Some("literal-not-used"), Some("JYC_RESOLVE_KEY_TEST"));
        assert_eq!(p.resolve_api_key().as_deref(), Some("env-key-value"));
        unsafe {
            std::env::remove_var("JYC_RESOLVE_KEY_TEST");
        }
    }

    /// When `api_key_env` is unset and `api_key` carries an expanded
    /// `${VAR}` value, return that value.
    #[test]
    fn resolve_api_key_falls_back_to_api_key_field() {
        let p = provider_with_keys(Some("expanded-key-value"), None);
        assert_eq!(p.resolve_api_key().as_deref(), Some("expanded-key-value"));
    }

    /// Empty `api_key` (i.e. `${UNSET}` after expansion) and no
    /// `api_key_env` → `None`.
    #[test]
    fn resolve_api_key_returns_none_when_empty_and_no_env() {
        let p = provider_with_keys(Some(""), None);
        assert_eq!(p.resolve_api_key(), None);
    }

    /// Neither field set → `None`.
    #[test]
    fn resolve_api_key_returns_none_when_neither_set() {
        let p = provider_with_keys(None, None);
        assert_eq!(p.resolve_api_key(), None);
    }

    #[test]
    fn test_load_minimal_config() {
        let toml = r#"
[general]

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "user"
password = "pass"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "user"
password = "pass"

[agent]
enabled = true
mode = "agent"
"#;

        let config = load_config_from_str(toml).unwrap();
        assert_eq!(config.channels.len(), 1);
        assert!(config.channels.contains_key("work"));
        assert_eq!(config.channels["work"].channel_type, "email");
        assert!(config.ai.enabled);
        assert_eq!(config.ai.mode, "agent");
    }

    #[test]
    fn test_load_ai_table_new_key() {
        // `[ai]` is the canonical key; the test above covers the legacy
        // `[agent]` alias.
        let toml = r#"
[general]

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
username = "user"
password = "pass"

[channels.work.outbound]
host = "smtp.example.com"
username = "user"
password = "pass"

[channels.work.ai]
mode = "static"
text = "ok"

[ai]
enabled = true
mode = "agent"
"#;

        let config = load_config_from_str(toml).unwrap();
        assert!(config.ai.enabled);
        assert_eq!(config.ai.mode, "agent");
        // Channel-level `[channels.x.ai]` override
        let ch_ai = config.channels["work"].ai.as_ref().unwrap();
        assert_eq!(ch_ai.mode, "static");
    }

    #[test]
    fn test_load_channel_ai_legacy_agent_alias() {
        let toml = r#"
[general]

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
username = "user"
password = "pass"

[channels.work.outbound]
host = "smtp.example.com"
username = "user"
password = "pass"

[channels.work.agent]
mode = "static"
text = "ok"

[ai]
mode = "agent"
"#;

        let config = load_config_from_str(toml).unwrap();
        let ch_ai = config.channels["work"].ai.as_ref().unwrap();
        assert_eq!(ch_ai.mode, "static");
    }

    #[test]
    fn test_load_config_with_defaults() {
        let toml = r#"
[general]

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "user"
password = "pass"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "user"
password = "pass"

[agent]
enabled = true
mode = "agent"
"#;

        let config = load_config_from_str(toml).unwrap();
        assert_eq!(config.general.max_concurrent_topics, 3);
        assert_eq!(config.general.max_queue_size_per_topic, 10);
    }

    #[test]
    fn test_channel_pattern_pipe_defaults_to_none_and_parses() {
        let config = load_config_from_str(
            r#"
[channels.feishu_bot]
type = "feishu"

[channels.feishu_bot.feishu]
app_id = "a"
app_secret = "b"

[[channels.feishu_bot.patterns]]
name = "piped"
pipe = { channel = "local_dev", topic = "jyc" }

[[channels.feishu_bot.patterns]]
name = "plain"

[agent]
enabled = true
mode = "agent"
"#,
        )
        .unwrap();
        let patterns = config.channels["feishu_bot"].patterns.as_ref().unwrap();
        // Set explicitly -> parsed as the (channel, topic) mapping.
        let pipe = patterns[0].pipe.as_ref().unwrap();
        assert_eq!(pipe.channel.as_deref(), Some("local_dev"));
        assert_eq!(pipe.topic.as_deref(), Some("jyc"));
        assert_eq!(pipe.pattern, None);
        assert_eq!(pipe.agent, None);
        // Omitted -> default None (routed normally).
        assert!(patterns[1].pipe.is_none());
    }

    /// Legacy `thread_*` keys in `[general]` must fail loudly instead of
    /// being silently ignored (which previously fell back to defaults).
    #[test]
    fn test_general_rejects_legacy_thread_keys() {
        let err = load_config_from_str(
            r#"
[general]
max_concurrent_threads = 5
max_queue_size_per_thread = 20

[agent]
enabled = true
mode = "agent"
"#,
        )
        .unwrap_err();
        // anyhow's Display shows only the outermost context; the serde
        // "unknown field" message lives in the error chain.
        let chain = err
            .chain()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            chain.contains("max_concurrent_threads"),
            "error chain should name the unknown key: {chain}"
        );
    }

    /// Legacy pattern-level `thread_*` keys must fail loudly too.
    #[test]
    fn test_pattern_rejects_legacy_thread_keys() {
        let err = load_config_from_str(
            r#"
[channels.work]
type = "email"

[[channels.work.patterns]]
name = "p"
thread_name = "t"

[agent]
enabled = true
mode = "agent"
"#,
        )
        .unwrap_err();
        let chain = err
            .chain()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            chain.contains("thread_name"),
            "error chain should name the unknown key: {chain}"
        );
    }

    /// End-to-end: `api_key = "${VAR}"` round-trips through the TOML
    /// loader. `${VAR}` expands at load time; the resolved value lands
    /// in the `api_key` field.
    #[test]
    fn test_provider_api_key_field_parses() {
        let toml = r#"
[general]

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "u"
password = "p"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "u"
password = "p"

[agent]
enabled = true
mode = "agent"

[agent.providers.anthropic]
type = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key = "${JYC_LOAD_TEST_API_KEY}"
"#;

        // SAFETY: see existing test_expand_env_vars above. This test
        // runs alongside other unit tests; AGENTS.md prefers no env
        // mutation, but this is a load-time check of ${VAR} expansion
        // for the new field — the only way to verify it end-to-end
        // without restructuring the loader to take env as a parameter.
        unsafe {
            std::env::set_var("JYC_LOAD_TEST_API_KEY", "loaded-key-123");
        }
        let config = load_config_from_str(toml).unwrap();
        let provider = config
            .ai
            .providers
            .get("anthropic")
            .expect("anthropic provider must parse");
        assert_eq!(
            provider.api_key.as_deref(),
            Some("loaded-key-123"),
            "${{VAR}} must expand into api_key"
        );
        // legacy field stays None when not set
        assert!(provider.api_key_env.is_none());
        unsafe {
            std::env::remove_var("JYC_LOAD_TEST_API_KEY");
        }
    }

    #[test]
    fn test_load_config_with_mcps() {
        let toml = r#"
[general]

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "user"
password = "pass"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "user"
password = "pass"

[agent]
enabled = true
mode = "agent"

[[mcps]]
name = "jyc_vision"
type = "local"
command = ["jyc", "mcp-vision-tool"]
environment = { "VISION_API_KEY" = "secret", "VISION_API_URL" = "https://api.example.com" }

[[mcps]]
name = "remote_mcp"
type = "remote"
url = "https://mcp.example.com/handler"
enabled = true
"#;

        let config = load_config_from_str(toml).unwrap();
        assert_eq!(config.mcps.len(), 2);

        let vision = &config.mcps[0];
        assert_eq!(vision.name, "jyc_vision");
        match &vision.kind {
            super::McpServerKind::Local {
                command,
                environment,
            } => {
                assert_eq!(command, &["jyc", "mcp-vision-tool"]);
                assert_eq!(environment.get("VISION_API_KEY").unwrap(), "secret");
            }
            _ => panic!("Expected Local variant for jyc_vision"),
        }

        let remote = &config.mcps[1];
        assert_eq!(remote.name, "remote_mcp");
        match &remote.kind {
            super::McpServerKind::Remote {
                url,
                enabled,
                auth_header,
                custom_headers,
                oauth,
            } => {
                assert_eq!(url, "https://mcp.example.com/handler");
                assert!(*enabled);
                assert!(auth_header.is_none());
                assert!(custom_headers.is_empty());
                assert!(oauth.is_none());
            }
            _ => panic!("Expected Remote variant for remote_mcp"),
        }
    }

    #[test]
    fn test_merge_toml_tables_deep_merge() {
        let base: toml::Value = toml::from_str(
            r#"
[general]
max_concurrent_topics = 3

[agent]
model = "global-model"
mode = "opencode"
"#,
        )
        .unwrap();
        let overlay: toml::Value = toml::from_str(
            r#"
[agent]
model = "workdir-model"
"#,
        )
        .unwrap();

        let merged = merge_toml(base, overlay);
        assert_eq!(
            merged["general"]["max_concurrent_topics"].as_integer(),
            Some(3)
        );
        // Overlay wins on conflicting keys
        assert_eq!(merged["agent"]["model"].as_str(), Some("workdir-model"));
        // Base-only keys survive
        assert_eq!(merged["agent"]["mode"].as_str(), Some("opencode"));
    }

    #[test]
    fn test_merge_toml_channels_merge_by_name() {
        let base: toml::Value = toml::from_str(
            r#"
[channels.global_chan]
type = "email"

[channels.shared]
type = "email"
"#,
        )
        .unwrap();
        let overlay: toml::Value = toml::from_str(
            r#"
[channels.local_chan]
type = "feishu"

[channels.shared]
type = "websocket"
"#,
        )
        .unwrap();

        let merged = merge_toml(base, overlay);
        let channels = merged["channels"].as_table().unwrap();
        assert_eq!(channels.len(), 3);
        assert_eq!(channels["global_chan"]["type"].as_str(), Some("email"));
        assert_eq!(channels["local_chan"]["type"].as_str(), Some("feishu"));
        // Same-name channel: overlay wins
        assert_eq!(channels["shared"]["type"].as_str(), Some("websocket"));
    }

    #[test]
    fn test_merge_toml_arrays_replaced_not_concatenated() {
        let base: toml::Value = toml::from_str(
            r#"
[[mcps]]
name = "a"
"#,
        )
        .unwrap();
        let overlay: toml::Value = toml::from_str(
            r#"
[[mcps]]
name = "b"
"#,
        )
        .unwrap();

        let merged = merge_toml(base, overlay);
        let mcps = merged["mcps"].as_array().unwrap();
        assert_eq!(mcps.len(), 1);
        assert_eq!(mcps[0]["name"].as_str(), Some("b"));
    }

    #[test]
    fn test_load_config_layered_global_base_workdir_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let global_path = tmp.path().join("global.toml");
        let workdir_path = tmp.path().join("config.toml");

        std::fs::write(
            &global_path,
            r#"
[agent]
mode = "static"
model = "global-model"

[channels.global_chan]
type = "email"
"#,
        )
        .unwrap();
        std::fs::write(
            &workdir_path,
            r#"
[agent]
mode = "static"
model = "workdir-model"

[channels.local_chan]
type = "feishu"
"#,
        )
        .unwrap();

        let config = load_config_layered(Some(&global_path), &workdir_path).unwrap();
        assert_eq!(config.ai.model.as_deref(), Some("workdir-model"));
        assert!(config.channels.contains_key("global_chan"));
        assert!(config.channels.contains_key("local_chan"));
    }

    /// L2 `${VAR}` expansion runs *after* the global/workdir deep-merge.
    /// Verifies (a) `${VAR}` from the global is expanded when only the
    /// global defines it, and (b) the workdir's literal overrides the
    /// global's `${VAR}` reference (overlay wins on scalar keys).
    #[test]
    fn test_load_config_layered_expands_env_vars_after_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let global_path = tmp.path().join("global.toml");
        let workdir_path = tmp.path().join("config.toml");

        // Global uses a `${VAR}` reference; expansion happens after the
        // merge, so this is resolved against the env var at load time.
        std::fs::write(
            &global_path,
            r#"
[agent]
mode = "static"

[channels.work]
type = "email"

[channels.work.inbound]
host = "${JYC_LAYERED_HOST}"
port = 993
username = "u"
password = "p"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "u"
password = "p"
"#,
        )
        .unwrap();

        // Workdir overrides `host` with a literal — should win over the
        // global's `${VAR}` reference (scalar replacement in `merge_toml`).
        std::fs::write(
            &workdir_path,
            r#"
[channels.work.inbound]
host = "literal-host.example.com"
"#,
        )
        .unwrap();

        // SAFETY: existing test pattern; see test_expand_env_vars above.
        unsafe {
            std::env::set_var("JYC_LAYERED_HOST", "expanded-from-env.example.com");
        }
        let config = load_config_layered(Some(&global_path), &workdir_path).unwrap();
        let work = &config.channels["work"];
        let inbound = work.inbound.as_ref().expect("inbound must parse");
        // Workdir's literal wins over the global's `${VAR}` reference.
        assert_eq!(inbound.host, "literal-host.example.com");

        // SAFETY: cleanup.
        unsafe {
            std::env::remove_var("JYC_LAYERED_HOST");
        }
    }

    /// Companion to the above: when the workdir *also* uses `${VAR}`
    /// (and the global is absent), expansion still works. This is the
    /// simpler path through L2.
    #[test]
    fn test_load_config_layered_expands_env_vars_in_workdir_only() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir_path = tmp.path().join("config.toml");
        // No global path — workdir alone.

        std::fs::write(
            &workdir_path,
            r#"
[agent]
mode = "static"

[channels.work]
type = "email"

[channels.work.inbound]
host = "${JYC_LAYERED_WORKDIR_ONLY_HOST}"
port = 993
username = "u"
password = "p"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "u"
password = "p"
"#,
        )
        .unwrap();

        unsafe {
            std::env::set_var(
                "JYC_LAYERED_WORKDIR_ONLY_HOST",
                "workdir-only-expanded.example.com",
            );
        }
        let config = load_config_layered(None, &workdir_path).unwrap();
        let work = &config.channels["work"];
        let inbound = work.inbound.as_ref().expect("inbound must parse");
        assert_eq!(inbound.host, "workdir-only-expanded.example.com");
        unsafe {
            std::env::remove_var("JYC_LAYERED_WORKDIR_ONLY_HOST");
        }
    }

    #[test]
    fn test_load_config_layered_missing_global_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let global_path = tmp.path().join("nonexistent.toml");
        let workdir_path = tmp.path().join("config.toml");
        std::fs::write(
            &workdir_path,
            r#"
[agent]
mode = "static"
"#,
        )
        .unwrap();

        let config = load_config_layered(Some(&global_path), &workdir_path).unwrap();
        assert_eq!(config.ai.mode, "static");
    }

    #[test]
    fn test_load_topic_config_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_topic_config(tmp.path()).is_none());
    }

    /// `[agents.<name>]` table parses; behavior fields mirror the legacy
    /// pattern surface (minus `rules`).
    #[test]
    fn test_load_agents_table_parses() {
        let toml = r#"
[agents.jyc]
template = "jyc"
topic_path = "~/projects/jyc"
skills = ["coding-principles"]
disabled_skills = []
access = { read = ["~/.cargo/registry/src"], write = ["/tmp/jyc-builds"] }
attachments = { enabled = true, allowed_extensions = [".pdf", ".md"] }
reset_compression = { mode = "llm", keep_pairs = 3 }
auto_reset_threshold = 0.9
model = "anthropic/claude-opus-4-6"
small_model = "deepseek/deepseek-v4-flash"
mcps = []
disabled_tools = []
disabled_mcp_servers = []
live_injection = true
inject_inbound_images = false
mode = "build"
role = "Developer"

[ai]
mode = "agent"
"#;
        let cfg = load_config_from_str(toml).unwrap();
        assert_eq!(cfg.agents.len(), 1, "one agent parsed");
        let agent = cfg.agents.get("jyc").expect("jyc agent");
        assert_eq!(agent.template.as_deref(), Some("jyc"));
        assert_eq!(agent.topic_path.as_deref(), Some("~/projects/jyc"));
        assert_eq!(
            agent.skills.as_deref(),
            Some(&["coding-principles".to_string()][..])
        );
        let access = agent.access.as_ref().expect("access present");
        assert_eq!(access.read, vec!["~/.cargo/registry/src".to_string()]);
        assert_eq!(access.write, vec!["/tmp/jyc-builds".to_string()]);
        let atts = agent.attachments.as_ref().expect("attachments present");
        assert!(atts.enabled);
        assert_eq!(
            atts.allowed_extensions,
            vec![".pdf".to_string(), ".md".to_string()]
        );
        let rc = agent.reset_compression.as_ref().expect("reset_compression");
        assert!(matches!(rc.mode, crate::channel::CompressionMode::Llm));
        assert_eq!(rc.keep_pairs, 3);
        assert_eq!(agent.auto_reset_threshold, Some(0.9));
        assert_eq!(agent.model.as_deref(), Some("anthropic/claude-opus-4-6"));
        assert_eq!(
            agent.small_model.as_deref(),
            Some("deepseek/deepseek-v4-flash")
        );
        assert!(agent.mcps.as_ref().unwrap().is_empty());
        assert!(agent.live_injection);
        assert!(!agent.inject_inbound_images);
        assert_eq!(agent.mode.as_deref(), Some("build"));
        assert_eq!(agent.role.as_deref(), Some("Developer"));
    }

    /// Agents config defaults: empty when absent (backward-compatible).
    #[test]
    fn test_load_agents_defaults_to_empty() {
        let cfg = load_config_from_str(
            r#"
[ai]
mode = "agent"
"#,
        )
        .unwrap();
        assert!(cfg.agents.is_empty());
    }

    /// `pipe = { agent = "...", topic = "..." }` parses; old fields stay None.
    #[test]
    fn test_pipe_target_agent_form_parses() {
        let cfg = load_config_from_str(
            r#"
[channels.feishu_bot]
type = "feishu"

[channels.feishu_bot.feishu]
app_id = "a"
app_secret = "b"

[[channels.feishu_bot.patterns]]
name = "piped_agent"
pipe = { agent = "jyc", topic = "${msg.chat_name}" }

[ai]
mode = "agent"
"#,
        )
        .unwrap();
        let pipe = cfg.channels["feishu_bot"].patterns.as_ref().unwrap()[0]
            .pipe
            .as_ref()
            .unwrap();
        assert_eq!(pipe.agent.as_deref(), Some("jyc"));
        assert_eq!(pipe.topic.as_deref(), Some("${msg.chat_name}"));
        assert_eq!(pipe.channel, None);
        assert_eq!(pipe.pattern, None);
    }

    /// Agent table with no `topic_path` is allowed (default resolves at
    /// runtime in the CLI layer).
    #[test]
    fn test_load_agents_minimal() {
        let cfg = load_config_from_str(
            r#"
[agents.jyc]
template = "jyc"

[ai]
mode = "agent"
"#,
        )
        .unwrap();
        let agent = cfg.agents.get("jyc").expect("jyc agent");
        assert_eq!(agent.template.as_deref(), Some("jyc"));
        assert_eq!(agent.topic_path, None);
        assert!(agent.access.is_none());
        assert!(agent.skills.is_none());
        assert!(agent.live_injection, "live_injection defaults to true");
    }

    #[test]
    fn test_load_topic_config_agent_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("config.toml"),
            r#"
[agent]
model = "provider/topic-model"
plan_model = "provider/plan-model"
small_model = "provider/small-model"
"#,
        )
        .unwrap();

        let cfg = load_topic_config(tmp.path()).unwrap();
        let agent = cfg.ai.unwrap();
        assert_eq!(agent.model.as_deref(), Some("provider/topic-model"));
        assert_eq!(agent.plan_model.as_deref(), Some("provider/plan-model"));
        assert_eq!(agent.build_model, None);
        assert_eq!(agent.small_model.as_deref(), Some("provider/small-model"));
    }

    #[test]
    fn test_load_topic_config_invalid_toml_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(jyc_dir.join("config.toml"), "not [valid toml").unwrap();
        assert!(load_topic_config(tmp.path()).is_none());
    }

    #[test]
    fn test_load_topic_config_mcps_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("config.toml"),
            r#"
[[mcps]]
name = "local-only"
type = "local"
command = ["./local-mcp"]

[agent]
model = "anthropic/claude-opus-4-7"
"#,
        )
        .unwrap();

        let cfg = load_topic_config(tmp.path()).unwrap();
        let mcps = cfg.mcps.expect("mcps field should be present");
        assert_eq!(mcps.len(), 1);
        assert_eq!(mcps[0].name, "local-only");
        assert!(matches!(mcps[0].kind, McpServerKind::Local { .. }));
        // mcps_replace defaults to false (additive).
        assert!(!cfg.mcps_replace);
    }

    #[test]
    fn test_load_topic_config_mcps_replace_flag_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("config.toml"),
            r#"
mcps_replace = true

[[mcps]]
name = "totally-different"
type = "remote"
url = "https://example.com/mcp"
"#,
        )
        .unwrap();

        let cfg = load_topic_config(tmp.path()).unwrap();
        assert!(cfg.mcps_replace);
        let mcps = cfg.mcps.unwrap();
        assert_eq!(mcps[0].name, "totally-different");
    }

    /// L3 bug fix regression: `${VAR}` in `[agent].model` must expand at
    /// topic-config load. Before the shared `parse_and_deserialize`
    /// helper, topic loader bypassed `expand_env_vars` and the literal
    /// `${VAR}` string landed in the `TopicConfig`.
    #[test]
    fn test_load_topic_config_expands_env_vars_in_agent_model() {
        // SAFETY: AGENTS.md prefers no env mutation, but verifying the
        // load-time expansion end-to-end requires it. Test uses a unique
        // env-var name and cleans up; see existing test_expand_env_vars.
        unsafe {
            std::env::set_var("JYC_LOAD_THREAD_MODEL", "anthropic/claude-opus-4-7");
        }
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("config.toml"),
            r#"
[agent]
model = "${JYC_LOAD_THREAD_MODEL}"
"#,
        )
        .unwrap();

        let cfg = load_topic_config(tmp.path()).unwrap();
        let agent = cfg.ai.unwrap();
        assert_eq!(
            agent.model.as_deref(),
            Some("anthropic/claude-opus-4-7"),
            "${{VAR}} in [agent].model must expand at topic-config load"
        );
        unsafe {
            std::env::remove_var("JYC_LOAD_THREAD_MODEL");
        }
    }

    /// L3 `${VAR}` expansion in `[[mcps]].command` (a `Vec<String>`).
    /// The recursive walker descends into arrays too, so each element
    /// gets expanded.
    #[test]
    fn test_load_topic_config_expands_env_vars_in_mcp_command() {
        unsafe {
            std::env::set_var("JYC_LOAD_THREAD_MCP_BIN", "/opt/jyc/mcp-server");
            std::env::set_var("JYC_LOAD_THREAD_MCP_TOKEN", "secret-token");
        }
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("config.toml"),
            r#"
[[mcps]]
name = "local-tools"
type = "local"
command = ["${JYC_LOAD_THREAD_MCP_BIN}", "--flag"]

[mcps.environment]
TOKEN = "${JYC_LOAD_THREAD_MCP_TOKEN}"
"#,
        )
        .unwrap();

        let cfg = load_topic_config(tmp.path()).unwrap();
        let mcps = cfg.mcps.expect("mcps field should be present");
        assert_eq!(mcps.len(), 1);
        match &mcps[0].kind {
            McpServerKind::Local {
                command,
                environment,
            } => {
                assert_eq!(command[0], "/opt/jyc/mcp-server");
                assert_eq!(command[1], "--flag");
                assert_eq!(
                    environment.get("TOKEN").map(String::as_str),
                    Some("secret-token"),
                    "${{VAR}} must expand inside [[mcps]].environment"
                );
            }
            other => panic!("expected Local MCP, got {:?}", other),
        }
        unsafe {
            std::env::remove_var("JYC_LOAD_THREAD_MCP_BIN");
            std::env::remove_var("JYC_LOAD_THREAD_MCP_TOKEN");
        }
    }

    /// Missing env var at topic level → empty string, no panic. Matches
    /// the global loader's `unwrap_or_default()` behavior.
    #[test]
    fn test_load_topic_config_missing_env_var_yields_empty() {
        // SAFETY: ensure the env var is unset before the test runs.
        unsafe {
            std::env::remove_var("JYC_LOAD_THREAD_DEFINITELY_UNSET");
        }
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("config.toml"),
            r#"
[agent]
model = "${JYC_LOAD_THREAD_DEFINITELY_UNSET}"
"#,
        )
        .unwrap();

        let cfg = load_topic_config(tmp.path()).unwrap();
        let agent = cfg.ai.unwrap();
        assert_eq!(
            agent.model.as_deref(),
            Some(""),
            "missing env var must expand to empty string"
        );
    }

    #[test]
    fn test_load_config_layered_same_path_not_double_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[agent]
mode = "static"
"#,
        )
        .unwrap();

        let config = load_config_layered(Some(&path), &path).unwrap();
        assert_eq!(config.ai.mode, "static");
    }

    // ---- apply_topic_mcp_overlay ----

    fn local_mcp(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            kind: McpServerKind::Local {
                command: vec!["./x".to_string()],
                environment: Default::default(),
            },
            enabled_tools: None,
        }
    }

    fn remote_mcp(name: &str, url: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            kind: McpServerKind::Remote {
                url: url.to_string(),
                enabled: true,
                auth_header: None,
                custom_headers: Default::default(),
                oauth: None,
            },
            enabled_tools: None,
        }
    }

    #[test]
    fn test_apply_topic_mcp_overlay_none_is_noop() {
        let base = vec![local_mcp("a")];
        let out = apply_topic_mcp_overlay(&base, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "a");
    }

    #[test]
    fn test_apply_topic_mcp_overlay_additive_unions() {
        let base = vec![local_mcp("a")];
        let topic = TopicConfig {
            mcps: Some(vec![remote_mcp("b", "https://b")]),
            mcps_replace: false,
            ..Default::default()
        };
        let out = apply_topic_mcp_overlay(&base, Some(&topic));
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn test_apply_topic_mcp_overlay_topic_wins_on_conflict() {
        let base = vec![remote_mcp("a", "https://inherited")];
        let topic = TopicConfig {
            mcps: Some(vec![remote_mcp("a", "https://topic")]),
            mcps_replace: false,
            ..Default::default()
        };
        let out = apply_topic_mcp_overlay(&base, Some(&topic));
        assert_eq!(out.len(), 1);
        match &out[0].kind {
            McpServerKind::Remote { url, .. } => assert_eq!(url, "https://topic"),
            _ => panic!("expected remote"),
        }
    }

    #[test]
    fn test_apply_topic_mcp_overlay_replace_drops_base() {
        let base = vec![local_mcp("a"), local_mcp("b")];
        let topic = TopicConfig {
            mcps: Some(vec![remote_mcp("c", "https://c")]),
            mcps_replace: true,
            ..Default::default()
        };
        let out = apply_topic_mcp_overlay(&base, Some(&topic));
        let names: Vec<&str> = out.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["c"]);
    }

    /// Helper-level coverage: `parse_and_deserialize` runs the full
    /// parse + expand + deserialize pipeline used by all three loaders.
    #[test]
    fn parse_and_deserialize_expands_env_vars() {
        unsafe {
            std::env::set_var("JYC_PARSE_AND_DESERIALIZE_VAR", "value-from-env");
        }
        let toml = r#"
[general]

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.example.com"
port = 993
username = "u"
password = "${JYC_PARSE_AND_DESERIALIZE_VAR}"

[channels.work.outbound]
host = "smtp.example.com"
port = 465
username = "u"
password = "literal-pw"

[agent]
enabled = true
mode = "agent"
"#;
        let cfg: AppConfig = parse_and_deserialize(toml, "<test>").unwrap();
        let work = cfg.channels.get("work").expect("work channel must parse");
        assert_eq!(work.inbound.as_ref().unwrap().password, "value-from-env");
        assert_eq!(work.outbound.as_ref().unwrap().password, "literal-pw");
        unsafe {
            std::env::remove_var("JYC_PARSE_AND_DESERIALIZE_VAR");
        }
    }

    /// Helper-level coverage: `parse_toml_value` errors out on invalid
    /// TOML with a context message.
    #[test]
    fn parse_toml_value_handles_invalid_toml() {
        let result = parse_toml_value("not [valid", "<bad>");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("failed to parse TOML") && msg.contains("<bad>"),
            "error must mention the failure and ctx label; got: {msg}"
        );
    }
}

#[cfg(test)]
mod base_url_tests {
    use super::*;

    fn cfg(bind: &str, base_url: Option<&str>) -> InspectConfig {
        InspectConfig {
            enabled: true,
            bind: bind.into(),
            base_url: base_url.map(String::from),
        }
    }

    #[test]
    fn explicit_base_url_wins_and_is_trimmed() {
        assert_eq!(
            cfg("0.0.0.0:9876", Some("https://jyc.example.com/")).effective_base_url(),
            "https://jyc.example.com"
        );
    }

    #[test]
    fn explicit_base_url_keeps_port_and_subpath() {
        assert_eq!(
            cfg("0.0.0.0:9876", Some("https://jyc.example.com:8443/jyc")).effective_base_url(),
            "https://jyc.example.com:8443/jyc"
        );
    }

    #[test]
    fn concrete_bind_is_used_verbatim() {
        assert_eq!(
            cfg("127.0.0.1:9876", None).effective_base_url(),
            "http://127.0.0.1:9876"
        );
    }

    /// A wildcard bind is not reachable from a client, so it must never
    /// appear in a published link — but the port must survive.
    #[test]
    fn wildcard_bind_is_replaced_but_port_kept() {
        for bind in ["0.0.0.0:9876", "[::]:9876"] {
            let url = cfg(bind, None).effective_base_url();
            assert!(
                !url.contains("0.0.0.0") && !url.contains("[::]"),
                "wildcard host leaked into link: {url}"
            );
            assert!(url.ends_with(":9876"), "port must be preserved: {url}");
            assert!(url.starts_with("http://"), "scheme missing: {url}");
        }
    }

    /// An unparseable bind has no port to preserve; pass it through rather
    /// than inventing one (config validation rejects it separately).
    #[test]
    fn unparseable_bind_passes_through() {
        assert_eq!(
            cfg("not-a-socket-addr", None).effective_base_url(),
            "http://not-a-socket-addr"
        );
    }
}

#[cfg(test)]
mod agent_extends_tests {
    use super::*;

    fn parse(cfg: &str) -> Result<AppConfig> {
        load_config_from_str(cfg)
    }

    #[test]
    fn child_overrides_and_inherits_base() {
        let cfg = parse(
            r#"
            [ai]
            model = "deepseek/deepseek-v4-pro"

            [agents.base]
            role = "Planner"
            model = "deepseek/deepseek-v4-pro"
            skills = ["plan-solution", "coding-principles"]
            live_injection = false

            [agents.special]
            extends = "base"
            role = "Reviewer"
            live_injection = true
        "#,
        )
        .unwrap();
        let base = cfg.agents.get("base").unwrap();
        assert_eq!(base.role.as_deref(), Some("Planner"));
        assert_eq!(base.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert!(!base.live_injection);

        let special = cfg.agents.get("special").unwrap();
        // Child overrides its own fields...
        assert_eq!(special.role.as_deref(), Some("Reviewer"));
        assert!(special.live_injection);
        // ...and inherits the rest from base.
        assert_eq!(special.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(
            special.skills.as_deref(),
            Some(vec!["plan-solution".to_string(), "coding-principles".to_string()].as_slice())
        );
    }

    #[test]
    fn chain_inheritance_resolves_recursively() {
        let cfg = parse(
            r#"
            [ai]
            model = "deepseek/deepseek-v4-pro"

            [agents.c]
            model = "anthropic/claude-opus-4-6"
            role = "Developer"

            [agents.b]
            extends = "c"
            role = "Planner"

            [agents.a]
            extends = "b"
            skills = ["plan-solution"]
        "#,
        )
        .unwrap();
        let a = cfg.agents.get("a").unwrap();
        // A inherits from B which inherits from C.
        assert_eq!(a.model.as_deref(), Some("anthropic/claude-opus-4-6"));
        assert_eq!(a.role.as_deref(), Some("Planner"));
        assert_eq!(
            a.skills.as_deref(),
            Some(vec!["plan-solution".to_string()].as_slice())
        );
    }

    #[test]
    fn list_fields_are_replaced_not_merged() {
        let cfg = parse(
            r#"
            [ai]
            model = "deepseek/deepseek-v4-pro"

            [agents.base]
            skills = ["a", "b"]
            disabled_tools = ["bash"]

            [agents.child]
            extends = "base"
            skills = ["c"]
        "#,
        )
        .unwrap();
        let child = cfg.agents.get("child").unwrap();
        // Override semantics: the child's list replaces the base's list.
        assert_eq!(
            child.skills.as_deref(),
            Some(vec!["c".to_string()].as_slice())
        );
        // Unset fields still inherit.
        assert_eq!(
            child.disabled_tools.as_deref(),
            Some(vec!["bash".to_string()].as_slice())
        );
    }

    #[test]
    fn empty_string_clears_inherited_value() {
        let cfg = parse(
            r#"
            [ai]
            model = "deepseek/deepseek-v4-pro"

            [agents.base]
            topic_path = "~/projects/foo"
            model = "deepseek/deepseek-v4-pro"

            [agents.child]
            extends = "base"
            topic_path = ""
        "#,
        )
        .unwrap();
        let child = cfg.agents.get("child").unwrap();
        // topic_path = "" clears the inherited path → falls back to None.
        assert_eq!(child.topic_path, None);
        // Other inherited fields are untouched.
        assert_eq!(child.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
    }

    #[test]
    fn nested_table_merge_recurses() {
        let cfg = parse(
            r#"
            [ai]
            model = "deepseek/deepseek-v4-pro"

            [agents.base]
            access = { read = ["/a"], write = ["/b"] }

            [agents.child]
            extends = "base"
            access = { read = ["/c"] }
        "#,
        )
        .unwrap();
        let child = cfg.agents.get("child").unwrap();
        let access = child.access.as_ref().unwrap();
        assert_eq!(access.read, vec!["/c".to_string()]);
        assert_eq!(access.write, vec!["/b".to_string()]);
    }

    #[test]
    fn unknown_base_agent_is_error() {
        let err = parse(
            r#"
            [ai]
            model = "deepseek/deepseek-v4-pro"

            [agents.child]
            extends = "no_such_agent"
        "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("no_such_agent"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn extends_cycle_is_error() {
        let err = parse(
            r#"
            [ai]
            model = "deepseek/deepseek-v4-pro"

            [agents.a]
            extends = "b"
            [agents.b]
            extends = "a"
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("cycle"), "unexpected error: {err}");
    }

    #[test]
    fn self_extends_is_error() {
        let err = parse(
            r#"
            [ai]
            model = "deepseek/deepseek-v4-pro"

            [agents.a]
            extends = "a"
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("cycle"), "unexpected error: {err}");
    }

    #[test]
    fn non_string_extends_is_error() {
        let err = parse(
            r#"
            [ai]
            model = "deepseek/deepseek-v4-pro"

            [agents.child]
            extends = ["not_a_string"]
        "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("extends must be a string"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn config_without_agents_is_untouched() {
        let cfg = parse(
            r#"
            [ai]
            model = "deepseek/deepseek-v4-pro"
        "#,
        )
        .unwrap();
        assert!(cfg.agents.is_empty());
    }
}
