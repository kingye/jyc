//! Feishu API client wrapper.
//!
//! This module provides a high-level client for Feishu API interactions
//! using the openlark SDK.

use anyhow::{Context, Result};
use open_lark::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;

use super::config::FeishuConfig;

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

/// Feishu API client wrapper.
///
/// Wraps the openlark `Client` and provides high-level methods for
/// sending messages and managing authentication.
pub struct FeishuClient {
    config: FeishuConfig,
    client: Arc<RwLock<Option<Client>>>,
    /// Cache for chat names (chat_id -> name). Rarely changes, avoids repeated API calls.
    chat_name_cache: Arc<RwLock<HashMap<String, String>>>,
    /// Cache for user display names (open_id -> name).
    user_name_cache: Arc<RwLock<HashMap<String, String>>>,
}

impl FeishuClient {
    /// Create a new Feishu client.
    pub fn new(config: FeishuConfig) -> Self {
        Self {
            config,
            client: Arc::new(RwLock::new(None)),
            chat_name_cache: Arc::new(RwLock::new(HashMap::new())),
            user_name_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Initialize the openlark client (lazy — only called on first use).
    pub async fn initialize(&self) -> Result<()> {
        let mut client = self.client.write().await;
        if client.is_none() {
            let openlark_client = Client::builder()
                .app_id(&self.config.app_id)
                .app_secret(&self.config.app_secret)
                .base_url(&self.config.base_url)
                .build()
                .map_err(|e| anyhow::anyhow!("Failed to build openlark client: {e}"))?;

            tracing::info!(
                app_id = %self.config.app_id,
                "Feishu client initialized"
            );
            *client = Some(openlark_client);
        }
        Ok(())
    }

    /// Get the internal openlark client.
    async fn get_client(&self) -> Result<Client> {
        let client_guard = self.client.read().await;
        client_guard
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!(FeishuError::NotInitialized))
    }

    /// Get the openlark core config (for use with IM APIs and AuthService).
    async fn get_core_config(&self) -> Result<open_lark::core::config::Config> {
        let client = self.get_client().await?;
        Ok(client.api_config().clone())
    }

    /// Get the current tenant access token via direct HTTP request.
    ///
    /// Calls Feishu's internal app token endpoint directly instead of
    /// using the openlark SDK (which returns empty responses for tenant tokens).
    pub async fn get_token(&self) -> Result<String> {
        let http = reqwest::Client::new();
        let url = format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            self.config.base_url.trim_end_matches('/')
        );

        let resp = http
            .post(&url)
            .json(&serde_json::json!({
                "app_id": self.config.app_id,
                "app_secret": self.config.app_secret
            }))
            .send()
            .await
            .context("Failed to request Feishu tenant access token")?;

        let body: serde_json::Value = resp.json().await
            .context("Failed to parse Feishu token response")?;

        let code = body["code"].as_i64().unwrap_or(-1);
        if code != 0 {
            anyhow::bail!(
                "Feishu token request failed: code={}, msg={}",
                code,
                body["msg"].as_str().unwrap_or("unknown")
            );
        }

        body["tenant_access_token"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("Feishu token response missing tenant_access_token"))
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
        let core_config = self.get_core_config().await?;

        use open_lark::communication::im::im::v1::message::create::{
            CreateMessageBody, CreateMessageRequest,
        };
        use open_lark::communication::im::im::v1::message::models::ReceiveIdType;

        // Build interactive card with markdown element
        let card_content = serde_json::json!({
            "elements": [
                {
                    "tag": "markdown",
                    "content": text
                }
            ]
        });

        let body = CreateMessageBody {
            receive_id: chat_id.to_string(),
            msg_type: "interactive".to_string(),
            content: card_content.to_string(),
            uuid: None,
        };

        let resp = CreateMessageRequest::new(core_config)
            .receive_id_type(ReceiveIdType::ChatId)
            .execute(body)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send Feishu message: {e}"))?;

        // Extract message_id from response JSON
        let message_id = resp
            .get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();

        tracing::info!(
            chat_id = %chat_id,
            message_id = %message_id,
            text_len = text.len(),
            "Feishu card message sent"
        );

        Ok(FeishuMessageResult { message_id })
    }

    /// Get the display name of a group chat (cached).
    ///
    /// Calls `GET /open-apis/im/v1/chats/:chat_id` on cache miss.
    /// Requires scope: `im:chat:readonly`.
    pub async fn get_chat_name(&self, chat_id: &str) -> Result<Option<String>> {
        // Check cache first
        {
            let cache = self.chat_name_cache.read().await;
            if let Some(name) = cache.get(chat_id) {
                return Ok(Some(name.clone()));
            }
        }

        // Cache miss — call Feishu API
        let core_config = self.get_core_config().await?;

        use open_lark::communication::im::im::v1::chat::get::GetChatRequest;

        let resp = GetChatRequest::new(core_config)
            .chat_id(chat_id)
            .execute()
            .await;

        match resp {
            Ok(data) => {
                tracing::debug!(chat_id = %chat_id, response = %data, "Chat info API response");
                // extract_response_data already unwraps the outer "data" envelope,
                // so `data` is the inner object directly (e.g., {"name": "...", ...})
                let name = data
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_string());

                if let Some(ref name) = name {
                    let mut cache = self.chat_name_cache.write().await;
                    cache.insert(chat_id.to_string(), name.clone());
                    tracing::debug!(chat_id = %chat_id, name = %name, "Chat name cached");
                } else {
                    tracing::warn!(chat_id = %chat_id, "Chat info returned but name field missing");
                }

                Ok(name)
            }
            Err(e) => {
                tracing::warn!(
                    chat_id = %chat_id,
                    error = %e,
                    "Failed to get chat name, using fallback"
                );
                Ok(None)
            }
        }
    }

    /// Get the display name of a user (cached).
    ///
    /// Calls `GET /open-apis/contact/v3/users/:user_id` on cache miss.
    /// Requires scope: `contact:user.base:readonly`.
    pub async fn get_user_name(&self, open_id: &str) -> Result<Option<String>> {
        // Check cache first
        {
            let cache = self.user_name_cache.read().await;
            if let Some(name) = cache.get(open_id) {
                return Ok(Some(name.clone()));
            }
        }

        // Cache miss — call Feishu API
        let core_config = self.get_core_config().await?;

        use open_lark::communication::contact::contact::v3::user::get::GetUserRequest;
        use open_lark::communication::contact::contact::v3::user::models::UserIdType;

        let resp = GetUserRequest::new(core_config)
            .user_id(open_id)
            .user_id_type(UserIdType::OpenId)
            .execute()
            .await;

        match resp {
            Ok(data) => {
                // UserResponse has a typed `user: User` field with `name: Option<String>`
                let name = data.user.name;

                if let Some(ref name) = name {
                    let mut cache = self.user_name_cache.write().await;
                    cache.insert(open_id.to_string(), name.clone());
                    tracing::debug!(open_id = %open_id, name = %name, "User name cached");
                }

                Ok(name)
            }
            Err(e) => {
                tracing::warn!(
                    open_id = %open_id,
                    error = %e,
                    "Failed to get user name (check contact:user.base:readonly scope)"
                );
                Ok(None)
            }
        }
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
        let core_config = self.get_core_config().await?;
        let file_bytes = tokio::fs::read(path).await
            .with_context(|| format!("Failed to read file: {}", path.display()))?;

        use open_lark::communication::im::im::v1::file::create::{
            CreateFileBody, CreateFileRequest,
        };

        let body = CreateFileBody::new(file_type, filename);
        let resp = CreateFileRequest::new(core_config)
            .execute(body, file_bytes)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to upload file to Feishu: {e}"))?;

        tracing::info!(
            filename = %filename,
            file_type = %file_type,
            file_key = %resp.file_key,
            "File uploaded to Feishu"
        );

        Ok(resp.file_key)
    }

    /// Upload an image to Feishu servers.
    ///
    /// Returns the `image_key` for use in image messages.
    /// Requires scope: `im:resource`.
    pub async fn upload_image(
        &self,
        path: &Path,
        filename: &str,
    ) -> Result<String> {
        let core_config = self.get_core_config().await?;
        let image_bytes = tokio::fs::read(path).await
            .with_context(|| format!("Failed to read image: {}", path.display()))?;

        use open_lark::communication::im::im::v1::image::create::CreateImageRequest;
        use open_lark::communication::im::im::v1::image::models::ImageType;

        let resp = CreateImageRequest::new(core_config)
            .image_type(ImageType::Message)
            .file_name(filename)
            .execute(image_bytes)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to upload image to Feishu: {e}"))?;

        tracing::info!(
            filename = %filename,
            image_key = %resp.image_key,
            "Image uploaded to Feishu"
        );

        Ok(resp.image_key)
    }

    /// Send a file message to a chat (after uploading via `upload_file()`).
    pub async fn send_file_message(
        &self,
        chat_id: &str,
        file_key: &str,
    ) -> Result<FeishuMessageResult> {
        let core_config = self.get_core_config().await?;

        use open_lark::communication::im::im::v1::message::create::{
            CreateMessageBody, CreateMessageRequest,
        };
        use open_lark::communication::im::im::v1::message::models::ReceiveIdType;

        let body = CreateMessageBody {
            receive_id: chat_id.to_string(),
            msg_type: "file".to_string(),
            content: serde_json::json!({"file_key": file_key}).to_string(),
            uuid: None,
        };

        let resp = CreateMessageRequest::new(core_config)
            .receive_id_type(ReceiveIdType::ChatId)
            .execute(body)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send file message: {e}"))?;

        let message_id = resp
            .get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();

        Ok(FeishuMessageResult { message_id })
    }

    /// Send an image message to a chat (after uploading via `upload_image()`).
    pub async fn send_image_message(
        &self,
        chat_id: &str,
        image_key: &str,
    ) -> Result<FeishuMessageResult> {
        let core_config = self.get_core_config().await?;

        use open_lark::communication::im::im::v1::message::create::{
            CreateMessageBody, CreateMessageRequest,
        };
        use open_lark::communication::im::im::v1::message::models::ReceiveIdType;

        let body = CreateMessageBody {
            receive_id: chat_id.to_string(),
            msg_type: "image".to_string(),
            content: serde_json::json!({"image_key": image_key}).to_string(),
            uuid: None,
        };

        let resp = CreateMessageRequest::new(core_config)
            .receive_id_type(ReceiveIdType::ChatId)
            .execute(body)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send image message: {e}"))?;

        let message_id = resp
            .get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();

        tracing::debug!(
            "Sent image message: chat_id = {}, image_key = {}, message_id = {}",
            chat_id,
            image_key,
            message_id
        );

        Ok(FeishuMessageResult { message_id })
    }

    /// Download a file from Feishu servers.
    ///
    /// Returns the file content as bytes.
    /// Validates that the response is actual file data, not an API error.
    pub async fn download_file(&self, file_key: &str) -> Result<Vec<u8>> {
        let core_config = self.get_core_config().await?;

        use open_lark::communication::im::im::v1::file::get::GetFileRequest;

        let request = GetFileRequest::new(core_config)
            .file_key(file_key);

        let file_bytes = request.execute().await
            .map_err(|e| anyhow::anyhow!("Failed to download file from Feishu: {e}"))?;

        // Validate response is actual file data, not a JSON error
        if file_bytes.starts_with(b"{\"code\"") || file_bytes.starts_with(b"{\"error\"") {
            let body_str = String::from_utf8_lossy(&file_bytes);
            anyhow::bail!(
                "Feishu file download returned error instead of file data: {}",
                &body_str[..body_str.len().min(200)]
            );
        }

        if file_bytes.is_empty() {
            anyhow::bail!("Feishu file download returned empty data for file_key={}", file_key);
        }

        tracing::debug!(
            "Downloaded file from Feishu: file_key = {}, size = {} bytes",
            file_key,
            file_bytes.len()
        );

        Ok(file_bytes)
    }

    /// Download an image from Feishu servers.
    ///
    /// Returns the image content as bytes.
    /// Uses direct HTTP request with the required `type` query parameter
    /// that the openlark SDK doesn't support.
    pub async fn download_image(&self, image_key: &str) -> Result<Vec<u8>> {
        let token = self.get_token().await?;
        let base_url = &self.config.base_url;
        let url = format!(
            "{}/open-apis/im/v1/images/{}",
            base_url.trim_end_matches('/'),
            image_key
        );

        let http = reqwest::Client::new();
        let resp = http
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .query(&[("type", "image")])
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

        let image_bytes = resp.bytes().await
            .context("Failed to read image response body")?
            .to_vec();

        // Check for API error responses
        if !status.is_success() || content_type.contains("application/json") {
            let body_str = String::from_utf8_lossy(&image_bytes);
            anyhow::bail!(
                "Feishu image download failed (HTTP {}): {}",
                status,
                &body_str[..body_str.len().min(300)]
            );
        }

        if image_bytes.is_empty() {
            anyhow::bail!("Feishu image download returned empty data for image_key={}", image_key);
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
    use crate::channels::feishu::config::{FeishuConfig, WebSocketConfig};

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
    fn test_feishu_message_result() {
        let result = FeishuMessageResult {
            message_id: "test_message_123".to_string(),
        };

        assert_eq!(result.message_id, "test_message_123");

        let cloned = result.clone();
        assert_eq!(cloned.message_id, result.message_id);
    }
}
