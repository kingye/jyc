use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::thread_event::ThreadEvent;

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
/// Uses a single-producer, multi-consumer channel with bounded capacity
/// to prevent unbounded memory growth.
pub struct SimpleThreadEventBus {
    tx: mpsc::Sender<ThreadEvent>,
}

impl SimpleThreadEventBus {
    /// Create a new thread event bus with the given capacity.
    /// 
    /// The capacity determines how many events can be queued before
    /// `publish` starts blocking or returning errors.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = mpsc::channel(capacity);
        Self { tx }
    }
}

#[async_trait]
impl ThreadEventBus for SimpleThreadEventBus {
    async fn publish(&self, event: ThreadEvent) -> Result<()> {
        self.tx
            .send(event)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to publish event: {}", e))
    }
    
    async fn subscribe(&self) -> Result<mpsc::Receiver<ThreadEvent>> {
        // For SimpleThreadEventBus, we need to create a new channel
        // and forward events from the main channel to each subscriber.
        // This is a simplified implementation - in practice we might
        // want to use a broadcast channel or similar.
        let (tx, rx) = mpsc::channel(10);
        
        // Clone the sender for the forwarding task
        let main_tx = self.tx.clone();
        
        // In a real implementation, we'd set up forwarding here.
        // For now, we'll return a receiver that will never receive anything
        // (placeholder implementation).
        Ok(rx)
    }
}

/// Type alias for Arc-wrapped thread event bus.
pub type ThreadEventBusRef = Arc<dyn ThreadEventBus>;