//! `ThreadManager` impl block: events.rs methods.
//!
//! Extracted from the monolithic `thread_manager.rs`.

use std::sync::Arc;

use crate::thread_event_bus::{SimpleThreadEventBus, ThreadEventBusRef};

/// Per-thread queue stats.
use super::ThreadManager;

impl ThreadManager {
    pub async fn get_event_bus(&self, thread_name: &str) -> Option<ThreadEventBusRef> {
        if !self.enable_events {
            return None;
        }

        let event_buses = self.event_buses.lock().await;
        event_buses.get(thread_name).cloned()
    }

    /// Check whether a thread has an active (open) message queue.
    ///
    /// Used by the ActivityTracker to distinguish between a thread whose
    /// event bus was cleaned up because the worker exited (no active queue)
    /// versus a thread that should have an event bus but doesn't yet.
    pub async fn has_active_queue(&self, thread_name: &str) -> bool {
        let queues = self.thread_queues.lock().await;
        queues.get(thread_name).is_some_and(|s| !s.is_closed())
    }

    /// Create a new event bus for a thread if one doesn't exist.
    ///
    /// Returns the event bus for the thread, or None if event support is disabled.
    pub async fn get_or_create_event_bus(&self, thread_name: &str) -> Option<ThreadEventBusRef> {
        if !self.enable_events {
            return None;
        }

        let mut event_buses = self.event_buses.lock().await;

        // Check if event bus already exists
        if let Some(event_bus) = event_buses.get(thread_name) {
            return Some(event_bus.clone());
        }

        // Create new event bus
        let event_bus = Arc::new(SimpleThreadEventBus::new(10)); // Capacity of 10 events

        event_buses.insert(thread_name.to_string(), event_bus.clone());
        Some(event_bus)
    }

    /// Publish an `IncomingMessage` event on the thread's event bus.
    ///
    /// Called from `enqueue()` after a message is successfully queued.
    /// Non-blocking — failures are silently ignored (event bus may not exist
    /// for brand-new threads until the worker creates it).
    pub(crate) async fn publish_incoming_message(
        &self,
        thread_name: &str,
        sender: &str,
        text: &str,
    ) {
        if !self.enable_events {
            return;
        }
        if let Some(bus) = self.get_event_bus(thread_name).await {
            let event = crate::thread_event::ThreadEvent::IncomingMessage {
                thread_name: thread_name.to_string(),
                sender: sender.to_string(),
                text: text.to_string(),
                timestamp: chrono::Utc::now(),
            };
            if let Err(e) = bus.publish(event).await {
                tracing::trace!(
                    thread = %thread_name,
                    error = %e,
                    "Failed to publish IncomingMessage event"
                );
            }
        }
    }

    /// Publish a `ReplySent` event on the thread's event bus.
    ///
    /// Called after `outbound.send_reply()` succeeds. Enables the dashboard
    /// to display live AI replies for non-WebSocket threads.
    pub(crate) async fn publish_reply_sent(&self, thread_name: &str, text: &str) {
        if !self.enable_events {
            return;
        }
        if let Some(bus) = self.get_event_bus(thread_name).await {
            let event = crate::thread_event::ThreadEvent::ReplySent {
                thread_name: thread_name.to_string(),
                text: text.to_string(),
                timestamp: chrono::Utc::now(),
            };
            if let Err(e) = bus.publish(event).await {
                tracing::trace!(
                    thread = %thread_name,
                    error = %e,
                    "Failed to publish ReplySent event"
                );
            }
        }
    }
}
