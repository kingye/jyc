use serde::{Deserialize, Serialize};

fn default_ws_url() -> String {
    "wss://openws.work.weixin.qq.com".to_string()
}

fn default_heartbeat_interval() -> u64 {
    30
}

fn default_reconnect_delay() -> u64 {
    30
}

fn default_max_reconnect() -> u32 {
    10
}

fn default_true() -> bool {
    true
}

/// WeCom Smart Robot (wecom_bot) configuration for a channel.
///
/// Uses WebSocket long connection (`wss://openws.work.weixin.qq.com`)
/// instead of HTTP callback. No AES encryption/decryption is required.
///
/// Reference: doc 101463 - Smart Robot WebSocket Long Connection
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WecomBotConfig {
    /// Smart Robot ID (unique identifier assigned by WeCom)
    pub bot_id: String,

    /// Smart Robot long connection secret (NOT corp_secret)
    /// Get from "My smart robot" → "Long connection secret"
    pub secret: String,

    /// WebSocket server URL (default: wss://openws.work.weixin.qq.com)
    /// Can be changed for internal network proxy
    #[serde(default = "default_ws_url")]
    pub ws_url: String,

    /// Heartbeat interval in seconds (default: 30)
    /// Client sends `ping` every N seconds, server responds with `pong`
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,

    /// Reconnect delay in seconds after disconnect (default: 5)
    #[serde(default = "default_reconnect_delay")]
    pub reconnect_delay_secs: u64,

    /// Max reconnect attempts (default: 10)
    #[serde(default = "default_max_reconnect")]
    pub max_reconnect_attempts: u32,

    /// Whether to auto-reconnect on disconnect (default: true)
    #[serde(default = "default_true")]
    pub auto_reconnect: bool,
}

impl Default for WecomBotConfig {
    fn default() -> Self {
        Self {
            bot_id: String::new(),
            secret: String::new(),
            ws_url: default_ws_url(),
            heartbeat_interval_secs: default_heartbeat_interval(),
            reconnect_delay_secs: default_reconnect_delay(),
            max_reconnect_attempts: default_max_reconnect(),
            auto_reconnect: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_ws_url() {
        assert_eq!(default_ws_url(), "wss://openws.work.weixin.qq.com");
    }

    #[test]
    fn test_wecom_bot_config_defaults() {
        let config = WecomBotConfig::default();
        assert_eq!(config.ws_url, "wss://openws.work.weixin.qq.com");
        assert_eq!(config.heartbeat_interval_secs, 30);
        assert_eq!(config.reconnect_delay_secs, 5);
        assert_eq!(config.max_reconnect_attempts, 10);
        assert!(config.auto_reconnect);
    }

    #[test]
    fn test_wecom_bot_config_serde() {
        let toml = r#"
            bot_id = "my_bot_id"
            secret = "my_secret"
            ws_url = "wss://proxy.example.com"
            heartbeat_interval_secs = 45
            reconnect_delay_secs = 3
            max_reconnect_attempts = 20
            auto_reconnect = false
        "#;
        let config: WecomBotConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.bot_id, "my_bot_id");
        assert_eq!(config.secret, "my_secret");
        assert_eq!(config.ws_url, "wss://proxy.example.com");
        assert_eq!(config.heartbeat_interval_secs, 45);
        assert_eq!(config.reconnect_delay_secs, 3);
        assert_eq!(config.max_reconnect_attempts, 20);
        assert!(!config.auto_reconnect);
    }

    #[test]
    fn test_wecom_bot_config_minimal() {
        let toml = r#"
            bot_id = "bot123"
            secret = "secret456"
        "#;
        let config: WecomBotConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.bot_id, "bot123");
        assert_eq!(config.secret, "secret456");
        assert_eq!(config.ws_url, "wss://openws.work.weixin.qq.com");
        assert_eq!(config.heartbeat_interval_secs, 30);
        assert!(config.auto_reconnect);
    }
}
