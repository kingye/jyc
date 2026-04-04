use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::channels::types::{InboundMessage, OutboundAttachment, SendResult};
use crate::config::types::SmtpConfig;
use crate::core::email_parser;
use crate::core::message_storage::MessageStorage;
use crate::services::smtp::client::{EmailAttachment, SmtpClient};

/// Email outbound adapter — owns the full reply lifecycle: format + send + store.
///
/// Responsibilities:
/// - Build email-formatted reply with quoted history (channel-specific)
/// - Send via SMTP with threading headers and attachments
/// - Store reply.md
///
/// This is the channel-specific component. The agent (OpenCodeService) and
/// ThreadManager are channel-agnostic — they pass raw AI text to this adapter.
pub struct EmailOutboundAdapter {
    smtp: Arc<Mutex<SmtpClient>>,
    storage: Arc<MessageStorage>,
    from_address: String,
    from_name: Option<String>,
}

impl EmailOutboundAdapter {
    pub fn new(config: &SmtpConfig, storage: Arc<MessageStorage>) -> Self {
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
    ///
    /// Owns the full reply lifecycle:
    /// 1. Build email-formatted reply with quoted history
    /// 2. Send via SMTP with threading headers + attachments
    /// 3. Store reply.md
    pub async fn send_reply(
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
        )
        .await;

        // 2. Send via SMTP
        let send_result = self
            .smtp_send(original, &full_reply, attachments)
            .await?;

        // 3. Store reply.md
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
                if refs.is_empty() { None } else { Some(&refs) },
                None,
            )
            .await?;

        Ok(SendResult { message_id })
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

    /// Send a heartbeat/progress update with more detailed information.
    /// This is similar to send_progress_update but includes additional context.
    pub async fn send_heartbeat(
        &self,
        original: &InboundMessage,
        elapsed_secs: u64,
        activity: &str,
        progress: &str,
    ) -> Result<SendResult> {
        let _elapsed_ms = elapsed_secs * 1000;
        let elapsed_secs_total = elapsed_secs;
        let minutes = elapsed_secs_total / 60;
        let seconds = elapsed_secs_total % 60;

        let subject = format!("[Processing Update] {}", original.topic);
        let body = format!(
            "Your message is still being processed.\n\n\
             **Time elapsed:** {}m {}s\n\
             **Current activity:** {}\n\
             **Progress:** {}\n\n\
             You will receive the full reply when processing is complete.",
            minutes, seconds, activity, progress
        );

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
                if refs.is_empty() { None } else { Some(&refs) },
                None,
            )
            .await?;

        Ok(SendResult { message_id })
    }
}
