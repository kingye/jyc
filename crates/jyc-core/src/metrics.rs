use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// Metric events reported by components.
#[derive(Debug)]
pub enum MetricEvent {
    MessageReceived { topic: String },
    MessageMatched { topic: String },
    ReplyByTool { topic: String },
    ReplyByFallback { topic: String },
    ProcessingError { topic: String, error: String },
    QueueDropped { topic: String },
}

/// Per-topic statistics.
#[derive(Debug, Default, Clone)]
pub struct TopicStats {
    pub received: u64,
    pub processed: u64,
    pub errors: u64,
}

/// Accumulated health statistics, queryable by the inspect server.
#[derive(Debug, Default, Clone)]
pub struct HealthStats {
    pub messages_received: u64,
    pub messages_matched: u64,
    pub messages_processed: u64,
    pub replies_by_tool: u64,
    pub replies_by_fallback: u64,
    pub errors: u64,
    pub dropped: u64,
    pub per_topic: HashMap<String, TopicStats>,
}

/// Shared reference to accumulated health stats.
pub type SharedHealthStats = Arc<Mutex<HealthStats>>;

/// Handle for reporting metrics from components.
///
/// Clone and pass to components that need to report events.
/// Events are sent asynchronously to the background collector task.
#[derive(Clone)]
pub struct MetricsHandle {
    sender: mpsc::Sender<MetricEvent>,
}

impl MetricsHandle {
    /// Report a message received.
    pub fn message_received(&self, topic: &str) {
        let _ = self.sender.try_send(MetricEvent::MessageReceived {
            topic: topic.to_string(),
        });
    }

    /// Report a pattern match.
    pub fn message_matched(&self, topic: &str) {
        let _ = self.sender.try_send(MetricEvent::MessageMatched {
            topic: topic.to_string(),
        });
    }

    /// Report a reply sent by MCP tool.
    pub fn reply_by_tool(&self, topic: &str) {
        let _ = self.sender.try_send(MetricEvent::ReplyByTool {
            topic: topic.to_string(),
        });
    }

    /// Report a reply sent by fallback.
    pub fn reply_by_fallback(&self, topic: &str) {
        let _ = self.sender.try_send(MetricEvent::ReplyByFallback {
            topic: topic.to_string(),
        });
    }

    /// Report a processing error.
    pub fn processing_error(&self, topic: &str, error: &str) {
        let _ = self.sender.try_send(MetricEvent::ProcessingError {
            topic: topic.to_string(),
            error: error.to_string(),
        });
    }

    /// Report a dropped message (queue full).
    pub fn queue_dropped(&self, topic: &str) {
        let _ = self.sender.try_send(MetricEvent::QueueDropped {
            topic: topic.to_string(),
        });
    }

    /// Create a no-op handle (for when metrics collection is disabled).
    pub fn noop() -> Self {
        let (tx, _rx) = mpsc::channel(1);
        Self { sender: tx }
    }
}

/// Background metrics collector.
///
/// Receives `MetricEvent`s from components and accumulates them into
/// `SharedHealthStats`. The inspect server reads these stats to report
/// to the dashboard.
pub struct MetricsCollector {
    cancel: CancellationToken,
}

impl MetricsCollector {
    pub fn new(cancel: CancellationToken) -> Self {
        Self { cancel }
    }

    /// Start the metrics collector. Returns a handle for sending events
    /// and the shared stats reference for querying.
    pub fn start(
        self,
    ) -> (
        MetricsHandle,
        SharedHealthStats,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = mpsc::channel::<MetricEvent>(1000);
        let handle = MetricsHandle { sender: tx };
        let stats = Arc::new(Mutex::new(HealthStats::default()));
        let stats_clone = stats.clone();
        let task = tokio::spawn(
            self.run(rx, stats_clone)
                .instrument(tracing::info_span!("metrics")),
        );
        (handle, stats, task)
    }

    /// Background task: receive events and accumulate stats.
    async fn run(self, mut rx: mpsc::Receiver<MetricEvent>, stats: SharedHealthStats) {
        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(event) => {
                            let mut s = stats.lock().await;
                            Self::handle_event(event, &mut s);
                        }
                        None => break, // All senders dropped
                    }
                }
                _ = self.cancel.cancelled() => {
                    break;
                }
            }
        }
        tracing::debug!("Metrics collector stopped");
    }

    fn handle_event(event: MetricEvent, stats: &mut HealthStats) {
        match event {
            MetricEvent::MessageReceived { ref topic } => {
                stats.messages_received += 1;
                stats.per_topic.entry(topic.clone()).or_default().received += 1;
            }
            MetricEvent::MessageMatched { .. } => {
                stats.messages_matched += 1;
            }
            MetricEvent::ReplyByTool { ref topic } => {
                stats.messages_processed += 1;
                stats.replies_by_tool += 1;
                stats.per_topic.entry(topic.clone()).or_default().processed += 1;
            }
            MetricEvent::ReplyByFallback { ref topic } => {
                stats.messages_processed += 1;
                stats.replies_by_fallback += 1;
                stats.per_topic.entry(topic.clone()).or_default().processed += 1;
            }
            MetricEvent::ProcessingError { ref topic, .. } => {
                stats.errors += 1;
                stats.per_topic.entry(topic.clone()).or_default().errors += 1;
            }
            MetricEvent::QueueDropped { .. } => {
                stats.dropped += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_metrics_handle_noop() {
        let handle = MetricsHandle::noop();
        // Should not panic — events are silently dropped
        handle.message_received("test-topic");
        handle.processing_error("test-topic", "some error");
        handle.queue_dropped("test-topic");
    }

    #[tokio::test]
    async fn test_metrics_collector_accumulates_stats() {
        let cancel = CancellationToken::new();
        let collector = MetricsCollector::new(cancel.clone());
        let (handle, stats, task) = collector.start();

        // Send events
        handle.message_received("topic-1");
        handle.message_received("topic-1");
        handle.message_received("topic-2");
        handle.message_matched("topic-1");
        handle.reply_by_tool("topic-1");
        handle.processing_error("topic-2", "oops");
        handle.queue_dropped("topic-3");

        // Give the collector time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let s = stats.lock().await;
        assert_eq!(s.messages_received, 3);
        assert_eq!(s.messages_matched, 1);
        assert_eq!(s.messages_processed, 1);
        assert_eq!(s.replies_by_tool, 1);
        assert_eq!(s.errors, 1);
        assert_eq!(s.dropped, 1);

        // Per-topic stats
        assert_eq!(s.per_topic["topic-1"].received, 2);
        assert_eq!(s.per_topic["topic-1"].processed, 1);
        assert_eq!(s.per_topic["topic-2"].received, 1);
        assert_eq!(s.per_topic["topic-2"].errors, 1);

        drop(s);

        // Shutdown
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn test_metrics_collector_stops_on_cancel() {
        let cancel = CancellationToken::new();
        let collector = MetricsCollector::new(cancel.clone());
        let (_handle, _stats, task) = collector.start();

        cancel.cancel();
        // Task should complete without hanging
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("collector should stop within 1 second")
            .unwrap();
    }

    #[tokio::test]
    async fn test_metrics_reply_by_fallback() {
        let cancel = CancellationToken::new();
        let collector = MetricsCollector::new(cancel.clone());
        let (handle, stats, task) = collector.start();

        handle.reply_by_fallback("topic-1");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let s = stats.lock().await;
        assert_eq!(s.messages_processed, 1);
        assert_eq!(s.replies_by_fallback, 1);
        assert_eq!(s.per_topic["topic-1"].processed, 1);
        drop(s);

        cancel.cancel();
        task.await.unwrap();
    }
}
