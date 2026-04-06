use std::sync::Arc;

use crate::channels::types::{ChannelMatcher, ChannelPattern, InboundMessage};
use crate::core::thread_manager::ThreadManager;

/// Routes inbound messages to the appropriate thread queue.
///
/// Channel-agnostic: delegates pattern matching and thread name derivation
/// to the `ChannelMatcher` provided by the caller.
pub struct MessageRouter {
    thread_manager: Arc<ThreadManager>,
}

impl MessageRouter {
    pub fn new(thread_manager: Arc<ThreadManager>) -> Self {
        Self { thread_manager }
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
                m
            }
            None => {
                tracing::debug!(
                    channel = %ch,
                    sender = %message.sender_address,
                    topic = %message.topic,
                    "No pattern matched, skipping"
                );
                return;
            }
        };

        // 2. Derive thread name (channel-specific)
        let thread_name =
            matcher.derive_thread_name(&message, patterns, Some(&pattern_match));

        tracing::info!(
            channel = %ch,
            thread = %thread_name,
            pattern = %pattern_match.pattern_name,
            "Routing to thread"
        );

        // 3. Get attachment config from the matched pattern
        let attachment_config = patterns
            .iter()
            .find(|p| p.name == pattern_match.pattern_name)
            .and_then(|p| p.attachments.clone());

        // 4. Enqueue (channel-agnostic)
        self.thread_manager
            .enqueue(message, thread_name, pattern_match, attachment_config)
            .await;
    }
}
