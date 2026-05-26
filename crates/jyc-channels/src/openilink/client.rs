//! HTTP client for OpeniLink Hub API.
//!
//! This module wraps `reqwest::Client` to provide high-level methods
//! for interacting with the OpeniLink Hub REST API. It handles
//! authentication via `Authorization: Bearer {api_key}` header.

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use std::sync::Arc;

use jyc_types::OpenilinkConfig;

use super::types::{
    HubConfig, SendMessageRequest, SendMessageResponse, SendTypingRequest, SendTypingResponse,
};

/// HTTP client for OpeniLink Hub API.
///
/// Provides methods for sending messages, typing indicators, and
/// retrieving hub configuration.
pub struct OpenilinkClient {
    config: OpenilinkConfig,
    client: Arc<reqwest::Client>,
}

impl OpenilinkClient {
    /// Create a new OpeniLink client.
    pub fn new(config: OpenilinkConfig) -> Self {
        let mut headers = HeaderMap::new();
        let api_key = config.api_key.clone();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", api_key))
                .unwrap_or_else(|_| HeaderValue::from_static("Bearer ")),
        );
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create reqwest client");

        Self {
            config,
            client: Arc::new(client),
        }
    }

    /// Get the base URL for API calls.
    fn api_url(&self, path: &str) -> String {
        let base = self.config.hub_url.trim_end_matches('/');
        format!("{}{}", base, path)
    }

    /// Send a text message to a WeChat user.
    ///
    /// If `context_token` is provided, the message is sent as a reply
    /// (retaining the conversation context). Otherwise, it's sent as
    /// an active push.
    pub async fn send_message(
        &self,
        to_user_id: &str,
        content: &str,
        context_token: Option<&str>,
    ) -> Result<SendMessageResponse> {
        let url = self.api_url("/api/v1/channels/send");

        let request = SendMessageRequest {
            to_user_id: to_user_id.to_string(),
            content: content.to_string(),
            context_token: context_token.map(|s| s.to_string()),
        };

        let resp = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("Failed to send message to {to_user_id}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Hub API returned error (status {status}): {body}"
            );
        }

        let send_resp: SendMessageResponse = resp
            .json()
            .await
            .context("Failed to parse send message response")?;

        if send_resp.code != 0 {
            anyhow::bail!(
                "Hub API error (code {}): {}",
                send_resp.code,
                send_resp.message.as_deref().unwrap_or("unknown")
            );
        }

        Ok(send_resp)
    }

    /// Send a typing indicator to a WeChat user.
    ///
    /// `action` can be "TypingBegin" or "TypingEnd".
    pub async fn send_typing(
        &self,
        to_user_id: &str,
        typing_ticket: Option<&str>,
        action: &str,
    ) -> Result<SendTypingResponse> {
        let url = self.api_url("/api/v1/channels/typing");

        let request = SendTypingRequest {
            to_user_id: to_user_id.to_string(),
            typing_ticket: typing_ticket.map(|s| s.to_string()),
            action: action.to_string(),
        };

        let resp = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("Failed to send typing indicator to {to_user_id}"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Hub API returned error (status {status}): {body}"
            );
        }

        let typing_resp: SendTypingResponse = resp
            .json()
            .await
            .context("Failed to parse typing response")?;

        if typing_resp.code != 0 {
            anyhow::bail!(
                "Hub API error (code {}): {}",
                typing_resp.code,
                typing_resp.message.as_deref().unwrap_or("unknown")
            );
        }

        Ok(typing_resp)
    }

    /// Retrieve hub configuration and connection status.
    ///
    /// This is used during adapter `connect()` to verify the API key is valid.
    pub async fn get_config(&self) -> Result<HubConfig> {
        let url = self.api_url("/api/v1/channels/config");

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to get hub config")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Hub API returned error (status {status}): {body}"
            );
        }

        let config: HubConfig = resp
            .json()
            .await
            .context("Failed to parse hub config response")?;

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let config = OpenilinkConfig {
            api_key: "sk-test".to_string(),
            hub_url: "https://hub.example.com".to_string(),
            ..OpenilinkConfig::default()
        };
        let client = OpenilinkClient::new(config);
        // Verify API URL construction
        assert_eq!(
            client.api_url("/api/v1/channels/send"),
            "https://hub.example.com/api/v1/channels/send"
        );
    }

    #[test]
    fn test_api_url_with_trailing_slash() {
        let config = OpenilinkConfig {
            api_key: "sk-test".to_string(),
            hub_url: "https://hub.example.com/".to_string(),
            ..OpenilinkConfig::default()
        };
        let client = OpenilinkClient::new(config);
        assert_eq!(
            client.api_url("/api/v1/channels/send"),
            "https://hub.example.com/api/v1/channels/send"
        );
    }

    #[test]
    fn test_send_message_request_serialization() {
        let req = SendMessageRequest {
            to_user_id: "wxid_abc".to_string(),
            content: "Hello!".to_string(),
            context_token: Some("token_123".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""to_user_id":"wxid_abc""#));
        assert!(json.contains(r#""context_token":"token_123""#));
    }

    #[test]
    fn test_send_message_request_no_token() {
        let req = SendMessageRequest {
            to_user_id: "wxid_abc".to_string(),
            content: "Hello!".to_string(),
            context_token: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("context_token"));
    }

    #[test]
    fn test_send_message_response_parse() {
        let json = r#"{
            "code": 0,
            "message": "success",
            "message_id": "msg_001"
        }"#;
        let resp: SendMessageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.code, 0);
        assert_eq!(resp.message_id.unwrap(), "msg_001");
    }

    #[test]
    fn test_send_message_response_error() {
        let json = r#"{
            "code": 401,
            "message": "invalid api key"
        }"#;
        let resp: SendMessageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.code, 401);
        assert_eq!(resp.message.unwrap(), "invalid api key");
    }

    #[test]
    fn test_hub_config_parse() {
        let json = r#"{
            "version": "1.2.0",
            "connected": true,
            "wechat_user": {
                "wxid": "wxid_xxxxx",
                "nickname": "My Bot",
                "avatar": "https://example.com/avatar.png"
            },
            "features": ["text", "image"]
        }"#;
        let config: HubConfig = serde_json::from_str(json).unwrap();
        assert!(config.connected);
        assert_eq!(config.version.unwrap(), "1.2.0");
        let user = config.wechat_user.unwrap();
        assert_eq!(user.wxid.unwrap(), "wxid_xxxxx");
    }
}
