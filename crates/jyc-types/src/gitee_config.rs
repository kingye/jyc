use serde::{Deserialize, Serialize};

const DEFAULT_API_URL: &str = "https://gitee.com/api/v5";

fn default_api_url() -> String {
    DEFAULT_API_URL.to_string()
}

fn default_poll_interval() -> u64 {
    60
}

fn default_true() -> bool {
    true
}

/// Gitee-specific configuration for a channel.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GiteeConfig {
    /// Gitee repository owner (user or organization)
    pub owner: String,

    /// Gitee repository name
    pub repo: String,

    /// Gitee Personal Access Token (scopes: projects, pull_requests, hook)
    pub token: String,

    /// Gitee API base URL (default: https://gitee.com/api/v5)
    /// For Gitee Enterprise, use: https://gitee.example.com/api/v5
    #[serde(default = "default_api_url")]
    pub api_url: String,

    /// Polling interval in seconds (default: 60)
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Whether to poll CI build status on open PRs (default: true)
    #[serde(default = "default_true")]
    pub poll_ci_status: bool,
}

impl Default for GiteeConfig {
    fn default() -> Self {
        Self {
            owner: String::new(),
            repo: String::new(),
            token: String::new(),
            api_url: default_api_url(),
            poll_interval_secs: default_poll_interval(),
            poll_ci_status: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = GiteeConfig::default();
        assert_eq!(config.poll_interval_secs, 60);
        assert_eq!(config.api_url, "https://gitee.com/api/v5");
        assert!(config.owner.is_empty());
    }

    #[test]
    fn test_config_deserialize() {
        let toml_str = r#"
            owner = "myuser"
            repo = "myproject"
            token = "gitee_token_123"
            poll_interval_secs = 120
        "#;

        let config: GiteeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.owner, "myuser");
        assert_eq!(config.repo, "myproject");
        assert_eq!(config.token, "gitee_token_123");
        assert_eq!(config.poll_interval_secs, 120);
        assert_eq!(config.api_url, "https://gitee.com/api/v5");
    }

    #[test]
    fn test_config_deserialize_defaults() {
        let toml_str = r#"
            owner = "myuser"
            repo = "myproject"
            token = "gitee_token_123"
        "#;

        let config: GiteeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.poll_interval_secs, 60);
        assert_eq!(config.api_url, "https://gitee.com/api/v5");
    }

    #[test]
    fn test_config_deserialize_enterprise_url() {
        let toml_str = r#"
            owner = "myorg"
            repo = "myrepo"
            token = "gitee_token_123"
            api_url = "https://gitee.example.com/api/v5"
        "#;

        let config: GiteeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.owner, "myorg");
        assert_eq!(config.repo, "myrepo");
        assert_eq!(config.api_url, "https://gitee.example.com/api/v5");
    }

    #[test]
    fn test_config_deserialize_poll_ci_status_default() {
        let toml_str = r#"
            owner = "myuser"
            repo = "myproject"
            token = "gitee_token_123"
        "#;

        let config: GiteeConfig = toml::from_str(toml_str).unwrap();
        assert!(config.poll_ci_status);
    }

    #[test]
    fn test_config_deserialize_poll_ci_status_disabled() {
        let toml_str = r#"
            owner = "myuser"
            repo = "myproject"
            token = "gitee_token_123"
            poll_ci_status = false
        "#;

        let config: GiteeConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.poll_ci_status);
    }
}
