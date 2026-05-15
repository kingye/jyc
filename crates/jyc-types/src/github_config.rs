use serde::{Deserialize, Serialize};

const DEFAULT_API_URL: &str = "https://api.github.com";

fn default_api_url() -> String {
    DEFAULT_API_URL.to_string()
}

fn default_poll_interval() -> u64 {
    60
}

fn default_true() -> bool {
    true
}

/// GitHub-specific configuration for a channel.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GithubConfig {
    /// GitHub repository owner (user or organization)
    pub owner: String,

    /// GitHub repository name
    pub repo: String,

    /// GitHub Personal Access Token (scopes: repo, read:user)
    pub token: String,

    /// GitHub API base URL (default: https://api.github.com)
    /// For GitHub Enterprise, use: https://github.example.com/api/v3
    #[serde(default = "default_api_url")]
    pub api_url: String,

    /// Polling interval in seconds (default: 60)
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Whether to poll CI check-run status on open PRs (default: true)
    #[serde(default = "default_true")]
    pub poll_ci_status: bool,
}

impl Default for GithubConfig {
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
        let config = GithubConfig::default();
        assert_eq!(config.poll_interval_secs, 60);
        assert_eq!(config.api_url, "https://api.github.com");
        assert!(config.owner.is_empty());
    }

    #[test]
    fn test_config_deserialize() {
        let toml_str = r#"
            owner = "kingye"
            repo = "jyc"
            token = "ghp_test123"
            poll_interval_secs = 120
        "#;

        let config: GithubConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.owner, "kingye");
        assert_eq!(config.repo, "jyc");
        assert_eq!(config.token, "ghp_test123");
        assert_eq!(config.poll_interval_secs, 120);
        assert_eq!(config.api_url, "https://api.github.com");
    }

    #[test]
    fn test_config_deserialize_defaults() {
        let toml_str = r#"
            owner = "kingye"
            repo = "jyc"
            token = "ghp_test123"
        "#;

        let config: GithubConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.poll_interval_secs, 60);
        assert_eq!(config.api_url, "https://api.github.com");
    }

    #[test]
    fn test_config_deserialize_enterprise_url() {
        let toml_str = r#"
            owner = "myorg"
            repo = "myrepo"
            token = "ghp_test123"
            api_url = "https://github.example.com/api/v3"
        "#;

        let config: GithubConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.owner, "myorg");
        assert_eq!(config.repo, "myrepo");
        assert_eq!(config.api_url, "https://github.example.com/api/v3");
    }

    #[test]
    fn test_config_deserialize_poll_ci_status_default() {
        let toml_str = r#"
            owner = "kingye"
            repo = "jyc"
            token = "ghp_test123"
        "#;

        let config: GithubConfig = toml::from_str(toml_str).unwrap();
        assert!(config.poll_ci_status);
    }

    #[test]
    fn test_config_deserialize_poll_ci_status_disabled() {
        let toml_str = r#"
            owner = "kingye"
            repo = "jyc"
            token = "ghp_test123"
            poll_ci_status = false
        "#;

        let config: GithubConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.poll_ci_status);
    }
}
