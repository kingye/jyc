use anyhow::Result;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

use crate::topic_event::TopicEvent;

/// Maximum number of events to buffer for late subscribers.
///
/// A small buffer suffices because the ActivityTracker discovers new topics
/// within 2 seconds. The buffer prevents events from being permanently lost
/// when no subscriber has yet subscribed (e.g., AI replies faster than the
/// ActivityTracker's discovery interval).
const EVENT_LOG_CAPACITY: usize = 20;

/// Topic-isolated event bus trait.
///
/// Each topic has its own event bus instance to ensure complete isolation
/// between topics. Events from one topic never leak to another.
#[async_trait]
pub trait TopicEventBus: Send + Sync {
    /// Publish an event to this topic's event bus.
    ///
    /// Returns an error if the event bus is closed or the channel is full.
    async fn publish(&self, event: TopicEvent) -> Result<()>;

    /// Subscribe to events from this topic's event bus.
    ///
    /// Returns a receiver that will receive events published to this bus.
    /// Each subscriber gets its own copy of events (broadcast semantics).
    /// Late subscribers receive previously published events from the buffer.
    async fn subscribe(&self) -> Result<mpsc::Receiver<TopicEvent>>;
}

/// Simple implementation of a topic-isolated event bus.
///
/// Uses a broadcast channel to support multiple subscribers.
/// Events are sent to all active subscribers.
///
/// Buffers recent events so that late subscribers (e.g., ActivityTracker
/// discovering a topic after events were published) do not miss them.
pub struct SimpleThreadEventBus {
    subscribers: Mutex<Vec<mpsc::Sender<TopicEvent>>>,
    event_log: Mutex<VecDeque<TopicEvent>>,
}

impl SimpleThreadEventBus {
    /// Create a new topic event bus with the given capacity.
    ///
    /// The capacity determines how many events can be queued before
    /// `publish` starts blocking or returning errors.
    pub fn new(_capacity: usize) -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            event_log: Mutex::new(VecDeque::with_capacity(EVENT_LOG_CAPACITY)),
        }
    }

    /// Internal method to forward events to all subscribers.
    ///
    /// Sends events sequentially (awaited) to preserve ordering. The mpsc channel
    /// capacity (10) provides backpressure — if a subscriber falls behind, the
    /// agent will slow down rather than send events out of order.
    async fn forward_to_subscribers(&self, event: &TopicEvent) {
        let mut subscribers = self.subscribers.lock().await;

        // Remove closed subscribers
        subscribers.retain(|subscriber| !subscriber.is_closed());

        // Forward event to all active subscribers IN ORDER
        for subscriber in subscribers.iter() {
            let _ = subscriber.send(event.clone()).await;
        }
    }
}

#[async_trait]
impl TopicEventBus for SimpleThreadEventBus {
    async fn publish(&self, event: TopicEvent) -> Result<()> {
        tracing::trace!("Publishing event to topic event bus");

        // Buffer the event so late subscribers can catch up.
        // Drop oldest if buffer is full to prevent unbounded growth.
        let mut log = self.event_log.lock().await;
        if log.len() >= EVENT_LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(event.clone());
        drop(log);

        // Forward to all active subscribers
        self.forward_to_subscribers(&event).await;

        Ok(())
    }

    async fn subscribe(&self) -> Result<mpsc::Receiver<TopicEvent>> {
        let (tx, rx) = mpsc::channel(10);

        let mut subscribers = self.subscribers.lock().await;

        // Replay buffered events to the new subscriber BEFORE adding it to the
        // subscribers list. This prevents duplicate delivery: the replay goes
        // to the new subscriber only, and subsequent live events go to all
        // subscribers (including the new one).
        let log = self.event_log.lock().await;
        for event in log.iter() {
            let _ = tx.send(event.clone()).await;
        }
        drop(log);

        // Add to subscribers list for future events
        subscribers.push(tx);

        Ok(rx)
    }
}

/// Type alias for Arc-wrapped topic event bus.
pub type TopicEventBusRef = Arc<dyn TopicEventBus>;
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::Arc;

    fn make_event(name: &str) -> TopicEvent {
        TopicEvent::ProcessingStarted {
            topic_name: name.to_string(),
            message_id: "test".to_string(),
            timestamp: Utc::now(),
        }
    }

    /// Regression test: events must be delivered in publication order.
    /// Previously used tokio::spawn per event, causing out-of-order delivery
    /// (e.g., ProcessingCompleted arriving before ToolStarted).
    #[tokio::test]
    async fn events_delivered_in_order() {
        let bus: Arc<dyn TopicEventBus> = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx = bus.subscribe().await.unwrap();

        // Publish 5 events rapidly
        for i in 0..5 {
            bus.publish(make_event(&format!("event-{}", i)))
                .await
                .unwrap();
        }

        // Verify order
        for i in 0..5 {
            let event = rx.recv().await.expect("Expected event");
            match event {
                TopicEvent::ProcessingStarted { topic_name, .. } => {
                    assert_eq!(
                        topic_name,
                        format!("event-{}", i),
                        "Event {} out of order",
                        i
                    );
                }
                _ => panic!("Unexpected event type"),
            }
        }
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_all_events() {
        let bus: Arc<dyn TopicEventBus> = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx1 = bus.subscribe().await.unwrap();
        let mut rx2 = bus.subscribe().await.unwrap();

        bus.publish(make_event("first")).await.unwrap();
        bus.publish(make_event("second")).await.unwrap();

        for rx in [&mut rx1, &mut rx2] {
            let e1 = rx.recv().await.unwrap();
            let e2 = rx.recv().await.unwrap();
            assert!(
                matches!(&e1, TopicEvent::ProcessingStarted { topic_name, .. } if topic_name == "first")
            );
            assert!(
                matches!(&e2, TopicEvent::ProcessingStarted { topic_name, .. } if topic_name == "second")
            );
        }
    }

    #[tokio::test]
    async fn closed_subscribers_are_pruned() {
        let bus = Arc::new(SimpleThreadEventBus::new(10));

        // Create a subscriber and immediately drop it
        {
            let _rx = bus.subscribe().await.unwrap();
        }

        // Publish — closed subscriber should be pruned without error
        bus.publish(make_event("test")).await.unwrap();
    }

    #[tokio::test]
    async fn late_subscriber_receives_buffered_events() {
        // Regression test: the ActivityTracker discovers topics every 2 seconds,
        // but an AI reply can be generated within that window. When no subscriber
        // has yet subscribed, events must be buffered so the late subscriber
        // receives them on subscribe().
        let bus: Arc<dyn TopicEventBus> = Arc::new(SimpleThreadEventBus::new(10));

        // Publish events BEFORE any subscriber exists (simulating the race)
        bus.publish(make_event("alpha")).await.unwrap();
        bus.publish(make_event("beta")).await.unwrap();

        // Now subscribe (late) — should receive the buffered events
        let mut rx = bus.subscribe().await.unwrap();

        let e1 = rx.recv().await.expect("Expected buffered event");
        match e1 {
            TopicEvent::ProcessingStarted { topic_name, .. } => {
                assert_eq!(topic_name, "alpha");
            }
            _ => panic!("Unexpected event type"),
        }

        let e2 = rx.recv().await.expect("Expected buffered event");
        match e2 {
            TopicEvent::ProcessingStarted { topic_name, .. } => {
                assert_eq!(topic_name, "beta");
            }
            _ => panic!("Unexpected event type"),
        }

        // Live events after subscribe should also work
        bus.publish(make_event("gamma")).await.unwrap();
        let e3 = rx.recv().await.expect("Expected live event");
        match e3 {
            TopicEvent::ProcessingStarted { topic_name, .. } => {
                assert_eq!(topic_name, "gamma");
            }
            _ => panic!("Unexpected event type"),
        }
    }
}
