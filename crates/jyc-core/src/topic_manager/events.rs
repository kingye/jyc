//! `TopicManager` impl block: events.rs methods.
//!
//! Extracted from the monolithic `topic_manager.rs`.

use std::sync::Arc;

use crate::topic_event_bus::{SimpleThreadEventBus, TopicEventBusRef};

/// Per-topic queue stats.
use super::TopicManager;

impl TopicManager {
    pub async fn get_event_bus(&self, topic_name: &str) -> Option<TopicEventBusRef> {
        if !self.enable_events {
            return None;
        }

        let event_buses = self.event_buses.lock().await;
        event_buses.get(topic_name).cloned()
    }

    /// Check whether a topic has an active (open) message queue.
    ///
    /// Used by the ActivityTracker to distinguish between a topic whose
    /// event bus was cleaned up because the worker exited (no active queue)
    /// versus a topic that should have an event bus but doesn't yet.
    pub async fn has_active_queue(&self, topic_name: &str) -> bool {
        let queues = self.topic_queues.lock().await;
        queues.get(topic_name).is_some_and(|s| !s.is_closed())
    }

    /// Create a new event bus for a topic if one doesn't exist.
    ///
    /// Returns the event bus for the topic, or None if event support is disabled.
    pub async fn get_or_create_event_bus(&self, topic_name: &str) -> Option<TopicEventBusRef> {
        if !self.enable_events {
            return None;
        }

        let mut event_buses = self.event_buses.lock().await;

        // Check if event bus already exists
        if let Some(event_bus) = event_buses.get(topic_name) {
            return Some(event_bus.clone());
        }

        // Create new event bus
        let event_bus = Arc::new(SimpleThreadEventBus::new(10)); // Capacity of 10 events

        event_buses.insert(topic_name.to_string(), event_bus.clone());
        Some(event_bus)
    }

    /// Publish an `IncomingMessage` event on the topic's event bus.
    ///
    /// Called from `enqueue()` after a message is successfully queued.
    /// Non-blocking — failures are silently ignored (event bus may not exist
    /// for brand-new topics until the worker creates it).
    pub(crate) async fn publish_incoming_message(
        &self,
        topic_name: &str,
        sender: &str,
        text: &str,
    ) {
        if !self.enable_events {
            return;
        }
        if let Some(bus) = self.get_event_bus(topic_name).await {
            let event = crate::topic_event::TopicEvent::IncomingMessage {
                topic_name: topic_name.to_string(),
                sender: sender.to_string(),
                text: text.to_string(),
                timestamp: chrono::Utc::now(),
            };
            if let Err(e) = bus.publish(event).await {
                tracing::trace!(
                    topic = %topic_name,
                    error = %e,
                    "Failed to publish IncomingMessage event"
                );
            }
        }
    }

    /// Publish a `ReplySent` event on the topic's event bus.
    ///
    /// Called after `outbound.send_reply()` succeeds. Enables the dashboard
    /// to display live AI replies for non-WebSocket topics.
    pub(crate) async fn publish_reply_sent(&self, topic_name: &str, text: &str) {
        if !self.enable_events {
            return;
        }
        if let Some(bus) = self.get_event_bus(topic_name).await {
            let event = crate::topic_event::TopicEvent::ReplySent {
                topic_name: topic_name.to_string(),
                text: text.to_string(),
                timestamp: chrono::Utc::now(),
            };
            if let Err(e) = bus.publish(event).await {
                tracing::trace!(
                    topic = %topic_name,
                    error = %e,
                    "Failed to publish ReplySent event"
                );
            }
        }
    }
}
