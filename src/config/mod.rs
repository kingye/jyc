#[allow(dead_code)]
pub mod types;
pub mod validation;

use anyhow::{Context, Result};
use regex::Regex;
use std::path::Path;

use types::AppConfig;

/// Load configuration from a TOML file.
///
/// Reads the file, expands `${VAR}` environment variable references,
/// then deserializes into `AppConfig`.
pub fn load_config(path: &Path) -> Result<AppConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;

    load_config_from_str(&content)
}

/// Load configuration from a TOML string.
///
/// Expands `${VAR}` environment variable references, then deserializes.
pub fn load_config_from_str(content: &str) -> Result<AppConfig> {
    // First parse as raw TOML Value so we can expand env vars
    let mut value: toml::Value = toml::from_str(content).context("failed to parse TOML")?;

    expand_env_vars(&mut value);

    // Now deserialize the expanded TOML into our config struct
    let config: AppConfig = value.try_into().context("failed to deserialize config")?;

    Ok(config)
}

/// Recursively expand `${VAR}` patterns in TOML string values
/// with values from environment variables.
///
/// Missing env vars are replaced with empty strings.
fn expand_env_vars(value: &mut toml::Value) {
    let re = Regex::new(r"\$\{(\w+)\}").unwrap();

    match value {
        toml::Value::String(s) => {
            if s.contains("${") {
                *s = re
                    .replace_all(s, |caps: &regex::Captures| {
                        std::env::var(&caps[1]).unwrap_or_default()
                    })
                    .to_string();
            }
        }
        toml::Value::Table(t) => {
            for (_, v) in t.iter_mut() {
                expand_env_vars(v);
            }
        }
        toml::Value::Array(a) => {
            for v in a.iter_mut() {
                expand_env_vars(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
mode = "opencode"
"#;

        let config = load_config_from_str(toml).unwrap();
        assert_eq!(config.channels.len(), 1);
        assert!(config.channels.contains_key("work"));
        assert_eq!(config.channels["work"].channel_type, "email");
        assert!(config.agent.enabled);
        assert_eq!(config.agent.mode, "opencode");
        // Verify default session summary config
        assert!(config.agent.summary.enabled);
        assert_eq!(config.agent.summary.timeout_hours, 1.0);
        assert_eq!(config.agent.summary.max_idle_hours, 120.0);
        assert_eq!(config.agent.summary.max_summaries, 50);
        assert_eq!(config.agent.summary.storage_dir, ".jyc/session-summaries");
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
mode = "opencode"
"#;

        let config = load_config_from_str(toml).unwrap();
        assert_eq!(config.general.max_concurrent_threads, 3);
        assert_eq!(config.general.max_queue_size_per_thread, 10);
        // Verify default session summary config
        assert!(config.agent.summary.enabled);
        assert_eq!(config.agent.summary.timeout_hours, 1.0);
        assert_eq!(config.agent.summary.max_idle_hours, 120.0);
        assert_eq!(config.agent.summary.max_summaries, 50);
        assert_eq!(config.agent.summary.storage_dir, ".jyc/session-summaries");
    }
}
