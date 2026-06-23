//! Local TUI channel outbound adapter.
//!
//! Sends AI replies from the async system back to the TUI for display.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;

use jyc_types::{
    InboundMessage, OutboundAdapter, OutboundAttachment, SendResult,
};

/// Local TUI outbound adapter.
///
/// Holds an optional mpsc sender that is injected after construction
/// (same pattern as WeCom Bot's `handle_arc`). Replies are sent to
/// the TUI via this channel.
pub struct LocalOutboundAdapter {
    /// Shared output sender — injected after construction.
    output_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>,
}

impl LocalOutboundAdapter {
    /// Create a new local outbound adapter.
    pub fn new() -> Self {
        Self {
            output_tx: Arc::new(Mutex::new(None)),
        }
    }

    /// Get the shared output sender Arc so the inbound adapter can set it.
    pub fn output_tx_arc(
        &self,
    ) -> Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>> {
        self.output_tx.clone()
    }

    /// Set the output sender.
    #[allow(dead_code)]
    pub async fn set_output_tx(&self, tx: tokio::sync::mpsc::UnboundedSender<String>) {
        let mut guard = self.output_tx.lock().await;
        *guard = Some(tx);
    }
}

#[async_trait]
impl OutboundAdapter for LocalOutboundAdapter {
    fn channel_type(&self) -> &str {
        "local"
    }

    async fn connect(&self) -> Result<()> {
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        Ok(())
    }

    fn clean_body(&self, raw_body: &str) -> String {
        raw_body.to_string()
    }

    async fn send_reply(
        &self,
        _original: &InboundMessage,
        _reply_text: &str,
        _thread_path: &Path,
        _message_dir: &str,
        _attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult> {
        unimplemented!()
    }

    async fn send_message(
        &self,
        _recipient: &str,
        _subject: &str,
        _body: &str,
    ) -> Result<SendResult> {
        unimplemented!()
    }
}
