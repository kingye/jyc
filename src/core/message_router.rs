use std::sync::Arc;

use crate::channels::email::inbound;
use crate::channels::types::{ChannelPattern, InboundMessage};
use crate::core::email_parser;
use crate::core::thread_manager::ThreadManager;

/// Routes inbound messages to the appropriate thread queue.
///
/// Delegates pattern matching and thread name derivation to channel-specific
/// logic, then dispatches to the ThreadManager.
pub struct MessageRouter {
    thread_manager: Arc<ThreadManager>,
}

impl MessageRouter {
    pub fn new(thread_manager: Arc<ThreadManager>) -> Self {
        Self { thread_manager }
    }

    /// Route a message from an email channel.
    ///
    /// 1. Match against patterns
    /// 2. Derive thread name
    /// 3. Enqueue for processing
    pub async fn route_email(
        &self,
        mut message: InboundMessage,
        patterns: &[ChannelPattern],
    ) {
        // 1. Pattern matching
        let pattern_match = match inbound::match_message(&message, patterns) {
            Some(m) => {
                tracing::info!(
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
                    sender = %message.sender_address,
                    topic = %message.topic,
                    "No pattern matched, skipping"
                );
                return;
            }
        };

        // 2. Derive thread name
        let subject_prefixes: Vec<String> = patterns
            .iter()
            .filter_map(|p| p.rules.subject.as_ref())
            .filter_map(|s| s.prefix.as_ref())
            .flatten()
            .cloned()
            .collect();

        let thread_name =
            email_parser::derive_thread_name(&message.topic, &subject_prefixes);

        tracing::info!(
            thread = %thread_name,
            pattern = %pattern_match.pattern_name,
            "Routing message to thread"
        );

        // 3. Get attachment config from the matched pattern
        let attachment_config = patterns
            .iter()
            .find(|p| p.name == pattern_match.pattern_name)
            .and_then(|p| p.attachments.clone());

        // 4. Enqueue
        self.thread_manager
            .enqueue(message, thread_name, pattern_match, attachment_config)
            .await;
    }
}
