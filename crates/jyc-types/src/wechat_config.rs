use serde::{Deserialize, Serialize};

/// WeChat-specific configuration for a channel.
///
/// Uses OpenILink WebSocket Bridge to connect to WeChat.
/// Both inbound and outbound messages share the same WebSocket connection.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WechatConfig {
    /// Hostname of the OpenILink server (e.g., "openilink.example.com").
    /// Do NOT include protocol prefix — the WebSocket URL is constructed
    /// as `wss://{base_url}/bot/v1/ws?token={token}` automatically.
    pub base_url: String,

    /// Access token for the OpenILink server
    pub token: String,

    /// WebSocket connection configuration
    #[serde(default)]
    pub websocket: WechatWebSocketConfig,
}

/// WebSocket connection configuration for WeChat.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WechatWebSocketConfig {
    /// Whether WebSocket is enabled (default: true)
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Reconnect delay in seconds (default: 5)
    #[serde(default = "default_reconnect_delay")]
    pub reconnect_delay_secs: u64,

    /// Maximum reconnect attempts (default: 10)
    #[serde(default = "default_max_reconnect_attempts")]
    pub max_reconnect_attempts: usize,
}

impl Default for WechatConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            token: String::new(),
            websocket: WechatWebSocketConfig::default(),
        }
    }
}

impl Default for WechatWebSocketConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            reconnect_delay_secs: default_reconnect_delay(),
            max_reconnect_attempts: default_max_reconnect_attempts(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_reconnect_delay() -> u64 {
    5
}

fn default_max_reconnect_attempts() -> usize {
    10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = WechatConfig::default();
        assert_eq!(config.base_url, "");
        assert_eq!(config.token, "");
        assert!(config.websocket.enabled);
        assert_eq!(config.websocket.reconnect_delay_secs, 5);
        assert_eq!(config.websocket.max_reconnect_attempts, 10);
    }

    #[test]
    fn test_config_deserialize() {
        let toml_str = r#"
            base_url = "openilink.example.com"
            token = "wechat_token_xxx"

            [websocket]
            enabled = false
            reconnect_delay_secs = 10
        "#;

        let config: WechatConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.base_url, "openilink.example.com");
        assert_eq!(config.token, "wechat_token_xxx");
        assert!(!config.websocket.enabled);
        assert_eq!(config.websocket.reconnect_delay_secs, 10);
    }
}
