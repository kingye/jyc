use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

use super::agent::{AgentResult, AgentService};
use crate::channels::email::outbound::EmailOutboundAdapter;
use crate::channels::types::InboundMessage;
use crate::core::email_parser;
use crate::core::message_storage::MessageStorage;

/// Static agent — replies with a fixed text (no AI).
pub struct StaticAgentService {
    reply_text: String,
    storage: Arc<MessageStorage>,
    outbound: Arc<EmailOutboundAdapter>,
}

impl StaticAgentService {
    pub fn new(
        reply_text: &str,
        storage: Arc<MessageStorage>,
        outbound: Arc<EmailOutboundAdapter>,
    ) -> Self {
        Self {
            reply_text: reply_text.to_string(),
            storage,
            outbound,
        }
    }
}

#[async_trait]
impl AgentService for StaticAgentService {
    async fn process(
        &self,
        message: &InboundMessage,
        thread_name: &str,
        thread_path: &Path,
        message_dir: &str,
    ) -> Result<AgentResult> {
        let body_text = message
            .content
            .text
            .as_deref()
            .or(message.content.markdown.as_deref())
            .unwrap_or("");

        let full_reply = email_parser::build_full_reply_text(
            &self.reply_text,
            thread_path,
            &message.sender,
            &message.timestamp.to_rfc3339(),
            &message.topic,
            body_text,
            message_dir,
        )
        .await;

        self.outbound.send_reply(message, &full_reply, None).await?;
        self.storage
            .store_reply(thread_path, &full_reply, message_dir)
            .await?;

        tracing::info!(thread = %thread_name, "Static reply sent");

        Ok(AgentResult {
            reply_sent: true,
            summary: "Static reply sent".to_string(),
        })
    }
}
