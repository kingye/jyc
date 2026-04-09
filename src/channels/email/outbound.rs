use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::channels::types::{InboundMessage, OutboundAdapter, OutboundAttachment, SendResult};
use crate::config::types::{OutboundAttachmentConfig, SmtpConfig};
use crate::utils::attachment_validator;
use crate::core::email_parser;
use crate::core::message_storage::MessageStorage;
use crate::services::smtp::client::{EmailAttachment, SmtpClient};

/// Email outbound adapter — owns the full reply lifecycle: format + send + store.
///
/// Responsibilities:
/// - Build email-formatted reply with quoted history (channel-specific)
/// - Send via SMTP with threading headers and attachments
/// - Store reply to chat log
///
/// This is the channel-specific component. The agent (OpenCodeService) and
/// ThreadManager are channel-agnostic — they pass raw AI text to this adapter.
pub struct EmailOutboundAdapter {
    smtp: Arc<Mutex<SmtpClient>>,
    storage: Arc<MessageStorage>,
    from_address: String,
    from_name: Option<String>,
    attachment_config: Option<OutboundAttachmentConfig>,
}

impl EmailOutboundAdapter {
    #[allow(dead_code)]
    pub fn new(config: &SmtpConfig, storage: Arc<MessageStorage>) -> Self {
        Self::new_with_attachments(config, storage, None)
    }
    
    pub fn new_with_attachments(
        config: &SmtpConfig,
        storage: Arc<MessageStorage>,
        attachment_config: Option<OutboundAttachmentConfig>,
    ) -> Self {
        let from_address = config
            .from_address
            .clone()
            .unwrap_or_else(|| config.username.clone());
        let from_name = config.from_name.clone();

        Self {
            smtp: Arc::new(Mutex::new(SmtpClient::new(config.clone()))),
            storage,
            from_address,
            from_name,
            attachment_config,
        }
    }

    /// Internal: send via SMTP with threading headers and attachments.
    async fn smtp_send(
        &self,
        original: &InboundMessage,
        full_reply: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        let mut smtp = self.smtp.lock().await;

        let mut refs: Vec<String> = original
            .thread_refs
            .clone()
            .unwrap_or_default();
        if let Some(ref ext_id) = original.external_id {
            refs.push(ext_id.clone());
        }

        let email_attachments = if let Some(atts) = attachments {
            let mut loaded = Vec::new();
            for att in atts {
                let data = tokio::fs::read(&att.path).await?;
                loaded.push(EmailAttachment {
                    filename: att.filename.clone(),
                    content_type: att.content_type.clone(),
                    data,
                });
            }
            Some(loaded)
        } else {
            None
        };

        let message_id = smtp
            .send_reply(
                &self.from_address,
                self.from_name.as_deref(),
                &original.sender_address,
                &original.topic,
                full_reply,
                original.external_id.as_deref(),
                if refs.is_empty() { None } else { Some(&refs) },
                email_attachments.as_deref(),
            )
            .await?;

        Ok(SendResult { message_id })
    }
}

#[async_trait]
impl OutboundAdapter for EmailOutboundAdapter {
    fn channel_type(&self) -> &str {
        "email"
    }

    async fn connect(&self) -> Result<()> {
        let mut smtp = self.smtp.lock().await;
        smtp.connect().await
    }

    async fn disconnect(&self) -> Result<()> {
        let mut smtp = self.smtp.lock().await;
        smtp.disconnect().await;
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        email_parser::strip_quoted_history(raw_body)
    }

    /// Send a reply to an inbound message.
    ///
    /// Owns the full reply lifecycle:
    /// 1. Build email-formatted reply with quoted history
    /// 2. Send via SMTP with threading headers + attachments
    /// 3. Store reply to chat log
    async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        message_dir: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        // 1. Read model/mode from reply context file (if available)
        let reply_ctx = crate::mcp::context::load_reply_context(thread_path).await.ok();
        let model = reply_ctx.as_ref().and_then(|c| c.model.as_deref());
        let mode = reply_ctx.as_ref().and_then(|c| c.mode.as_deref());

        // Read current input tokens from session state
        let (input_tokens, max_tokens) = crate::services::opencode::session::read_input_tokens(thread_path).await;

        // 2. Build full reply with email-specific quoted history + model/mode footer
        let body_text = original
            .content
            .text
            .as_deref()
            .or(original.content.markdown.as_deref())
            .unwrap_or("");

        let full_reply = email_parser::build_full_reply_text(
            reply_text,
            thread_path,
            &original.sender,
            &original.timestamp.to_rfc3339(),
            &original.topic,
            body_text,
            message_dir,
            model,
            mode,
            input_tokens,
            max_tokens,
        )
        .await;

        // 3. Validate attachments if configuration is present
        if let Some(attachments) = attachments {
            if let Some(ref config) = self.attachment_config {
                attachment_validator::validate_outbound_attachments(attachments, config)
                    .await
                    .context("Failed to validate outbound attachments")?;
                tracing::debug!("Outbound attachments validated successfully");
            }
        }

        // 4. Send via SMTP
        let send_result = self
            .smtp_send(original, &full_reply, attachments)
            .await?;

        // 4. Store reply to chat log
        self.storage
            .store_reply(thread_path, &full_reply, message_dir)
            .await?;

        tracing::debug!(
            message_dir = %message_dir,
            "Reply stored"
        );

        Ok(send_result)
    }

    /// Send a fresh alert email (not a reply, no formatting/threading/storage).
    async fn send_alert(
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

    /// Send a heartbeat/progress update to the user via email.
    ///
    /// The `message` is pre-formatted from the per-channel heartbeat_template.
    /// Sent as a threaded reply to the original email.
    async fn send_heartbeat(
        &self,
        original: &InboundMessage,
        message: &str,
    ) -> Result<SendResult> {
        let subject = format!("[Processing Update] {}", original.topic);

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
                message,
                original.external_id.as_deref(),
                if refs.is_empty() { None } else { Some(&refs) },
                None,
            )
            .await?;

        Ok(SendResult { message_id })
    }
}
