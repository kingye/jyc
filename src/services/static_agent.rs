use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::agent::{AgentResult, AgentService};
use crate::channels::types::InboundMessage;
use crate::core::thread_event_bus::ThreadEventBusRef;
use crate::core::thread_manager::QueueItem;

/// Static agent — replies with a fixed text (no AI).
///
/// Channel-agnostic — just returns the configured text.
/// The outbound adapter handles formatting, sending, and storing.
/// `pending_rx` is accepted but not used (no AI session to inject into).
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
    async fn base_url(&self) -> Result<String> {
        anyhow::bail!("Static agent mode does not use OpenCode server")
    }

    async fn process(
        &self,
        _message: &InboundMessage,
        _thread_name: &str,
        _thread_path: &Path,
        _message_dir: &str,
        _pending_rx: &mut mpsc::Receiver<QueueItem>,
        _thread_cancel: CancellationToken,
    ) -> Result<AgentResult> {
        tracing::info!("Static reply generated");

        Ok(AgentResult {
            reply_sent_by_tool: false,
            reply_text: Some(self.reply_text.clone()),
        })
    }

    async fn set_thread_event_bus(&self, _thread_name: &str, _event_bus: Option<ThreadEventBusRef>) {
        // Static agent doesn't use event bus
    }
}
