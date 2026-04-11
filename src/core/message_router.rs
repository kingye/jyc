use std::sync::Arc;

use crate::channels::types::{ChannelMatcher, ChannelPattern, InboundMessage};
use crate::core::message_storage::MessageStorage;
use crate::core::thread_manager::ThreadManager;

/// Routes inbound messages to the appropriate thread queue.
///
/// Channel-agnostic: delegates pattern matching and thread name derivation
/// to the `ChannelMatcher` provided by the caller.
pub struct MessageRouter {
    thread_manager: Arc<ThreadManager>,
    storage: Arc<MessageStorage>,
}

impl MessageRouter {
    pub fn new(thread_manager: Arc<ThreadManager>, storage: Arc<MessageStorage>) -> Self {
        Self { thread_manager, storage }
    }

    /// Route a message from any channel type.
    ///
    /// Pattern matching and thread name derivation are delegated to
    /// the channel-specific `ChannelMatcher` implementation.
    pub async fn route(
        &self,
        matcher: &dyn ChannelMatcher,
        mut message: InboundMessage,
        patterns: &[ChannelPattern],
    ) {
        let ch = &message.channel;

        // 1. Pattern matching (channel-specific)
        let pattern_match = match matcher.match_message(&message, patterns) {
            Some(m) => {
                tracing::info!(
                    channel = %ch,
                    pattern = %m.pattern_name,
                    sender = %message.sender_address,
                    topic = %message.topic,
                    "Pattern matched"
                );
                message.matched_pattern = Some(m.pattern_name.clone());
                Some(m)
            }
            None => {
                // Check if we should store unmatched messages for this channel
                if matcher.store_unmatched_messages() {
                    tracing::debug!(
                        channel = %ch,
                        sender = %message.sender_address,
                        topic = %message.topic,
                        "No pattern matched, but storing for channel context"
                    );
                    // Store the message but don't process it
                    self.store_unmatched_message(matcher, &message, patterns).await;
                } else {
                    tracing::debug!(
                        channel = %ch,
                        sender = %message.sender_address,
                        topic = %message.topic,
                        "No pattern matched, skipping"
                    );
                }
                return;
            }
        };

        // 2. Derive thread name (channel-specific)
        let thread_name =
            matcher.derive_thread_name(&message, patterns, pattern_match.as_ref());

        // Safe to unwrap because we checked is_some() above
        let pattern_name = pattern_match.as_ref().expect("pattern_match should be Some").pattern_name.clone();
        
        tracing::info!(
            channel = %ch,
            thread = %thread_name,
            pattern = %pattern_name,
            "Routing to thread"
        );

        // 3. Get attachment config and template from the matched pattern
        let matched_pattern_name = pattern_name;
        let attachment_config = patterns
            .iter()
            .find(|p| p.name == matched_pattern_name)
            .and_then(|p| p.attachments.clone());
        
        // Store template name in message metadata for thread initialization
        if let Some(template) = patterns
            .iter()
            .find(|p| p.name == matched_pattern_name)
            .and_then(|p| p.template.clone())
        {
            message.metadata.insert("template".to_string(), serde_json::Value::String(template));
        }

        // 4. Enqueue (channel-agnostic)
        let pm = pattern_match.expect("pattern_match should be Some");
        self.thread_manager
            .enqueue(message, thread_name, pm, attachment_config)
            .await;
    }

    /// Store an unmatched message for channels that want to keep full conversation context.
    async fn store_unmatched_message(
        &self,
        matcher: &dyn ChannelMatcher,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) {
        // Derive thread name even for unmatched messages
        let thread_name = matcher.derive_thread_name(message, patterns, None);

        tracing::info!(
            channel = %message.channel,
            thread = %thread_name,
            sender = %message.sender_address,
            topic = %message.topic,
            "Storing unmatched message"
        );

        // Store the message without processing (is_matched = false)
        match self.storage.store_with_match(message, &thread_name, false, None).await {
            Ok(store_result) => {
                tracing::debug!(
                    channel = %message.channel,
                    thread = %thread_name,
                    path = %store_result.message_dir,
                    "Unmatched message stored"
                );
            }
            Err(e) => {
                tracing::error!(
                    channel = %message.channel,
                    thread = %thread_name,
                    error = %e,
                    "Failed to store unmatched message"
                );
            }
        }
    }
}
