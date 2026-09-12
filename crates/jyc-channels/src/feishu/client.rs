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
    /// Feishu card markdown renders GFM tables as raw pipe text, so
    /// tables become native card table components first — see
    /// [`build_card_content`].
    pub async fn send_text_message(
        &self,
        chat_id: &str,
        text: &str,
    ) -> Result<FeishuMessageResult> {
        let card_content = build_card_content(text);
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

    /// Replace the content of an already-sent interactive-card message with
    /// the given card JSON.
    ///
    /// Calls `PATCH /open-apis/im/v1/messages/{message_id}`. Only messages
    /// sent by the bot itself can be updated. Used by the progress watcher
    /// to refresh the live status card as processing advances.
    pub async fn update_card_message(
        &self,
        message_id: &str,
        card: &serde_json::Value,
    ) -> Result<()> {
        let token = self.get_token().await?;
        let url = update_message_url(&self.config.base_url, message_id);
        let body = serde_json::json!({
            "content": card.to_string(),
        });

        let resp = self
            .http
            .patch(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .context("Failed to send Feishu update-message request")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Feishu update-message response")?;

        if body["code"].as_i64() != Some(0) {
            anyhow::bail!(
                "Feishu update_message failed: code={}, msg={}",
                body["code"].as_i64().unwrap_or(-1),
                body["msg"].as_str().unwrap_or("unknown")
            );
        }

        Ok(())
    }
}

/// Result of sending a Feishu message.
#[derive(Debug, Clone)]
pub struct FeishuMessageResult {
    pub message_id: String,
}

/// Build the URL for editing a message's content (`update_card_message`).
///
/// `PATCH /open-apis/im/v1/messages/{message_id}`
fn update_message_url(base_url: &str, message_id: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    format!("{base_url}/open-apis/im/v1/messages/{message_id}")
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
    fn test_update_message_url_builds_expected_path() {
        let url = update_message_url("https://open.feishu.cn", "om_abc123");
        assert_eq!(
            url,
            "https://open.feishu.cn/open-apis/im/v1/messages/om_abc123"
        );
        let url = update_message_url("https://open.feishu.cn/", "om_abc123");
        assert_eq!(
            url,
            "https://open.feishu.cn/open-apis/im/v1/messages/om_abc123"
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

/// Feishu card markdown renders GFM tables as raw pipe text, so tables
/// become native Feishu card table components: text is split into
/// alternating markdown and table segments (see [`build_card_content`]).
/// Box-drawing ASCII art (tree diagrams, box sketches) has no card
/// equivalent and is fenced in a code block — monospace preserves its
/// alignment on desktop (mobile Feishu wraps long code lines; accepted,
/// such art is usually narrow). Content already inside a code fence is
/// left untouched.
fn split_segments(text: &str) -> Vec<Segment> {
    let lines: Vec<&str> = text.lines().collect();
    let mut segments = Vec::new();
    let mut md: Vec<&str> = Vec::new();
    let mut in_fence = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            md.push(line);
            i += 1;
            continue;
        }
        if !in_fence && is_gfm_table_start(&lines, i) {
            let mut end = i;
            while end < lines.len() && is_table_line(lines[end]) {
                end += 1;
            }
            if !md.is_empty() {
                segments.push(Segment::Markdown(md.join("\n")));
                md.clear();
            }
            segments.push(Segment::Table(parse_gfm_table(&lines[i..end])));
            i = end;
            continue;
        }
        if !in_fence && is_box_line(trimmed) {
            let mut end = i;
            while end < lines.len() && is_box_line(lines[end].trim_start()) {
                end += 1;
            }
            // Require a run: a single stray box char is prose, not art.
            if end - i >= 2 {
                md.push("```");
                md.extend_from_slice(&lines[i..end]);
                md.push("```");
            } else {
                md.push(line);
            }
            i = end;
            continue;
        }
        md.push(line);
        i += 1;
    }
    if !md.is_empty() {
        segments.push(Segment::Markdown(md.join("\n")));
    }
    segments
}

/// A parsed GFM table: header cells, per-column alignment from the
/// separator row, and data rows.
struct GfmTable {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
}

/// Horizontal cell alignment, from the `:` markers of a GFM separator cell.
#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

/// Text split around the GFM tables it contains.
enum Segment {
    Markdown(String),
    Table(GfmTable),
}

/// Parse a GFM table block (header row, separator row, data rows).
fn parse_gfm_table(block: &[&str]) -> GfmTable {
    fn cells(line: &str) -> Vec<String> {
        line.trim()
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_string())
            .collect()
    }
    GfmTable {
        headers: cells(block[0]),
        aligns: cells(block[1])
            .iter()
            .map(|c| match (c.starts_with(':'), c.ends_with(':')) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
            .collect(),
        rows: block[2..].iter().map(|l| cells(l)).collect(),
    }
}

/// A (trimmed) line belonging to a GFM table block.
fn is_table_line(line: &str) -> bool {
    line.trim().starts_with('|')
}

/// The `|---|---|` separator row of a GFM table.
fn is_separator_row(line: &str) -> bool {
    let t = line.trim();
    is_table_line(t) && t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// A GFM table block starts at `i` when a header row is immediately
/// followed by the separator row.
fn is_gfm_table_start(lines: &[&str], i: usize) -> bool {
    i + 1 < lines.len() && is_table_line(lines[i]) && is_separator_row(lines[i + 1])
}

/// A box-drawing art row (tree diagrams, box sketches) opens with a box char.
fn is_box_line(trimmed: &str) -> bool {
    trimmed
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '┌' | '├' | '│' | '└'))
}

/// Build a Feishu card table component (card JSON 1.0, client V7.4+).
fn table_element(table: &GfmTable) -> serde_json::Value {
    let columns: Vec<_> = table
        .headers
        .iter()
        .enumerate()
        .map(|(j, name)| {
            serde_json::json!({
                "name": format!("c{j}"),
                "display_name": name,
                "data_type": "text",
                "horizontal_align": match table.aligns.get(j) {
                    Some(Align::Right) => "right",
                    Some(Align::Center) => "center",
                    _ => "left",
                },
                "width": "auto",
            })
        })
        .collect();
    let rows: Vec<_> = table
        .rows
        .iter()
        .map(|row| {
            let mut cells = serde_json::Map::new();
            for (j, _) in table.headers.iter().enumerate() {
                cells.insert(
                    format!("c{j}"),
                    serde_json::Value::String(row.get(j).cloned().unwrap_or_default()),
                );
            }
            serde_json::Value::Object(cells)
        })
        .collect();
    serde_json::json!({
        "tag": "table",
        "page_size": 10,
        "row_height": "low",
        "header_style": { "background_style": "grey" },
        "columns": columns,
        "rows": rows,
    })
}

/// Feishu allows at most 5 table components per card and 50 columns per
/// table; tables past either limit degrade to a fenced text table.
const MAX_TABLE_ELEMENTS: usize = 5;
const MAX_TABLE_COLUMNS: usize = 50;

/// Build interactive-card content for a markdown text: GFM tables become
/// native table components (mobile-responsive, unlike code blocks), the
/// rest stays markdown.
fn build_card_content(text: &str) -> serde_json::Value {
    let mut elements = Vec::new();
    let mut tables = 0;
    for segment in split_segments(text) {
        let element = match segment {
            Segment::Markdown(md) if md.trim().is_empty() => continue,
            Segment::Markdown(md) => serde_json::json!({ "tag": "markdown", "content": md }),
            Segment::Table(t)
                if tables < MAX_TABLE_ELEMENTS && t.headers.len() <= MAX_TABLE_COLUMNS =>
            {
                tables += 1;
                table_element(&t)
            }
            Segment::Table(t) => {
                serde_json::json!({ "tag": "markdown", "content": render_gfm_table(&t) })
            }
        };
        elements.push(element);
    }
    if elements.is_empty() {
        elements.push(serde_json::json!({ "tag": "markdown", "content": text }));
    }
    serde_json::json!({ "elements": elements })
}

/// Render a parsed table as a display-width-aligned plain-text table
/// inside a fenced code block — the degradation path for Feishu's table
/// component limits.
fn render_gfm_table(table: &GfmTable) -> String {
    use unicode_width::UnicodeWidthStr;
    let mut widths: Vec<usize> = table
        .headers
        .iter()
        .map(|h| UnicodeWidthStr::width(h.as_str()))
        .collect();
    for row in &table.rows {
        for (j, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(j) {
                *w = (*w).max(UnicodeWidthStr::width(cell.as_str()));
            }
        }
    }
    let mut out = String::from("```\n");
    let mut rows = Vec::with_capacity(table.rows.len() + 1);
    rows.push(&table.headers);
    rows.extend(&table.rows);
    for row in rows {
        out.push('|');
        for (j, w) in widths.iter().enumerate() {
            let cell = row.get(j).map(String::as_str).unwrap_or("");
            out.push(' ');
            out.push_str(cell);
            out.push_str(&" ".repeat(w - UnicodeWidthStr::width(cell)));
            out.push_str(" |");
        }
        out.push('\n');
    }
    out.push_str("```");
    out
}

#[cfg(test)]
mod card_content_tests {
    use super::build_card_content;

    #[test]
    fn gfm_table_becomes_native_table_element() {
        let text = "before\n| 模型 | calls |\n|:-----|------:|\n| kimi | 3 |\nafter";
        let card = build_card_content(text);
        let els = card["elements"].as_array().unwrap();
        assert_eq!(els.len(), 3);
        assert_eq!(els[0]["tag"], "markdown");
        assert_eq!(els[0]["content"], "before");
        assert_eq!(els[1]["tag"], "table");
        assert_eq!(els[1]["page_size"], 10);
        assert_eq!(els[1]["columns"][0]["display_name"], "模型");
        assert_eq!(els[1]["columns"][0]["horizontal_align"], "left");
        assert_eq!(els[1]["columns"][1]["display_name"], "calls");
        assert_eq!(els[1]["columns"][1]["horizontal_align"], "right");
        assert_eq!(els[1]["rows"][0]["c0"], "kimi");
        assert_eq!(els[1]["rows"][0]["c1"], "3");
        assert_eq!(els[2]["tag"], "markdown");
        assert_eq!(els[2]["content"], "after");
    }

    #[test]
    fn sixth_table_degrades_to_fenced_text() {
        let one = "| a |\n|---|\n| 1 |";
        let text = [one; 6].join("\n\n");
        let card = build_card_content(&text);
        let els = card["elements"].as_array().unwrap();
        let tables = els.iter().filter(|e| e["tag"] == "table").count();
        assert_eq!(tables, 5);
        let last = els.last().unwrap();
        assert_eq!(last["tag"], "markdown");
        assert!(last["content"].as_str().unwrap().starts_with("```\n| a |"));
    }

    #[test]
    fn row_cells_shorter_than_headers_are_padded_empty() {
        let card = build_card_content("| a | b |\n|---|---|\n| 1 |");
        assert_eq!(card["elements"][0]["rows"][0]["c1"], "");
    }

    #[test]
    fn box_art_stays_fenced_in_markdown() {
        let text = "header\n┌──┬──┐\n│ a│ b│\n└──┴──┘\nfooter";
        let card = build_card_content(text);
        let els = card["elements"].as_array().unwrap();
        assert_eq!(els.len(), 1);
        assert_eq!(
            els[0]["content"],
            "header\n```\n┌──┬──┐\n│ a│ b│\n└──┴──┘\n```\nfooter"
        );
    }

    #[test]
    fn table_inside_existing_fence_is_untouched() {
        let text = "```\n| a | b |\n|---|---|\n| 1 | 2 |\n```";
        let card = build_card_content(text);
        let els = card["elements"].as_array().unwrap();
        assert_eq!(els.len(), 1);
        assert_eq!(els[0]["content"], text);
    }

    #[test]
    fn plain_text_passes_through_as_single_markdown_element() {
        let card = build_card_content("just | one pipe\nand a stray ─ dash");
        let els = card["elements"].as_array().unwrap();
        assert_eq!(els.len(), 1);
        assert_eq!(els[0]["tag"], "markdown");
        assert_eq!(els[0]["content"], "just | one pipe\nand a stray ─ dash");
    }
}
