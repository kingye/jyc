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
        _attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        // Ensure client is initialized
        self.client.initialize().await
            .context("Failed to initialize Feishu client before sending reply")?;
        
        // Extract chat ID from original message
        let chat_id = original.channel_uid.as_str();
        
        // Send message using Feishu client
        let result = self.client.send_text_message(chat_id, reply_text).await
            .context("Failed to send Feishu reply")?;
        
        tracing::info!(
            chat_id = %chat_id,
            text_len = reply_text.len(),
            "Feishu reply sent"
        );

        // Store reply.md
        self.storage
            .store_reply(thread_path, reply_text, message_dir)
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
