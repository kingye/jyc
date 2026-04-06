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
        // Pre-warm: initialize the openlark client and acquire access token.
        // This moves the slow HTTP calls (DNS + TLS + token acquisition) to startup
        // rather than deferring them to the first send_reply(), which runs inside
        // the time-critical MCP reply tool subprocess with a limited timeout.
        self.client.initialize().await
            .context("Failed to initialize Feishu client during connect")?;

        // Pre-acquire access token — this makes the first send_reply() faster
        // because the token is already cached by the openlark SDK.
        match self.client.get_token().await {
            Ok(_) => tracing::info!("Feishu outbound adapter connected (client + token ready)"),
            Err(e) => {
                // Token pre-fetch failure is non-fatal — the SDK will retry on first API call
                tracing::warn!(error = %e, "Feishu token pre-fetch failed, will retry on first send");
            }
        }
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
        
        tracing::info!("Feishu reply sent to {}: {}", chat_id, &reply_text[..reply_text.len().min(50)]);

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

    async fn send_heartbeat(
        &self,
        original: &InboundMessage,
        elapsed_secs: u64,
        activity: &str,
        progress: &str,
    ) -> Result<SendResult> {
        // Ensure client is initialized
        self.client.initialize().await
            .context("Failed to initialize Feishu client before sending heartbeat")?;
        
        // Extract chat ID from original message
        let chat_id = original.channel_uid.as_str();
        
        // Format heartbeat message
        let minutes = elapsed_secs / 60;
        let seconds = elapsed_secs % 60;
        let heartbeat_text = format!(
            "Processing update ({}m {}s elapsed)\n\n**Activity:** {}\n**Progress:** {}",
            minutes, seconds, activity, progress
        );
        
        // Send heartbeat using Feishu client
        let result = self.client.send_text_message(chat_id, &heartbeat_text).await
            .context("Failed to send Feishu heartbeat")?;
        
        tracing::debug!("Feishu heartbeat sent to {}: {}", chat_id, activity);
        
        Ok(SendResult {
            message_id: result.message_id,
        })
    }
}
