//! Local TUI channel inbound adapter and matcher.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapterOptions, InboundMessage, MessageContent,
    PatternMatch,
};

/// Local channel-specific pattern matching and thread name derivation.
pub struct LocalMatcher {
    channel_name: String,
}

impl LocalMatcher {
    /// Create a new local matcher.
    pub fn new(channel_name: String) -> Self {
        Self { channel_name }
    }
}

impl ChannelMatcher for LocalMatcher {
    fn channel_type(&self) -> &str {
        "local"
    }

    fn derive_thread_name(
        &self,
        _message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Each local channel has exactly one thread named after the channel.
        self.channel_name.clone()
    }

    fn match_message(
        &self,
        _message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        // Local input is always for this channel — match the first enabled pattern.
        patterns.iter().find(|p| p.enabled).map(|p| PatternMatch {
            pattern_name: p.name.clone(),
            channel: "local".to_string(),
            matches: HashMap::new(),
        })
    }
}

/// Type alias for the TUI spawner closure.
///
/// Takes `(input_tx, output_rx)` — the TUI's end of the channels.
/// Returns a JoinHandle for the spawned blocking TUI task.
pub type TuiSpawner = Box<
    dyn FnOnce(
            tokio::sync::mpsc::UnboundedSender<String>,
            tokio::sync::mpsc::UnboundedReceiver<String>,
        ) -> tokio::task::JoinHandle<Result<()>>
        + Send,
>;

/// Local TUI inbound adapter.
///
/// Bridges TUI (blocking terminal I/O) ↔ async message processing
/// via mpsc channels and `tokio::task::spawn_blocking`.
pub struct LocalInboundAdapter {
    channel_name: String,
    /// Shared output sender — set here, read by `LocalOutboundAdapter`.
    output_tx_arc: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>,
    /// TUI spawner closure — consumed in `start()`.
    run_tui: std::sync::Mutex<Option<TuiSpawner>>,
}

impl LocalInboundAdapter {
    /// Create a new local inbound adapter.
    pub fn new(
        channel_name: String,
        output_tx_arc: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>,
        run_tui: TuiSpawner,
    ) -> Self {
        Self {
            channel_name,
            output_tx_arc,
            run_tui: std::sync::Mutex::new(Some(run_tui)),
        }
    }
}

impl ChannelMatcher for LocalInboundAdapter {
    fn channel_type(&self) -> &str {
        "local"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        LocalMatcher::new(self.channel_name.clone()).derive_thread_name(
            message,
            patterns,
            pattern_match,
        )
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        LocalMatcher::new(self.channel_name.clone()).match_message(message, patterns)
    }
}

#[async_trait]
impl jyc_types::InboundAdapter for LocalInboundAdapter {
    async fn start(&self, options: InboundAdapterOptions, cancel: CancellationToken) -> Result<()> {
        let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (output_tx, output_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        // Set output_tx so the outbound adapter can send replies to the TUI.
        {
            let mut guard = self.output_tx_arc.lock().await;
            *guard = Some(output_tx);
        }
        tracing::info!(channel = %self.channel_name, "Local outbound output_tx set");

        // Consume the TUI spawner closure.
        let run_tui =
            self.run_tui.lock().unwrap().take().ok_or_else(|| {
                anyhow::anyhow!("LocalInboundAdapter::start() called more than once")
            })?;

        // Spawn the TUI in a blocking task.
        let tui_handle = (run_tui)(input_tx, output_rx);

        // Process messages from the TUI.
        let mut process_error = None;
        loop {
            tokio::select! {
                text = input_rx.recv() => {
                    let Some(text) = text else {
                        // Channel closed — TUI exited.
                        break;
                    };

                    if cancel.is_cancelled() {
                        break;
                    }

                    let message = InboundMessage {
                        id: uuid::Uuid::new_v4().to_string(),
                        channel: self.channel_name.clone(),
                        channel_uid: "local".to_string(),
                        sender: "user".to_string(),
                        sender_address: "local".to_string(),
                        recipients: vec![],
                        topic: self.channel_name.clone(),
                        content: MessageContent {
                            text: Some(text),
                            html: None,
                            markdown: None,
                        },
                        timestamp: chrono::Utc::now(),
                        thread_refs: None,
                        reply_to_id: None,
                        external_id: None,
                        attachments: vec![],
                        metadata: HashMap::new(),
                        matched_pattern: None,
                    };

                    if let Err(e) = (options.on_message)(message) {
                        process_error = Some(e);
                        break;
                    }
                }
                _ = cancel.cancelled() => {
                    break;
                }
            }
        }

        // Wait for the TUI task to finish.
        let tui_result = tui_handle.await;

        if let Some(e) = process_error {
            return Err(e);
        }
        match tui_result {
            Ok(Ok(())) => {
                tracing::info!(channel = %self.channel_name, "Local TUI stopped cleanly");
                Ok(())
            }
            Ok(Err(e)) => {
                tracing::error!(channel = %self.channel_name, error = %e, "Local TUI error");
                Err(e)
            }
            Err(e) => {
                tracing::error!(channel = %self.channel_name, error = %e, "Local TUI task panicked");
                Err(anyhow::anyhow!("Local TUI task panicked: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_message() -> InboundMessage {
        InboundMessage {
            id: "test".to_string(),
            channel: "local".to_string(),
            channel_uid: "user".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
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
            metadata: HashMap::new(),
            matched_pattern: None,
        }
    }

    #[test]
    fn test_derive_thread_name() {
        let matcher = LocalMatcher::new("my-local".to_string());
        let msg = create_test_message();
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "my-local");
    }

    #[test]
    fn test_match_message_first_enabled() {
        let matcher = LocalMatcher::new("my-local".to_string());
        let msg = create_test_message();

        let patterns = vec![
            ChannelPattern {
                name: "p1".to_string(),
                channel: "local".to_string(),
                enabled: true,
                ..Default::default()
            },
            ChannelPattern {
                name: "p2".to_string(),
                channel: "local".to_string(),
                enabled: false,
                ..Default::default()
            },
        ];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "p1");
    }

    #[test]
    fn test_match_message_skips_disabled() {
        let matcher = LocalMatcher::new("my-local".to_string());
        let msg = create_test_message();

        let patterns = vec![
            ChannelPattern {
                name: "p1".to_string(),
                channel: "local".to_string(),
                enabled: false,
                ..Default::default()
            },
            ChannelPattern {
                name: "p2".to_string(),
                channel: "local".to_string(),
                enabled: true,
                ..Default::default()
            },
        ];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "p2");
    }

    #[test]
    fn test_match_message_none_when_all_disabled() {
        let matcher = LocalMatcher::new("my-local".to_string());
        let msg = create_test_message();

        let patterns = vec![ChannelPattern {
            name: "p1".to_string(),
            channel: "local".to_string(),
            enabled: false,
            ..Default::default()
        }];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }
}
