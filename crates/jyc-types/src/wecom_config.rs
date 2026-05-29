use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// WeCom (企业微信) global configuration — shared HTTP server settings.
///
/// All WeCom channels share a single HTTP server for receiving callback messages.
/// The server listens on `bind_addr` and routes requests by path `/webhook/{channel_name}`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WecomGlobalConfig {
    /// TCP bind address for the shared HTTP server (default: "127.0.0.1:10001")
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
}

impl Default for WecomGlobalConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_bind_addr(),
        }
    }
}

fn default_bind_addr() -> String {
    "127.0.0.1:10001".to_string()
}

/// WeCom (企业微信) channel-specific configuration.
///
/// Contains credentials for receiving callback messages (token, encoding_aes_key, corp_id)
/// and the Bot webhook URL for sending messages.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WecomConfig {
    /// Token for callback message verification (from WeCom Bot callback settings)
    pub token: String,

    /// Encoding AES Key (with "=" padding) for message decryption
    pub encoding_aes_key: String,

    /// Corp ID / Enterprise ID for message decryption
    pub corp_id: String,

    /// Bot webhook URL for sending outgoing messages
    /// (e.g., "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=xxx")
    pub webhook_url: String,

    /// Additional metadata
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_global_config() {
        let config = WecomGlobalConfig::default();
        assert_eq!(config.bind_addr, "127.0.0.1:10001");
    }

    #[test]
    fn test_config_deserialize() {
        let toml_str = r#"
            token = "wecom_token_xxx"
            encoding_aes_key = "abc123abc123abc123abc123abc123abc123abc123abc123abc12"
            corp_id = "ww1234567890abcdef"
            webhook_url = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=xxx-xxx-xxx"
        "#;

        let config: WecomConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.token, "wecom_token_xxx");
        assert_eq!(
            config.encoding_aes_key,
            "abc123abc123abc123abc123abc123abc123abc123abc123abc12"
        );
        assert_eq!(config.corp_id, "ww1234567890abcdef");
        assert_eq!(
            config.webhook_url,
            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=xxx-xxx-xxx"
        );
    }

    #[test]
    fn test_global_config_deserialize() {
        let toml_str = r#"
            bind_addr = "0.0.0.0:20001"
        "#;

        let config: WecomGlobalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.bind_addr, "0.0.0.0:20001");
    }
}
