use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Feishu-specific configuration for a channel.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeishuConfig {
    /// Feishu app ID
    pub app_id: String,

    /// Feishu app secret
    pub app_secret: String,

    /// Base URL for API calls (default: https://open.feishu.cn)
    #[serde(default = "default_base_url")]
    pub base_url: String,

    /// WebSocket configuration
    #[serde(default)]
    pub websocket: WebSocketConfig,

    /// Event types to subscribe to
    #[serde(default = "default_events")]
    pub events: Vec<String>,

    /// Message format preference
    #[serde(default = "default_message_format")]
    pub message_format: String,

    /// Additional metadata
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// WebSocket connection configuration for Feishu.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebSocketConfig {
    /// Whether WebSocket is enabled (default: true)
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Reconnect delay in seconds (default: 5)
    #[serde(default = "default_reconnect_delay")]
    pub reconnect_delay_secs: u64,

    /// Maximum reconnect attempts (default: 10)
    #[serde(default = "default_max_reconnect_attempts")]
    pub max_reconnect_attempts: usize,

    /// Heartbeat interval in seconds (default: 30)
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
}

// Default implementations
impl Default for FeishuConfig {
    fn default() -> Self {
        Self {
            app_id: String::new(),
            app_secret: String::new(),
            base_url: default_base_url(),
            websocket: WebSocketConfig::default(),
            events: default_events(),
            message_format: default_message_format(),
            metadata: HashMap::new(),
        }
    }
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            reconnect_delay_secs: default_reconnect_delay(),
            max_reconnect_attempts: default_max_reconnect_attempts(),
            heartbeat_interval_secs: default_heartbeat_interval(),
        }
    }
}

// Default value functions
fn default_base_url() -> String {
    "https://open.feishu.cn".to_string()
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

fn default_heartbeat_interval() -> u64 {
    30
}

fn default_events() -> Vec<String> {
    vec![
        "im.message.receive_v1".to_string(),
        "im.chat.member.bot.added_v1".to_string(),
    ]
}

fn default_message_format() -> String {
    "markdown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = FeishuConfig::default();
        assert_eq!(config.base_url, "https://open.feishu.cn");
        assert!(config.websocket.enabled);
        assert_eq!(config.websocket.reconnect_delay_secs, 5);
        assert_eq!(config.websocket.max_reconnect_attempts, 10);
        assert_eq!(config.websocket.heartbeat_interval_secs, 30);
        assert!(config.events.contains(&"im.message.receive_v1".to_string()));
        assert_eq!(config.message_format, "markdown");
    }

    #[test]
    fn test_config_deserialize() {
        let toml_str = r#"
            app_id = "cli_xxxxxxxx"
            app_secret = "xxxxxxxxxxxxxxxx"
            base_url = "https://open.larksuite.com"
            
            [websocket]
            enabled = false
            reconnect_delay_secs = 10
        "#;

        let config: FeishuConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.app_id, "cli_xxxxxxxx");
        assert_eq!(config.app_secret, "xxxxxxxxxxxxxxxx");
        assert_eq!(config.base_url, "https://open.larksuite.com");
        assert!(!config.websocket.enabled);
        assert_eq!(config.websocket.reconnect_delay_secs, 10);
    }
}
