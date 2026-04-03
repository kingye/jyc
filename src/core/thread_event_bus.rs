use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::core::thread_event::ThreadEvent;

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
    tx: mpsc::Sender<ThreadEvent>,
    subscribers: Mutex<Vec<mpsc::Sender<ThreadEvent>>>,
}

impl SimpleThreadEventBus {
    /// Create a new thread event bus with the given capacity.
    /// 
    /// The capacity determines how many events can be queued before
    /// `publish` starts blocking or returning errors.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = mpsc::channel(capacity);
        Self {
            tx,
            subscribers: Mutex::new(Vec::new()),
        }
    }
    
    /// Internal method to forward events to all subscribers.
    async fn forward_to_subscribers(&self, event: ThreadEvent) {
        let mut subscribers = self.subscribers.lock().await;
        
        // Remove closed subscribers
        subscribers.retain(|subscriber| !subscriber.is_closed());
        
        // Forward event to all active subscribers
        for subscriber in subscribers.iter() {
            // Clone the event for each subscriber
            let event_clone = event.clone();
            let subscriber_clone = subscriber.clone();
            
            // Spawn a task to send without blocking
            tokio::spawn(async move {
                let _ = subscriber_clone.send(event_clone).await;
            });
        }
    }
}

#[async_trait]
impl ThreadEventBus for SimpleThreadEventBus {
    async fn publish(&self, event: ThreadEvent) -> Result<()> {
        // Send to the main channel
        self.tx
            .send(event.clone())
            .await
            .context("Failed to publish event to main channel")?;
        
        // Forward to all subscribers
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