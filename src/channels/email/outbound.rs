use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::channels::types::{InboundMessage, OutboundAttachment, SendResult};
use crate::config::types::SmtpConfig;
use crate::services::smtp::client::SmtpClient;

/// Email outbound adapter — sends replies, alerts, and progress updates via SMTP.
///
/// Wraps SmtpClient and implements the OutboundAdapter interface.
/// Designed to be shared (via Arc) across ThreadManager, AlertService, and MCP reply tool.
pub struct EmailOutboundAdapter {
    smtp: Arc<Mutex<SmtpClient>>,
    from_address: String,
    from_name: Option<String>,
}

impl EmailOutboundAdapter {
    pub fn new(config: &SmtpConfig) -> Self {
        let from_address = config
            .from_address
            .clone()
            .unwrap_or_else(|| config.username.clone());
        let from_name = config.from_name.clone();

        Self {
            smtp: Arc::new(Mutex::new(SmtpClient::new(config.clone()))),
            from_address,
            from_name,
        }
    }

    pub async fn connect(&self) -> Result<()> {
        let mut smtp = self.smtp.lock().await;
        smtp.connect().await
    }

    pub async fn disconnect(&self) -> Result<()> {
        let mut smtp = self.smtp.lock().await;
        smtp.disconnect().await;
        Ok(())
    }

    /// Send a reply to an inbound message.
    pub async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        _attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        let mut smtp = self.smtp.lock().await;

        // Build references: original's References + original's Message-ID
        let mut refs: Vec<String> = original
            .thread_refs
            .clone()
            .unwrap_or_default();
        if let Some(ref ext_id) = original.external_id {
            refs.push(ext_id.clone());
        }

        let message_id = smtp
            .send_reply(
                &self.from_address,
                self.from_name.as_deref(),
                &original.sender_address,
                &original.topic,
                reply_text,
                original.external_id.as_deref(),
                if refs.is_empty() {
                    None
                } else {
                    Some(&refs)
                },
            )
            .await?;

        Ok(SendResult { message_id })
    }

    /// Send a fresh alert email (not a reply, no threading headers).
    pub async fn send_alert(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
    ) -> Result<SendResult> {
        let mut smtp = self.smtp.lock().await;
        let message_id = smtp
            .send_mail(&self.from_address, recipient, subject, body)
            .await?;
        Ok(SendResult { message_id })
    }

    /// Send a progress update email (threaded with the original message).
    pub async fn send_progress_update(
        &self,
        original: &InboundMessage,
        elapsed_ms: u64,
        activity: &str,
    ) -> Result<SendResult> {
        let elapsed_secs = elapsed_ms / 1000;
        let minutes = elapsed_secs / 60;
        let seconds = elapsed_secs % 60;

        let subject = format!("[Processing Update] {}", original.topic);
        let body = format!(
            "Your message is still being processed.\n\n\
             **Time elapsed:** {}m {}s\n\
             **Current activity:** {}\n\n\
             You will receive the full reply when processing is complete.",
            minutes, seconds, activity
        );

        // Send as a threaded reply
        let mut smtp = self.smtp.lock().await;
        let mut refs: Vec<String> = original
            .thread_refs
            .clone()
            .unwrap_or_default();
        if let Some(ref ext_id) = original.external_id {
            refs.push(ext_id.clone());
        }

        let message_id = smtp
            .send_reply(
                &self.from_address,
                self.from_name.as_deref(),
                &original.sender_address,
                &subject,
                &body,
                original.external_id.as_deref(),
                if refs.is_empty() {
                    None
                } else {
                    Some(&refs)
                },
            )
            .await?;

        Ok(SendResult { message_id })
    }

    /// Get a clone of the inner SmtpClient (for MCP reply tool to create its own).
    pub fn smtp_client(&self) -> Arc<Mutex<SmtpClient>> {
        self.smtp.clone()
    }
}
