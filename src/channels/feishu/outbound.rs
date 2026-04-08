//! Feishu outbound adapter implementation.
//! 
//! This module handles sending messages to Feishu via HTTP API.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::channels::feishu::client::FeishuClient;
use crate::channels::feishu::config::FeishuConfig;
use crate::channels::types::{InboundMessage, OutboundAttachment, SendResult};
use crate::core::email_parser;
use crate::core::message_storage::MessageStorage;

/// Feishu outbound adapter for sending messages via HTTP API.
pub struct FeishuOutboundAdapter {
    client: FeishuClient,
    storage: Arc<MessageStorage>,
}

impl FeishuOutboundAdapter {
    /// Create a new Feishu outbound adapter.
    pub fn new(config: FeishuConfig, storage: Arc<MessageStorage>) -> Self {
        Self {
            client: FeishuClient::new(config),
            storage,
        }
    }
}

#[async_trait]
impl crate::channels::types::OutboundAdapter for FeishuOutboundAdapter {
    fn channel_type(&self) -> &str {
        "feishu"
    }

    async fn connect(&self) -> Result<()> {
        // Pre-warm: initialize the openlark client.
        // The SDK handles token acquisition internally when sending messages,
        // so we don't need to pre-fetch the token here.
        self.client.initialize().await
            .context("Failed to initialize Feishu client during connect")?;
        tracing::info!("Feishu outbound adapter connected");
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        tracing::info!("Feishu outbound adapter disconnected");
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        // Feishu messages don't have quoted reply history like email.
        // Just trim whitespace for now.
        raw_body.trim().to_string()
    }

    async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        message_dir: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        // Ensure client is initialized
        self.client.initialize().await
            .context("Failed to initialize Feishu client before sending reply")?;
        
        // Extract chat ID from original message
        let chat_id = original.channel_uid.as_str();
        
        // 1. Read model/mode from reply context file (if available)
        let reply_ctx = crate::mcp::context::load_reply_context(thread_path).await.ok();
        let model = reply_ctx.as_ref().and_then(|c| c.model.as_deref());
        let mode = reply_ctx.as_ref().and_then(|c| c.mode.as_deref());
        
        // Read current input tokens from session state
        let (input_tokens, max_tokens) = crate::services::opencode::session::read_input_tokens(thread_path).await;
        
        // 2. Build footer with model/mode/tokens information
        let footer = email_parser::build_footer(model, mode, input_tokens, max_tokens);
        
        // 3. Combine reply text with footer
        let full_reply = if footer.is_empty() {
            reply_text.to_string()
        } else {
            format!("{}\n\n{}", reply_text, footer)
        };
        
        // 4. Send text reply
        let result = self.client.send_text_message(chat_id, &full_reply).await
            .context("Failed to send Feishu reply")?;
        
        tracing::info!(
            chat_id = %chat_id,
            text_len = full_reply.len(),
            "Feishu reply sent"
        );

        // 2. Send attachments as separate messages (upload → send)
        if let Some(attachments) = attachments {
            use crate::channels::feishu::client::{feishu_file_type, is_image_content_type};

            for att in attachments {
                if is_image_content_type(&att.content_type) {
                    // Image: upload → send image message
                    match self.client.upload_image(&att.path, &att.filename).await {
                        Ok(image_key) => {
                            match self.client.send_image_message(chat_id, &image_key).await {
                                Ok(_) => tracing::info!(
                                    filename = %att.filename,
                                    "Image attachment sent"
                                ),
                                Err(e) => tracing::warn!(
                                    filename = %att.filename,
                                    error = %e,
                                    "Failed to send image attachment message"
                                ),
                            }
                        }
                        Err(e) => tracing::warn!(
                            filename = %att.filename,
                            error = %e,
                            "Failed to upload image attachment"
                        ),
                    }
                } else {
                    // File: upload → send file message
                    let ext = att.path.extension()
                        .map(|e| e.to_string_lossy().to_lowercase())
                        .unwrap_or_default();
                    let file_type = feishu_file_type(&ext);

                    match self.client.upload_file(&att.path, &att.filename, file_type).await {
                        Ok(file_key) => {
                            match self.client.send_file_message(chat_id, &file_key).await {
                                Ok(_) => tracing::info!(
                                    filename = %att.filename,
                                    file_type = %file_type,
                                    "File attachment sent"
                                ),
                                Err(e) => tracing::warn!(
                                    filename = %att.filename,
                                    error = %e,
                                    "Failed to send file attachment message"
                                ),
                            }
                        }
                        Err(e) => tracing::warn!(
                            filename = %att.filename,
                            error = %e,
                            "Failed to upload file attachment"
                        ),
                    }
                }
            }
        }

        // 5. Store reply to chat log
        self.storage
            .store_reply(thread_path, &full_reply, message_dir)
            .await?;

        tracing::debug!(
            message_dir = %message_dir,
            "Reply stored"
        );
        
        Ok(SendResult {
            message_id: result.message_id,
        })
    }

    async fn send_alert(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
    ) -> Result<SendResult> {
        // Ensure client is initialized
        self.client.initialize().await
            .context("Failed to initialize Feishu client before sending alert")?;
        
        // Format alert message
        let alert_text = format!("**{}**\n\n{}", subject, body);
        
        // Send alert using Feishu client
        let result = self.client.send_text_message(recipient, &alert_text).await
            .context("Failed to send Feishu alert")?;
        
        tracing::info!("Feishu alert sent to {}: {}", recipient, subject);
        
        Ok(SendResult {
            message_id: result.message_id,
        })
    }

    /// Send a heartbeat/progress update to the user via Feishu.
    ///
    /// The `message` is pre-formatted from the per-channel heartbeat_template.
    async fn send_heartbeat(
        &self,
        original: &InboundMessage,
        message: &str,
    ) -> Result<SendResult> {
        // Ensure client is initialized
        self.client.initialize().await
            .context("Failed to initialize Feishu client before sending heartbeat")?;
        
        // Extract chat ID from original message
        let chat_id = original.channel_uid.as_str();
        
        // Send heartbeat using Feishu client
        let result = self.client.send_text_message(chat_id, message).await
            .context("Failed to send Feishu heartbeat")?;
        
        tracing::debug!("Feishu heartbeat sent to {}", chat_id);
        
        Ok(SendResult {
            message_id: result.message_id,
        })
    }
}
