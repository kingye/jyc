use std::sync::Arc;

use crate::channels::email::inbound;
use crate::channels::types::{ChannelPattern, InboundMessage};
use crate::core::email_parser;
use crate::core::thread_manager::ThreadManager;

/// Routes inbound messages to the appropriate thread queue.
pub struct MessageRouter {
    thread_manager: Arc<ThreadManager>,
}

impl MessageRouter {
    pub fn new(thread_manager: Arc<ThreadManager>) -> Self {
        Self { thread_manager }
    }

    /// Route a message from an email channel.
    pub async fn route_email(
        &self,
        mut message: InboundMessage,
        patterns: &[ChannelPattern],
    ) {
        let ch = &message.channel;

        // 1. Pattern matching
        let pattern_match = match inbound::match_message(&message, patterns) {
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

        // 4. Enqueue
        self.thread_manager
            .enqueue(message, thread_name, pattern_match, attachment_config)
            .await;
    }
}
