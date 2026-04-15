use serde::{Deserialize, Serialize};

/// GitHub-specific configuration for a channel.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GithubConfig {
    /// GitHub repository owner (user or organization)
    pub owner: String,

    /// GitHub repository name
    pub repo: String,

    /// GitHub Personal Access Token (scopes: repo, read:user)
    pub token: String,

    /// Polling interval in seconds (default: 60)
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            owner: String::new(),
            repo: String::new(),
            token: String::new(),
            poll_interval_secs: default_poll_interval(),
        }
    }
}

fn default_poll_interval() -> u64 {
    60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = GithubConfig::default();
        assert_eq!(config.poll_interval_secs, 60);
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
    }
}
