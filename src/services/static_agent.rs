use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;

use super::agent::{AgentResult, AgentService};
use crate::channels::types::InboundMessage;

/// Static agent — replies with a fixed text (no AI).
///
/// Channel-agnostic — just returns the configured text.
/// The outbound adapter handles formatting, sending, and storing.
pub struct StaticAgentService {
    reply_text: String,
}

impl StaticAgentService {
    pub fn new(reply_text: &str) -> Self {
        Self {
            reply_text: reply_text.to_string(),
        }
    }
}

#[async_trait]
impl AgentService for StaticAgentService {
    async fn process(
        &self,
        _message: &InboundMessage,
        thread_name: &str,
        _thread_path: &Path,
        _message_dir: &str,
    ) -> Result<AgentResult> {
        tracing::info!("Static reply generated");

        Ok(AgentResult {
            reply_sent_by_tool: false,
            reply_text: Some(self.reply_text.clone()),
        })
    }
}
