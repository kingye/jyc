//! GitHub-specific configuration for a channel.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitHubConfig {
    pub owner: String,
    pub repo: String,
    pub token: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_events")]
    pub events: Vec<String>,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            owner: String::new(),
            repo: String::new(),
            token: String::new(),
            poll_interval_secs: default_poll_interval(),
            events: default_events(),
            metadata: HashMap::new(),
        }
    }
}

fn default_poll_interval() -> u64 {
    30
}

fn default_events() -> Vec<String> {
    vec!["issue_comment".to_string(), "issues".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = GitHubConfig::default();
        assert_eq!(config.poll_interval_secs, 30);
        assert!(config.events.contains(&"issue_comment".to_string()));
        assert!(config.events.contains(&"issues".to_string()));
    }

    #[test]
    fn test_config_deserialize() {
        let toml_str = r#"
            owner = "myorg"
            repo = "myrepo"
            token = "ghp_xxxxx"
            poll_interval_secs = 60
        "#;

        let config: GitHubConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.owner, "myorg");
        assert_eq!(config.repo, "myrepo");
        assert_eq!(config.token, "ghp_xxxxx");
        assert_eq!(config.poll_interval_secs, 60);
    }
}
