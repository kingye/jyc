//! Feishu API client (hand-rolled, no openlark SDK).
//!
//! Provides a high-level client for the Feishu (Lark) REST APIs that JYC
//! needs: sending messages, uploading/downloading files & images, and
//! resolving chat/user display names. Authentication uses the tenant
//! access token, cached with an expiry.
//!
//! The WebSocket long-connection (real-time events) still uses the
//! `openlark-client` SDK's `ws_client` (see `websocket.rs`).

use anyhow::{Context, Result};
use reqwest::multipart::{Form, Part};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use thiserror::Error;
use tokio::sync::RwLock;

use jyc_types::FeishuConfig;
use jyc_utils::helpers::truncate_str;

/// Feishu client errors.
#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum FeishuError {
    /// Client not initialized
    #[error("Feishu client not initialized. Call initialize() first")]
    NotInitialized,

    /// Configuration error
    #[error("Feishu configuration error: {0}")]
    ConfigError(String),

    /// API error
    #[error("Feishu API error: {0}")]
    ApiError(String),

    /// Network error
    #[error("Network error: {0}")]
    NetworkError(String),

    /// Authentication error
    #[error("Authentication error: {0}")]
    AuthError(String),
}

/// Feishu API client.
///
/// Uses raw `reqwest` calls with a cached tenant access token.
pub struct FeishuClient {
    config: FeishuConfig,
    http: reqwest::Client,
    /// Cache for chat names (chat_id -> name). Rarely changes, avoids repeated API calls.
    chat_name_cache: Arc<RwLock<HashMap<String, String>>>,
    /// Cache for user display names (open_id -> name).
    user_name_cache: Arc<RwLock<HashMap<String, String>>>,
    /// Cached tenant access token + expiry instant.
    token_cache: Arc<RwLock<Option<(String, Instant)>>>,
}

impl FeishuClient {
    /// Create a new Feishu client.
    pub fn new(config: FeishuConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            chat_name_cache: Arc::new(RwLock::new(HashMap::new())),
            user_name_cache: Arc::new(RwLock::new(HashMap::new())),
            token_cache: Arc::new(RwLock::new(None)),
        }
    }

    /// Pre-warm hook (kept for API compatibility).
    ///
    /// The hand-rolled client has nothing to initialize up front — the HTTP
    /// client is built eagerly and the tenant token is fetched lazily on the
    /// first authenticated call. Callers may call this for symmetry; it is a
    /// no-op.
    pub async fn initialize(&self) -> Result<()> {
        Ok(())
    }

    /// Get the current tenant access token (cached, refreshed on expiry).
    pub async fn get_token(&self) -> Result<String> {
        // Serve from cache if still valid (with a 60s safety buffer).
        {
            let cache = self.token_cache.read().await;
            if let Some((token, expires_at)) = cache.as_ref()
                && Instant::now() < *expires_at
            {
                return Ok(token.clone());
            }
        }

        let url = format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            self.config.base_url.trim_end_matches('/')
        );

        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "app_id": self.config.app_id,
                "app_secret": self.config.app_secret
            }))
            .send()
            .await
            .context("Failed to request Feishu tenant access token")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Feishu token response")?;

        let code = body["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            anyhow::bail!(
                "Feishu token request failed: code={}, msg={}",
                code,
                body["msg"].as_str().unwrap_or("unknown")
            );
        }

        let token = body["tenant_access_token"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("Feishu token response missing tenant_access_token"))?;

        // Cache with expiry (default 7200s when absent), minus a safety buffer.
        let expire_secs = body["expire"].as_u64().unwrap_or(7200);
        let expires_at =
            Instant::now() + std::time::Duration::from_secs(expire_secs.saturating_sub(60));
        *self.token_cache.write().await = Some((token.clone(), expires_at));

        Ok(token)
    }

    /// Send a message to a chat as an interactive card with markdown rendering.
    ///
    /// Uses Feishu's `"interactive"` message type which supports markdown
    /// formatting (bold, italic, code, lists, links) natively in the card UI.
    pub async fn send_text_message(
        &self,
        chat_id: &str,
        text: &str,
    ) -> Result<FeishuMessageResult> {
        let card_content = serde_json::json!({
            "elements": [
                {
                    "tag": "markdown",
                    "content": text
                }
            ]
        });
        self.send_message(chat_id, "interactive", &card_content.to_string())
            .await
    }

    /// Send a file message to a chat (after uploading via `upload_file()`).
    pub async fn send_file_message(
        &self,
        chat_id: &str,
        file_key: &str,
    ) -> Result<FeishuMessageResult> {
        let content = serde_json::json!({"file_key": file_key}).to_string();
        self.send_message(chat_id, "file", &content).await
    }

    /// Send an image message to a chat (after uploading via `upload_image()`).
    pub async fn send_image_message(
        &self,
        chat_id: &str,
        image_key: &str,
    ) -> Result<FeishuMessageResult> {
        let content = serde_json::json!({"image_key": image_key}).to_string();
        self.send_message(chat_id, "image", &content).await
    }

    /// Shared implementation for `POST /open-apis/im/v1/messages`.
    async fn send_message(
        &self,
        chat_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<FeishuMessageResult> {
        let token = self.get_token().await?;
        let url = format!(
            "{}/open-apis/im/v1/messages?receive_id_type=chat_id",
            self.config.base_url.trim_end_matches('/')
        );

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({
                "receive_id": chat_id,
                "msg_type": msg_type,
                "content": content,
            }))
            .send()
            .await
            .with_context(|| format!("Failed to send Feishu {msg_type} message"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Feishu send-message response")?;

        let code = body["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            anyhow::bail!(
                "Feishu send message failed: code={}, msg={}",
                code,
                body["msg"].as_str().unwrap_or("unknown")
            );
        }

        let message_id = body["data"]["message_id"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        tracing::info!(
            chat_id = %chat_id,
            message_id = %message_id,
            "Feishu {msg_type} message sent"
        );

        Ok(FeishuMessageResult { message_id })
    }

    /// Get the display name of a group chat (cached).
    ///
    /// Calls `GET /open-apis/im/v1/chats/:chat_id` on cache miss.
    /// Requires scope: `im:chat:readonly`. Degrades to `Ok(None)` on any
    /// API error (logged) so callers can fall back to the raw chat id.
    pub async fn get_chat_name(&self, chat_id: &str) -> Result<Option<String>> {
        // Check cache first
        {
            let cache = self.chat_name_cache.read().await;
            if let Some(name) = cache.get(chat_id) {
                return Ok(Some(name.clone()));
            }
        }

        // Cache miss — call Feishu API
        let token = match self.get_token().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(chat_id = %chat_id, error = %e, "Failed to get chat name, using fallback");
                return Ok(None);
            }
        };
        let url = format!(
            "{}/open-apis/im/v1/chats/{}",
            self.config.base_url.trim_end_matches('/'),
            chat_id
        );

        let resp = match self.http.get(&url).bearer_auth(token).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(chat_id = %chat_id, error = %e, "Failed to get chat name, using fallback");
                return Ok(None);
            }
        };
        let body: serde_json::Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(chat_id = %chat_id, error = %e, "Failed to parse chat info, using fallback");
                return Ok(None);
            }
        };

        let code = body["code"].as_i64().unwrap_or(0);
        if code != 0 {
            tracing::warn!(
                chat_id = %chat_id,
                code,
                msg = %body["msg"].as_str().unwrap_or("unknown"),
                "Failed to get chat name, using fallback"
            );
            return Ok(None);
        }

        let name = body["data"]["name"].as_str().map(|s| s.to_string());

        if let Some(ref name) = name {
            let mut cache = self.chat_name_cache.write().await;
            cache.insert(chat_id.to_string(), name.clone());
            tracing::debug!(chat_id = %chat_id, name = %name, "Chat name cached");
        } else {
            tracing::warn!(chat_id = %chat_id, "Chat info returned but name field missing");
        }

        Ok(name)
    }

    /// Get the display name of a user (cached).
    ///
    /// Calls `GET /open-apis/contact/v3/users/:user_id` on cache miss.
    /// Requires scope: `contact:user.base:readonly`. Degrades to `Ok(None)`
    /// on any API error (logged) so callers can fall back to the raw open_id.
    pub async fn get_user_name(&self, open_id: &str) -> Result<Option<String>> {
        // Check cache first
        {
            let cache = self.user_name_cache.read().await;
            if let Some(name) = cache.get(open_id) {
                return Ok(Some(name.clone()));
            }
        }

        // Cache miss — call Feishu API
        let token = match self.get_token().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(open_id = %open_id, error = %e, "Failed to get user name (check contact:user.base:readonly scope)");
                return Ok(None);
            }
        };
        let url = format!(
            "{}/open-apis/contact/v3/users/{}?user_id_type=open_id",
            self.config.base_url.trim_end_matches('/'),
            open_id
        );

        let resp = match self.http.get(&url).bearer_auth(token).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(open_id = %open_id, error = %e, "Failed to get user name (check contact:user.base:readonly scope)");
                return Ok(None);
            }
        };
        let body: serde_json::Value = match resp.json().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(open_id = %open_id, error = %e, "Failed to parse user info, using fallback");
                return Ok(None);
            }
        };

        let code = body["code"].as_i64().unwrap_or(0);
        if code != 0 {
            tracing::warn!(
                open_id = %open_id,
                code,
                msg = %body["msg"].as_str().unwrap_or("unknown"),
                "Failed to get user name (check contact:user.base:readonly scope)"
            );
            return Ok(None);
        }

        let name = body["data"]["user"]["name"].as_str().map(|s| s.to_string());

        if let Some(ref name) = name {
            let mut cache = self.user_name_cache.write().await;
            cache.insert(open_id.to_string(), name.clone());
            tracing::debug!(open_id = %open_id, name = %name, "User name cached");
        }

        Ok(name)
    }

    /// Upload a file to Feishu servers.
    ///
    /// Returns the `file_key` for use in file messages.
    /// Requires scope: `im:resource`.
    pub async fn upload_file(
        &self,
        path: &Path,
        filename: &str,
        file_type: &str,
    ) -> Result<String> {
        let token = self.get_token().await?;
        let file_bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("Failed to read file: {}", path.display()))?;

        let url = format!(
            "{}/open-apis/im/v1/files",
            self.config.base_url.trim_end_matches('/')
        );

        let form = Form::new()
            .text("file_type", file_type.to_string())
            .text("file_name", filename.to_string())
            .part(
                "file",
                Part::bytes(file_bytes).file_name(filename.to_string()),
            );

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await
            .context("Failed to upload file to Feishu")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Feishu file upload response")?;

        let code = body["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            anyhow::bail!(
                "Feishu file upload failed: code={}, msg={}",
                code,
                body["msg"].as_str().unwrap_or("unknown")
            );
        }

        let file_key = body["data"]["file_key"]
            .as_str()
            .context("Feishu file upload response missing file_key")?
            .to_string();

        tracing::info!(filename = %filename, file_type = %file_type, file_key = %file_key, "File uploaded to Feishu");

        Ok(file_key)
    }

    /// Upload an image to Feishu servers.
    ///
    /// Returns the `image_key` for use in image messages.
    /// Requires scope: `im:resource`.
    pub async fn upload_image(&self, path: &Path, filename: &str) -> Result<String> {
        let token = self.get_token().await?;
        let image_bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("Failed to read image: {}", path.display()))?;

        let url = format!(
            "{}/open-apis/im/v1/images",
            self.config.base_url.trim_end_matches('/')
        );

        let form = Form::new().text("image_type", "message").part(
            "file",
            Part::bytes(image_bytes).file_name(filename.to_string()),
        );

        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await
            .context("Failed to upload image to Feishu")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Feishu image upload response")?;

        let code = body["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            anyhow::bail!(
                "Feishu image upload failed: code={}, msg={}",
                code,
                body["msg"].as_str().unwrap_or("unknown")
            );
        }

        let image_key = body["data"]["image_key"]
            .as_str()
            .context("Feishu image upload response missing image_key")?
            .to_string();

        tracing::info!(filename = %filename, image_key = %image_key, "Image uploaded to Feishu");

        Ok(image_key)
    }

    /// Download a file from a Feishu message.
    ///
    /// Prefers the message resource endpoint, which requires both message_id
    /// and file_key. The standalone files endpoint (/im/v1/files/:file_key)
    /// only serves files uploaded by the app itself — chat message files fail
    /// with 234008 "The app is not the resource sender".
    ///
    /// Returns the file content as bytes.
    /// Validates that the response is actual file data, not an API error.
    pub async fn download_file(&self, file_key: &str, message_id: Option<&str>) -> Result<Vec<u8>> {
        let token = self.get_token().await?;
        let url = file_download_url(
            self.config.base_url.trim_end_matches('/'),
            file_key,
            message_id,
        );

        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .context("Failed to download file from Feishu")?;

        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let file_bytes = resp
            .bytes()
            .await
            .context("Failed to read file response body")?
            .to_vec();

        // API errors come back as JSON bodies, not file data
        if !status.is_success() || content_type.contains("application/json") {
            let body_str = String::from_utf8_lossy(&file_bytes);
            anyhow::bail!(
                "Feishu file download failed (HTTP {}): {}",
                status,
                truncate_str(&body_str, 300)
            );
        }

        if file_bytes.is_empty() {
            anyhow::bail!(
                "Feishu file download returned empty data for file_key={}",
                file_key
            );
        }

        tracing::debug!(
            "Downloaded file from Feishu: file_key = {}, size = {} bytes",
            file_key,
            file_bytes.len()
        );

        Ok(file_bytes)
    }

    /// Download an image from a Feishu message.
    ///
    /// Uses the message resource endpoint which requires both message_id and image_key.
    /// The standalone image endpoint (/im/v1/images/:image_key) returns 400 for
    /// chat message images.
    pub async fn download_image(
        &self,
        image_key: &str,
        message_id: Option<&str>,
    ) -> Result<Vec<u8>> {
        let token = self.get_token().await?;
        let base_url = self.config.base_url.trim_end_matches('/');

        // Use message resource endpoint if message_id is available (preferred)
        // Falls back to standalone image endpoint
        let url = if let Some(msg_id) = message_id {
            format!(
                "{}/open-apis/im/v1/messages/{}/resources/{}?type=image",
                base_url, msg_id, image_key
            )
        } else {
            format!(
                "{}/open-apis/im/v1/images/{}?type=image",
                base_url, image_key
            )
        };

        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .context("Failed to download image from Feishu")?;

        let status = resp.status();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let image_bytes = resp
            .bytes()
            .await
            .context("Failed to read image response body")?
            .to_vec();

        // Check for API error responses
        if !status.is_success() || content_type.contains("application/json") {
            let body_str = String::from_utf8_lossy(&image_bytes);
            anyhow::bail!(
                "Feishu image download failed (HTTP {}): {}",
                status,
                truncate_str(&body_str, 300)
            );
        }

        if image_bytes.is_empty() {
            anyhow::bail!(
                "Feishu image download returned empty data for image_key={}",
                image_key
            );
        }

        tracing::debug!(
            "Downloaded image from Feishu: image_key={}, size={} bytes, content_type={}",
            image_key,
            image_bytes.len(),
            content_type
        );

        Ok(image_bytes)
    }
}

/// Result of sending a Feishu message.
#[derive(Debug, Clone)]
pub struct FeishuMessageResult {
    pub message_id: String,
}

/// Build the download URL for a message file.
///
/// Chat-message files must go through the message resource endpoint — the
/// standalone `/im/v1/files/:file_key` endpoint only serves files uploaded by
/// the app itself (error 234008 "The app is not the resource sender").
fn file_download_url(base_url: &str, file_key: &str, message_id: Option<&str>) -> String {
    match message_id {
        Some(msg_id) => {
            format!("{base_url}/open-apis/im/v1/messages/{msg_id}/resources/{file_key}?type=file")
        }
        None => format!("{base_url}/open-apis/im/v1/files/{file_key}"),
    }
}

/// Map file extension to Feishu file_type string.
///
/// Feishu supports: "opus", "mp4", "pdf", "doc", "xls", "ppt", "stream".
/// Text/code files and unknown types default to "stream" (generic binary).
pub fn feishu_file_type(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "pdf" => "pdf",
        "doc" | "docx" => "doc",
        "xls" | "xlsx" => "xls",
        "ppt" | "pptx" => "ppt",
        "mp4" => "mp4",
        "opus" | "ogg" => "opus",
        _ => "stream",
    }
}

/// Check if a content_type represents an image.
pub fn is_image_content_type(content_type: &str) -> bool {
    content_type.starts_with("image/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::{FeishuConfig, WebSocketConfig};

    #[test]
    fn test_feishu_client_creation() {
        let config = FeishuConfig {
            app_id: "test_app_id".to_string(),
            app_secret: "test_app_secret".to_string(),
            base_url: "https://open.feishu.cn".to_string(),
            websocket: WebSocketConfig::default(),
            events: vec![],
            message_format: "markdown".to_string(),
            metadata: Default::default(),
        };

        let _client = FeishuClient::new(config);
    }

    #[test]
    fn test_file_download_url_prefers_message_resource_endpoint() {
        let url = file_download_url("https://open.feishu.cn", "file_v3_x", Some("om_123"));
        assert_eq!(
            url,
            "https://open.feishu.cn/open-apis/im/v1/messages/om_123/resources/file_v3_x?type=file"
        );
    }

    #[test]
    fn test_file_download_url_falls_back_without_message_id() {
        let url = file_download_url("https://open.feishu.cn", "file_v3_x", None);
        assert_eq!(
            url,
            "https://open.feishu.cn/open-apis/im/v1/files/file_v3_x"
        );
    }

    #[test]
    fn test_feishu_message_result() {
        let result = FeishuMessageResult {
            message_id: "test_message_123".to_string(),
        };

        assert_eq!(result.message_id, "test_message_123");

        let cloned = result.clone();
        assert_eq!(cloned.message_id, result.message_id);
    }
}
