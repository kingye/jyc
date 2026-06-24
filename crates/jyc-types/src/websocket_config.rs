use serde::{Deserialize, Serialize};

/// Configuration for a WebSocket channel.
///
/// The WebSocket server runs inside `jyc monitor` and accepts connections
/// from `jyc dashboard` chat panes.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebsocketConfig {
    /// TCP bind address for the WebSocket server (default: "127.0.0.1:9877")
    #[serde(default = "default_websocket_bind")]
    pub bind: String,
}

impl Default for WebsocketConfig {
    fn default() -> Self {
        Self {
            bind: default_websocket_bind(),
        }
    }
}

fn default_websocket_bind() -> String {
    "127.0.0.1:9877".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_bind() {
        let config = WebsocketConfig::default();
        assert_eq!(config.bind, "127.0.0.1:9877");
    }

    #[test]
    fn test_deserialize_with_defaults() {
        let toml = r#"
[websocket]
"#;
        let parsed: toml::Value = toml::from_str(toml).unwrap();
        let ws = parsed["websocket"].clone();
        let config: WebsocketConfig = ws.try_into().unwrap();
        assert_eq!(config.bind, "127.0.0.1:9877");
    }

    #[test]
    fn test_deserialize_custom_bind() {
        let toml = r#"
bind = "0.0.0.0:8080"
"#;
        let config: WebsocketConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.bind, "0.0.0.0:8080");
    }

    #[test]
    fn test_serialize_roundtrip() {
        let config = WebsocketConfig {
            bind: "192.168.1.1:9000".to_string(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: WebsocketConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.bind, "192.168.1.1:9000");
    }
}
