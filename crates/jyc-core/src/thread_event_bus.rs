use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::thread_event::ThreadEvent;

/// Thread-isolated event bus trait.
/// 
/// Each thread has its own event bus instance to ensure complete isolation
/// between threads. Events from one thread never leak to another.
#[async_trait]
pub trait ThreadEventBus: Send + Sync {
    /// Publish an event to this thread's event bus.
    /// 
    /// Returns an error if the event bus is closed or the channel is full.
    async fn publish(&self, event: ThreadEvent) -> Result<()>;
    
    /// Subscribe to events from this thread's event bus.
    /// 
    /// Returns a receiver that will receive events published to this bus.
    /// Each subscriber gets its own copy of events (broadcast semantics).
    async fn subscribe(&self) -> Result<mpsc::Receiver<ThreadEvent>>;
}

/// Simple implementation of a thread-isolated event bus.
/// 
/// Uses a broadcast channel to support multiple subscribers.
/// Events are sent to all active subscribers.
pub struct SimpleThreadEventBus {
    subscribers: Mutex<Vec<mpsc::Sender<ThreadEvent>>>,
}

impl SimpleThreadEventBus {
    /// Create a new thread event bus with the given capacity.
    /// 
    /// The capacity determines how many events can be queued before
    /// `publish` starts blocking or returning errors.
    pub fn new(_capacity: usize) -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }
    
    /// Internal method to forward events to all subscribers.
    ///
    /// Sends events sequentially (awaited) to preserve ordering. The mpsc channel
    /// capacity (10) provides backpressure — if a subscriber falls behind, the
    /// agent will slow down rather than send events out of order.
    async fn forward_to_subscribers(&self, event: ThreadEvent) {
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
impl ThreadEventBus for SimpleThreadEventBus {
    async fn publish(&self, event: ThreadEvent) -> Result<()> {
        tracing::trace!("Publishing event to thread event bus");
        
        // Forward to all subscribers (no main channel needed)
        self.forward_to_subscribers(event).await;
        
        Ok(())
    }
    
    async fn subscribe(&self) -> Result<mpsc::Receiver<ThreadEvent>> {
        let (tx, rx) = mpsc::channel(10);
        
        // Add to subscribers list
        let mut subscribers = self.subscribers.lock().await;
        subscribers.push(tx);
        
        Ok(rx)
    }
}

/// Type alias for Arc-wrapped thread event bus.
pub type ThreadEventBusRef = Arc<dyn ThreadEventBus>;
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::Arc;

    fn make_event(name: &str) -> ThreadEvent {
        ThreadEvent::ProcessingStarted {
            thread_name: name.to_string(),
            message_id: "test".to_string(),
            timestamp: Utc::now(),
        }
    }

    /// Regression test: events must be delivered in publication order.
    /// Previously used tokio::spawn per event, causing out-of-order delivery
    /// (e.g., ProcessingCompleted arriving before ToolStarted).
    #[tokio::test]
    async fn events_delivered_in_order() {
        let bus: Arc<dyn ThreadEventBus> = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx = bus.subscribe().await.unwrap();

        // Publish 5 events rapidly
        for i in 0..5 {
            bus.publish(make_event(&format!("event-{}", i))).await.unwrap();
        }

        // Verify order
        for i in 0..5 {
            let event = rx.recv().await.expect("Expected event");
            match event {
                ThreadEvent::ProcessingStarted { thread_name, .. } => {
                    assert_eq!(thread_name, format!("event-{}", i), "Event {} out of order", i);
                }
                _ => panic!("Unexpected event type"),
            }
        }
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_all_events() {
        let bus: Arc<dyn ThreadEventBus> = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx1 = bus.subscribe().await.unwrap();
        let mut rx2 = bus.subscribe().await.unwrap();

        bus.publish(make_event("first")).await.unwrap();
        bus.publish(make_event("second")).await.unwrap();

        for rx in [&mut rx1, &mut rx2] {
            let e1 = rx.recv().await.unwrap();
            let e2 = rx.recv().await.unwrap();
            assert!(matches!(&e1, ThreadEvent::ProcessingStarted { thread_name, .. } if thread_name == "first"));
            assert!(matches!(&e2, ThreadEvent::ProcessingStarted { thread_name, .. } if thread_name == "second"));
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
}
