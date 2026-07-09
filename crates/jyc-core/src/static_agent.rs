use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentResult, AgentService};
use crate::thread_event_bus::ThreadEventBusRef;
use jyc_types::InboundMessage;
use jyc_types::QueueItem;

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
        anyhow::bail!("Static agent mode does not support base_url")
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

    async fn set_thread_event_bus(
        &self,
        _thread_name: &str,
        _event_bus: Option<ThreadEventBusRef>,
    ) {
        // Static agent doesn't use event bus
    }

    async fn reset_session(
        &self,
        thread_path: &Path,
        _thread_name: &str,
        config: &jyc_types::channel::ResetCompressionConfig,
    ) -> Result<()> {
        use jyc_types::channel::CompressionMode;

        let jyc_dir = thread_path.join(".jyc");
        match config.mode {
            CompressionMode::None => {
                tokio::fs::remove_file(jyc_dir.join("agent-context.json"))
                    .await
                    .ok();
                tokio::fs::remove_file(jyc_dir.join("agent-session.json"))
                    .await
                    .ok();
            }
            CompressionMode::Heuristic | CompressionMode::Llm => {
                // For static agent, heuristic and LLM are equivalent: just delete
                tokio::fs::remove_file(jyc_dir.join("agent-context.json"))
                    .await
                    .ok();
                tokio::fs::remove_file(jyc_dir.join("agent-session.json"))
                    .await
                    .ok();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::{InboundMessage, MessageContent};
    use std::path::Path;
    use tokio::sync::mpsc;

    fn dummy_message() -> InboundMessage {
        InboundMessage {
            id: "msg1".to_string(),
            channel: "test".to_string(),
            channel_uid: "user1".to_string(),
            sender: "user1".to_string(),
            sender_address: "user1".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: std::collections::HashMap::new(),
            matched_pattern: None,
        }
    }

    #[test]
    fn new_service() {
        let svc = StaticAgentService::new("fixed reply");
        assert_eq!(svc.reply_text, "fixed reply");
    }

    #[tokio::test]
    async fn base_url_returns_error() {
        let svc = StaticAgentService::new("reply");
        let result = svc.base_url().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Static agent"));
    }

    #[tokio::test]
    async fn process_returns_static_text() {
        let svc = StaticAgentService::new("hello world");
        let msg = dummy_message();
        let (_tx, mut rx) = mpsc::channel::<QueueItem>(10);
        let cancel = CancellationToken::new();
        let result = svc
            .process(
                &msg,
                "test_thread",
                Path::new("/tmp"),
                "msg1",
                &mut rx,
                cancel,
            )
            .await
            .unwrap();
        assert!(!result.reply_sent_by_tool);
        assert_eq!(result.reply_text, Some("hello world".to_string()));
    }

    #[tokio::test]
    async fn set_thread_event_bus_does_nothing() {
        let svc = StaticAgentService::new("reply");
        svc.set_thread_event_bus("test", None).await;
        // No panic = success
    }
}
