use serde::{Deserialize, Serialize};
use crate::feishu_config::WebSocketConfig;

/// OpeniLink-specific configuration for a channel.
///
/// Connects to an OpeniLink Hub instance via WebSocket for receiving
/// messages and HTTP API for sending replies.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OpenilinkConfig {
    /// API key for authenticating with the OpeniLink Hub
    pub api_key: String,

    /// Base URL of the OpeniLink Hub (e.g., "https://hub.example.com")
    pub hub_url: String,

    /// WebSocket configuration for receiving messages
    #[serde(default)]
    pub websocket: WebSocketConfig,

    /// Max size of the context_token cache (maps sender wxid -> token)
    /// Default: 1000
    #[serde(default = "default_cache_size")]
    pub context_token_cache_size: usize,
}

impl Default for OpenilinkConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            hub_url: String::new(),
            websocket: WebSocketConfig::default(),
            context_token_cache_size: default_cache_size(),
        }
    }
}

fn default_cache_size() -> usize {
    1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = OpenilinkConfig::default();
        assert_eq!(config.api_key, "");
        assert_eq!(config.hub_url, "");
        assert!(config.websocket.enabled);
        assert_eq!(config.websocket.reconnect_delay_secs, 5);
        assert_eq!(config.context_token_cache_size, 1000);
    }

    #[test]
    fn test_config_deserialize() {
        let toml_str = r#"
            api_key = "sk-xxxxxxxx"
            hub_url = "https://hub.example.com"

            [websocket]
            enabled = false
            reconnect_delay_secs = 10
        "#;

        let config: OpenilinkConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.api_key, "sk-xxxxxxxx");
        assert_eq!(config.hub_url, "https://hub.example.com");
        assert!(!config.websocket.enabled);
        assert_eq!(config.websocket.reconnect_delay_secs, 10);
    }

    #[test]
    fn test_config_deserialize_with_cache_size() {
        let toml_str = r#"
            api_key = "sk-xxxxxxxx"
            hub_url = "https://hub.example.com"
            context_token_cache_size = 500
        "#;

        let config: OpenilinkConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.context_token_cache_size, 500);
    }
}
