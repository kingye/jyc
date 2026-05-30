use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_max_message_age_seconds() -> u64 {
    300
}

/// WeCom KF (Customer Service) channel-specific configuration.
///
/// Unlike the regular `WecomConfig`, the KF channel uses the
/// WeCom Customer Service API (`kf/sync_msg` and `kf/send_msg`)
/// instead of the external contact message API.
///
/// Fields:
/// - `token`: Token for callback message verification (from WeCom KF callback settings)
/// - `encoding_aes_key`: Encoding AES Key (with "=" padding) for message decryption
/// - `corp_id`: Corp ID / Enterprise ID
/// - `corp_secret`: Corp secret for access_token acquisition
/// - `open_kf_ids`: Optional list of KF account IDs to filter (empty = accept all)
/// - `cursor_store_path`: Optional file path for cursor persistence (JSON file)
/// - `metadata`: Additional metadata
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WecomKfConfig {
    /// Token for callback message verification (from WeCom KF callback settings).
    pub token: String,

    /// Encoding AES Key (with "=" padding) for message decryption.
    pub encoding_aes_key: String,

    /// Corp ID / Enterprise ID.
    pub corp_id: String,

    /// Corp secret for access_token acquisition.
    pub corp_secret: String,

    /// Optional list of KF account IDs to filter (empty = accept all).
    #[serde(default)]
    pub open_kf_ids: Vec<String>,

    /// Optional file path for cursor persistence (JSON file).
    #[serde(default)]
    pub cursor_store_path: Option<String>,

    /// Maximum age (in seconds) of messages to process. Messages older than
    /// this are skipped. Default: 300 (5 minutes). Set to 0 to disable.
    #[serde(default = "default_max_message_age_seconds")]
    pub max_message_age_seconds: u64,

    /// Additional metadata.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_full() {
        let toml_str = r#"
            token = "kf_token_xxx"
            encoding_aes_key = "abc123abc123abc123abc123abc123abc123abc123abc123abc12"
            corp_id = "ww1234567890abcdef"
            corp_secret = "my_corp_secret_value"
            open_kf_ids = ["kf001", "kf002"]
            cursor_store_path = "/tmp/kf_cursors.json"
        "#;

        let config: WecomKfConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.token, "kf_token_xxx");
        assert_eq!(
            config.encoding_aes_key,
            "abc123abc123abc123abc123abc123abc123abc123abc123abc12"
        );
        assert_eq!(config.corp_id, "ww1234567890abcdef");
        assert_eq!(config.corp_secret, "my_corp_secret_value");
        assert_eq!(config.open_kf_ids, vec!["kf001", "kf002"]);
        assert_eq!(
            config.cursor_store_path,
            Some("/tmp/kf_cursors.json".to_string())
        );
    }

    #[test]
    fn test_deserialize_minimal() {
        let toml_str = r#"
            token = "kf_token_xxx"
            encoding_aes_key = "abc123abc123abc123abc123abc123abc123abc123abc123abc12"
            corp_id = "ww1234567890abcdef"
            corp_secret = "my_corp_secret_value"
        "#;

        let config: WecomKfConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.token, "kf_token_xxx");
        assert!(config.open_kf_ids.is_empty());
        assert!(config.cursor_store_path.is_none());
        assert!(config.metadata.is_empty());
    }

    #[test]
    fn test_deserialize_with_metadata() {
        let toml_str = r#"
            token = "kf_token_xxx"
            encoding_aes_key = "abc123abc123abc123abc123abc123abc123abc123abc123abc12"
            corp_id = "ww1234567890abcdef"
            corp_secret = "my_corp_secret_value"

            [metadata]
            source = "wecom_kf"
            priority = 1
        "#;

        let config: WecomKfConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.metadata.get("source").and_then(|v| v.as_str()),
            Some("wecom_kf")
        );
        assert_eq!(
            config.metadata.get("priority").and_then(|v| v.as_i64()),
            Some(1)
        );
    }
}
