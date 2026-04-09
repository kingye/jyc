//! WebSocket connection handler for Feishu real-time events.
//!
//! Uses the openlark SDK's `LarkWsClient` to establish a persistent WebSocket
//! connection to Feishu's event subscription endpoint. Incoming events are
//! parsed and converted to `InboundMessage` for the channel-agnostic pipeline.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use open_lark::ws_client::{EventDispatcherHandler, LarkWsClient};

use crate::channels::types::{InboundMessage, MessageAttachment, MessageContent};
use crate::config::types::InboundAttachmentConfig;
use crate::utils::helpers;

use super::client::FeishuClient;
use super::config::FeishuConfig;
use super::types::{
    EventEnvelope, FileContent, ImageContent, TextContent,
};

/// WebSocket connection handler for Feishu.
///
/// Manages the lifecycle of a single WebSocket connection:
/// connect → receive events → parse → convert to InboundMessage → callback.
///
/// Reconnection logic is handled by the caller (`FeishuInboundAdapter::start()`).
pub struct FeishuWebSocket {
    config: FeishuConfig,
    client: Arc<FeishuClient>,
    reconnect_count: usize,
    #[allow(dead_code)]
    attachment_config: Option<InboundAttachmentConfig>,
}

impl FeishuWebSocket {
    /// Create a new WebSocket handler.
    pub fn new(config: &FeishuConfig, client: Arc<FeishuClient>) -> Self {
        Self {
            config: config.clone(),
            client,
            reconnect_count: 0,
            attachment_config: None,
        }
    }
    
    /// Create a new WebSocket handler with attachment configuration.
    pub fn new_with_attachments(
        config: &FeishuConfig,
        client: Arc<FeishuClient>,
        attachment_config: Option<InboundAttachmentConfig>,
    ) -> Self {
        Self {
            config: config.clone(),
            client,
            reconnect_count: 0,
            attachment_config,
        }
    }

    /// Connect to Feishu WebSocket and run the event loop.
    ///
    /// Blocks until the cancel token fires or the connection drops.
    /// For each received message event, converts to `InboundMessage`
    /// and calls `on_message`.
    ///
    /// Returns `Ok(())` on clean cancellation, `Err(...)` on connection failure.
    pub async fn run(
        &mut self,
        channel_name: &str,
        on_message: &(dyn Fn(InboundMessage) -> Result<()> + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<()> {
        // 1. Build openlark client Config from FeishuConfig
        let ws_config = open_lark::Config::builder()
            .app_id(&self.config.app_id)
            .app_secret(&self.config.app_secret)
            .base_url(&self.config.base_url)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build Feishu WebSocket config: {e}"))?;

        // 2. Create event payload channel
        let (payload_tx, mut payload_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // 3. Build event dispatcher handler
        let event_handler = EventDispatcherHandler::builder()
            .payload_sender(payload_tx)
            .build();

        // 4. Spawn LarkWsClient::open() — blocks until connection drops
        let ws_config = Arc::new(ws_config);
        tracing::info!("Connecting to Feishu WebSocket...");
        let ws_handle = tokio::spawn(async move {
            LarkWsClient::open(ws_config, event_handler).await
        });

        // Connection succeeded if we get here (open() spawns internal loops)
        self.reset_reconnection_count();
        tracing::info!("Feishu WebSocket connected, listening for events");

        // 5. Event loop: consume payloads until cancel or channel closes
        loop {
            tokio::select! {
                payload = payload_rx.recv() => {
                    match payload {
                        Some(data) => {
                            if let Err(e) = self.handle_payload(channel_name, &data, on_message).await {
                                tracing::warn!(error = %e, "Failed to process Feishu event");
                            }
                        }
                        None => {
                            // Channel closed — WebSocket connection dropped
                            tracing::warn!("Feishu WebSocket payload channel closed");
                            break;
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    tracing::info!("Feishu WebSocket cancelled");
                    ws_handle.abort();
                    return Ok(());
                }
            }
        }

        // If we get here, the connection dropped (not cancelled)
        match ws_handle.await {
            Ok(Ok(())) => {
                Err(anyhow::anyhow!("Feishu WebSocket connection closed unexpectedly"))
            }
            Ok(Err(e)) => {
                Err(anyhow::anyhow!("Feishu WebSocket error: {e}"))
            }
            Err(e) if e.is_cancelled() => {
                // Task was aborted (normal on cancel)
                Ok(())
            }
            Err(e) => {
                Err(anyhow::anyhow!("Feishu WebSocket task panicked: {e}"))
            }
        }
    }

    /// Parse a raw WebSocket payload and route it as an `InboundMessage`.
    async fn handle_payload(
        &self,
        channel_name: &str,
        data: &[u8],
        on_message: &(dyn Fn(InboundMessage) -> Result<()> + Send + Sync),
    ) -> Result<()> {
        let envelope: EventEnvelope = serde_json::from_slice(data)
            .context("Failed to parse Feishu event payload as JSON")?;

        // Only handle message received events
        if envelope.header.event_type != "im.message.receive_v1" {
            tracing::debug!(
                event_type = %envelope.header.event_type,
                "Skipping non-message event"
            );
            return Ok(());
        }

        let message = self.convert_to_inbound(channel_name, &envelope).await
            .context("Failed to convert Feishu event to InboundMessage")?;

        tracing::info!(
            message_id = %message.channel_uid,
            sender = %message.sender_address,
            chat_type = message.metadata.get("chat_type").and_then(|v| v.as_str()).unwrap_or("?"),
            msg_type = envelope.event.message.message_type.as_str(),
            "Feishu message received"
        );

        on_message(message)?;
        Ok(())
    }

    /// Convert a Feishu event envelope to a channel-agnostic `InboundMessage`.
    async fn convert_to_inbound(
        &self,
        channel_name: &str,
        envelope: &EventEnvelope,
    ) -> Result<InboundMessage> {
        let event = &envelope.event;
        let msg = &event.message;
        let sender = &event.sender;

        // Extract text content and attachments based on message_type
        let (text, attachments) = match msg.message_type.as_str() {
            "text" => {
                let content: TextContent = serde_json::from_str(&msg.content)
                    .context("Failed to parse text message content")?;
                // Strip mention placeholders like @_user_1 from text
                let cleaned = strip_mention_placeholders(&content.text, msg.mentions.as_deref());
                (cleaned, vec![])
            }
            "image" => {
                let content: ImageContent = serde_json::from_str(&msg.content)
                    .context("Failed to parse image message content")?;
                
                tracing::debug!("Processing image message: image_key = {}", content.image_key);
                
                let mut attachments = vec![];
                
                // Check if attachment download is enabled and image type is allowed
                if let Some(ref config) = self.attachment_config {
                    // Check if image extensions are allowed (jpg, jpeg, png, gif)
                    let image_allowed = config.allowed_extensions.is_empty() || 
                        config.allowed_extensions.iter().any(|ext| 
                            ext == ".jpg" || ext == ".jpeg" || ext == ".png" || ext == ".gif" || 
                            ext == ".bmp" || ext == ".webp" || ext == ".svg"
                        );
                    
                    if config.enabled && image_allowed {
                        // Download image from Feishu
                        let image_bytes = match self.client.download_image(&content.image_key).await {
                            Ok(bytes) => {
                                tracing::debug!("Image downloaded: size = {} bytes", bytes.len());
                                bytes
                            }
                            Err(e) => {
                                tracing::warn!("Failed to download image from Feishu: {}", e);
                                Vec::new() // Return empty bytes on failure
                            }
                        };
                        
                        // Parse max file size from human-readable string (e.g., "25mb")
                        let max_size_bytes = config.max_file_size.as_ref()
                            .and_then(|s| parse_human_size(s).ok())
                            .unwrap_or(0);
                        
                        // Check size limit if configured
                        if max_size_bytes == 0 || image_bytes.len() <= max_size_bytes as usize {
                            // Create image attachment with downloaded content
                            let image_attachment = MessageAttachment {
                                filename: format!("image_{}.jpg", content.image_key),
                                content_type: "image/jpeg".to_string(),
                                size: image_bytes.len(),
                                content: Some(image_bytes),
                                saved_path: None,
                            };
                            attachments.push(image_attachment);
                        } else {
                            tracing::warn!(
                                "Image size {} exceeds limit {} bytes, skipping download",
                                image_bytes.len(),
                                max_size_bytes
                            );
                        }
                    } else {
                        tracing::debug!("Image download disabled or image type not allowed");
                    }
                } else {
                    tracing::debug!("Attachment config not provided, skipping image download");
                }
                
                (format!("[Image: {}]", content.image_key), attachments)
            }
            "file" => {
                let content: FileContent = serde_json::from_str(&msg.content)
                    .context("Failed to parse file message content")?;
                let name = content.file_name.as_deref().unwrap_or("unnamed");
                
                let mut attachments = vec![];
                
                // Check if attachment download is enabled
                if let Some(ref config) = self.attachment_config {
                    if config.enabled {
                        // Determine file extension for content type guessing
                        let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
                        let ext_with_dot = if ext.is_empty() { String::new() } else { format!(".{}", ext) };
                        
                        // Check if this file extension is allowed
                        let allowed = if config.allowed_extensions.is_empty() {
                            // If no extensions specified, all are allowed
                            true
                        } else if ext_with_dot.is_empty() {
                            // No extension - check if empty string is in allowed extensions
                            config.allowed_extensions.iter().any(|e| e.is_empty())
                        } else {
                            // Check if extension is in allowed list
                            config.allowed_extensions.iter().any(|allowed_ext| 
                                allowed_ext == &ext_with_dot || 
                                allowed_ext == &ext // Allow both with and without dot
                            )
                        };
                        
                        if allowed {
                            // Download file from Feishu
                            match self.client.download_file(&content.file_key).await {
                                Ok(file_bytes) => {
                                    tracing::debug!("File downloaded: file_key = {}, size = {} bytes", 
                                                   content.file_key, file_bytes.len());
                                    
                                // Parse max file size from human-readable string (e.g., "25mb")
                                let max_size_bytes = config.max_file_size.as_ref()
                                    .and_then(|s| parse_human_size(s).ok())
                                    .unwrap_or(0);
                                
                                // Check size limit if configured
                                if max_size_bytes == 0 || file_bytes.len() <= max_size_bytes as usize {
                                        // Determine content type based on extension
                                        let content_type = match ext.as_str() {
                                            "pdf" => "application/pdf",
                                            "doc" => "application/msword",
                                            "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                                            "xls" => "application/vnd.ms-excel",
                                            "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                                            "ppt" => "application/vnd.ms-powerpoint",
                                            "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                                            "txt" | "md" => "text/plain",
                                            "zip" => "application/zip",
                                            "rar" => "application/x-rar-compressed",
                                            "7z" => "application/x-7z-compressed",
                                            _ => "application/octet-stream",
                                        };
                                        
                                        // Create file attachment with downloaded content
                                        let file_attachment = MessageAttachment {
                                            filename: name.to_string(),
                                            content_type: content_type.to_string(),
                                            size: file_bytes.len(),
                                            content: Some(file_bytes),
                                            saved_path: None,
                                        };
                                        attachments.push(file_attachment);
                                    } else {
                                        tracing::warn!(
                                            "File size {} exceeds limit {} bytes, skipping download",
                                            file_bytes.len(),
                                            max_size_bytes
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to download file from Feishu: {}", e);
                                }
                            }
                        } else {
                            tracing::debug!("File type not allowed by attachment config");
                        }
                    } else {
                        tracing::debug!("Attachment download disabled");
                    }
                } else {
                    tracing::debug!("Attachment config not provided, skipping file download");
                }
                
                (format!("[File: {} (key: {})]", name, content.file_key), attachments)
            }
            "interactive" => {
                // Card messages: store raw content JSON for now
                (format!("[Card message]: {}", msg.content), vec![])
            }
            other => {
                (format!("[Unsupported message type: {}]: {}", other, msg.content), vec![])
            }
        };

        // Resolve chat_id — the reply target
        // For group messages: use chat_id from the message
        // For p2p (direct messages): use sender's open_id
        let chat_id = match msg.chat_type.as_str() {
            "p2p" => sender.sender_id.open_id.clone().unwrap_or_default(),
            _ => msg.chat_id.clone().unwrap_or_default(),
        };

        // Build mentions metadata as array of {id, name} objects
        let mentions_meta = msg.mentions.as_ref().map(|mentions| {
            let arr: Vec<serde_json::Value> = mentions
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id.open_id.as_deref().unwrap_or(""),
                        "name": m.name,
                    })
                })
                .collect();
            serde_json::Value::Array(arr)
        });

        let mut metadata = HashMap::new();
        metadata.insert(
            "chat_id".to_string(),
            serde_json::Value::String(chat_id.clone()),
        );
        metadata.insert(
            "chat_type".to_string(),
            serde_json::Value::String(msg.chat_type.clone()),
        );
        if let Some(open_id) = &sender.sender_id.open_id {
            metadata.insert(
                "user_id".to_string(),
                serde_json::Value::String(open_id.clone()),
            );
        }
        if let Some(mentions) = mentions_meta {
            metadata.insert("mentions".to_string(), mentions);
        }
        if let Some(ref event_id) = envelope.header.event_id {
            metadata.insert(
                "event_id".to_string(),
                serde_json::Value::String(event_id.clone()),
            );
        }

        let sender_id = sender
            .sender_id
            .open_id
            .as_deref()
            .unwrap_or("unknown");

        // Fetch readable names (cached after first call)
        let sender_name = if sender_id != "unknown" {
            self.client.get_user_name(sender_id).await.ok().flatten()
        } else {
            None
        };

        // For group chats, also look up the chat name
        let chat_name = if msg.chat_type == "group" {
            if !chat_id.is_empty() {
                self.client.get_chat_name(&chat_id).await.ok().flatten()
            } else {
                tracing::warn!("Group message has empty chat_id, cannot look up chat name");
                None
            }
        } else {
            None
        };

        tracing::debug!(
            sender_id = %sender_id,
            sender_name = ?sender_name,
            chat_id = %chat_id,
            chat_name = ?chat_name,
            chat_type = %msg.chat_type,
            "Name resolution completed"
        );

        // Store names in metadata for derive_thread_name() and prompt_builder
        if let Some(ref name) = sender_name {
            metadata.insert(
                "sender_name".to_string(),
                serde_json::Value::String(name.clone()),
            );
        }
        if let Some(ref name) = chat_name {
            metadata.insert(
                "chat_name".to_string(),
                serde_json::Value::String(name.clone()),
            );
        }

        let display_sender = sender_name.as_deref().unwrap_or(sender_id);

        let timestamp = msg
            .create_time
            .as_deref()
            .and_then(|t| t.parse::<i64>().ok())
            .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms))
            .unwrap_or_else(chrono::Utc::now);

        // Attachments will be saved later in thread directory
        // We keep the content in memory for now
        for attachment in &attachments {
            if attachment.content.is_some() {
                tracing::debug!(
                    "Attachment downloaded: {} ({} bytes), will be saved to thread directory later",
                    attachment.filename,
                    attachment.size
                );
            }
        }
        let saved_attachments = attachments;

        Ok(InboundMessage {
            id: uuid::Uuid::new_v4().to_string(),
            channel: channel_name.to_string(),
            channel_uid: chat_id,
            sender: display_sender.to_string(),
            sender_address: sender_id.to_string(),
            recipients: vec![],
            topic: chat_name.unwrap_or_default(), // Use chat name as topic for better AI context
            content: MessageContent {
                text: Some(text),
                html: None,
                markdown: None,
            },
            timestamp,
            thread_refs: None,
            reply_to_id: None,
            external_id: Some(msg.message_id.clone()),
            attachments: saved_attachments,
            metadata,
            matched_pattern: None,
        })
    }

    /// Handle reconnection with backoff. Returns `true` if we should retry.
    pub async fn handle_reconnection(&mut self) -> bool {
        if self.reconnect_count >= self.config.websocket.max_reconnect_attempts {
            tracing::error!(
                max_attempts = self.config.websocket.max_reconnect_attempts,
                "Maximum reconnection attempts reached"
            );
            return false;
        }

        let delay_secs = self.config.websocket.reconnect_delay_secs;
        self.reconnect_count += 1;
        tracing::info!(
            attempt = self.reconnect_count,
            max_attempts = self.config.websocket.max_reconnect_attempts,
            delay_secs = delay_secs,
            "Reconnecting to Feishu WebSocket"
        );

        tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
        true
    }

    /// Reset reconnection count (called after successful connection).
    pub fn reset_reconnection_count(&mut self) {
        self.reconnect_count = 0;
    }
}

/// Strip mention placeholder keys (e.g., "@_user_1") from message text.
///
/// In Feishu, when someone types "@jyc hello", the text content contains
/// `"@_user_1 hello"` with a separate mentions array mapping `@_user_1`
/// to the actual user. We replace placeholder keys with the display name.
fn strip_mention_placeholders(text: &str, mentions: Option<&[super::types::EventMention]>) -> String {
    let Some(mentions) = mentions else {
        return text.to_string();
    };

    let mut result = text.to_string();
    for mention in mentions {
        // Replace "@_user_1" with "@displayname"
        result = result.replace(&mention.key, &format!("@{}", mention.name));
    }
    result.trim().to_string()
}

/// Save an attachment to disk.
///
/// Creates a directory structure like:
/// thread_dir/attachments/channel_name/timestamp_uuid_filename
/// Returns the saved file path.
async fn save_attachment_to_disk(
    attachment: &MessageAttachment,
    channel_name: &str,
    thread_dir: &str,
    config_save_path: Option<&str>,
) -> Result<String> {
    // Determine the actual save path
    let save_path = match config_save_path {
        Some(path) => path.to_string(),
        None => {
            // Default path: <thread_dir>/attachments/
            let mut path = std::path::Path::new(thread_dir).to_path_buf();
            path.push("attachments");
            path.to_string_lossy().to_string()
        }
    };
    
    // Generate a unique filename with timestamp and UUID
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let uuid_short = uuid::Uuid::new_v4().to_string()[..8].to_string();
    
    // Sanitize filename to remove any problematic characters
    let safe_filename = helpers::sanitize_for_filesystem(&attachment.filename);
    let unique_filename = format!("{}_{}_{}", timestamp, uuid_short, safe_filename);
    
    // Create directory path: save_path/channel_name/
    let dir_path = std::path::Path::new(&save_path)
        .join(channel_name);
    
    // Ensure directory exists
    tokio::fs::create_dir_all(&dir_path).await
        .context("Failed to create attachment directory")?;
    
    // Full file path
    let file_path = dir_path.join(&unique_filename);
    
    // Write file content
    if let Some(ref content) = attachment.content {
        tokio::fs::write(&file_path, content).await
            .context("Failed to write attachment file")?;
    } else {
        return Err(anyhow::anyhow!("Attachment has no content to save"));
    }
    
    tracing::info!(
        "Attachment saved: {} ({} bytes) -> {}",
        attachment.filename,
        attachment.size,
        file_path.display()
    );
    
    // Return the absolute file path
    Ok(file_path.to_string_lossy().to_string())
}

/// Parse human-readable size strings like "25mb", "150kb", "2gb" into bytes.
fn parse_human_size(size_str: &str) -> Result<u64> {
    let size_str = size_str.trim().to_lowercase();
    
    // Find the numeric part and unit
    let numeric_end = size_str
        .chars()
        .position(|c| !c.is_ascii_digit() && c != '.')
        .unwrap_or(size_str.len());
    
    let numeric_part = &size_str[..numeric_end];
    let unit = &size_str[numeric_end..];
    
    let number: f64 = numeric_part.parse()
        .context("Invalid numeric part in size string")?;
    
    let multiplier = match unit {
        "" | "b" => 1.0,
        "kb" | "k" => 1024.0,
        "mb" | "m" => 1024.0 * 1024.0,
        "gb" | "g" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "t" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return Err(anyhow::anyhow!("Unknown size unit: {}", unit)),
    };
    
    Ok((number * multiplier) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_human_size() {
        assert_eq!(parse_human_size("100").unwrap(), 100);
        assert_eq!(parse_human_size("1kb").unwrap(), 1024);
        assert_eq!(parse_human_size("1k").unwrap(), 1024);
        assert_eq!(parse_human_size("1mb").unwrap(), 1024 * 1024);
        assert_eq!(parse_human_size("1m").unwrap(), 1024 * 1024);
        assert_eq!(parse_human_size("1gb").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_human_size("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_human_size("2.5mb").unwrap(), (2.5 * 1024.0 * 1024.0) as u64);
        assert_eq!(parse_human_size("  150kb  ").unwrap(), 150 * 1024);
        
        // Test invalid inputs
        assert!(parse_human_size("abc").is_err());
        assert!(parse_human_size("10xb").is_err());
    }

    #[test]
    fn test_websocket_creation() {
        let config = FeishuConfig::default();
        let client = Arc::new(super::super::client::FeishuClient::new(config.clone()));
        let ws = FeishuWebSocket::new(&config, client);
        assert_eq!(ws.reconnect_count, 0);
    }

    #[test]
    fn test_strip_mention_placeholders_none() {
        let result = strip_mention_placeholders("hello world", None);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_strip_mention_placeholders_empty() {
        let result = strip_mention_placeholders("hello world", Some(&[]));
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_strip_mention_placeholders_single() {
        use super::super::types::{EventMention, MentionIds};
        let mentions = vec![EventMention {
            key: "@_user_1".to_string(),
            id: MentionIds {
                open_id: Some("ou_xxx".to_string()),
                user_id: None,
                union_id: None,
            },
            name: "jyc".to_string(),
        }];
        let result = strip_mention_placeholders("@_user_1 hello", Some(&mentions));
        assert_eq!(result, "@jyc hello");
    }

    #[test]
    fn test_strip_mention_placeholders_multiple() {
        use super::super::types::{EventMention, MentionIds};
        let mentions = vec![
            EventMention {
                key: "@_user_1".to_string(),
                id: MentionIds {
                    open_id: Some("ou_aaa".to_string()),
                    user_id: None,
                    union_id: None,
                },
                name: "bot".to_string(),
            },
            EventMention {
                key: "@_user_2".to_string(),
                id: MentionIds {
                    open_id: Some("ou_bbb".to_string()),
                    user_id: None,
                    union_id: None,
                },
                name: "admin".to_string(),
            },
        ];
        let result = strip_mention_placeholders("@_user_1 @_user_2 help", Some(&mentions));
        assert_eq!(result, "@bot @admin help");
    }

    #[test]
    fn test_reconnection_count() {
        let mut config = FeishuConfig::default();
        config.websocket.max_reconnect_attempts = 3;
        let client = Arc::new(super::super::client::FeishuClient::new(config.clone()));
        let mut ws = FeishuWebSocket::new(&config, client);
        assert_eq!(ws.reconnect_count, 0);

        ws.reconnect_count = 2;
        ws.reset_reconnection_count();
        assert_eq!(ws.reconnect_count, 0);
    }
}
