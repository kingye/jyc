use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::channels::types::OutboundAdapter;
use crate::config::types::AlertingConfig;

/// An error entry in the alert buffer.
#[derive(Debug, Clone)]
struct ErrorEntry {
    timestamp: String,
    message: String,
    thread: Option<String>,
    context: Option<String>,
}

/// Health stats tracked from reported events, reset after each report.
#[derive(Debug, Default)]
struct HealthStats {
    messages_received: u64,
    messages_matched: u64,
    messages_processed: u64,
    replies_by_tool: u64,
    replies_by_fallback: u64,
    errors: u64,
    dropped: u64,
    per_thread: HashMap<String, ThreadStats>,
    period_start: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct ThreadStats {
    received: u64,
    processed: u64,
    errors: u64,
}

/// Event types that can be reported to the alert service.
#[derive(Debug)]
pub enum AlertEvent {
    Error { message: String, thread: Option<String>, context: Option<String> },
    MessageReceived { thread: String },
    MessageMatched { thread: String },
    ReplyByTool { thread: String },
    ReplyByFallback { thread: String },
    ProcessingError { thread: String, error: String },
    QueueDropped { thread: String },
}

/// Handle for unified logging + alerting.
///
/// Components call `alert.info(...)`, `alert.error(...)` etc. Each call:
/// 1. Delegates to `tracing` for console/file logging
/// 2. Sends a structured event to the alert service for stats + error buffering
///
/// Clone this and pass to components that need to log + report.
#[derive(Clone)]
pub struct AppLogger {
    sender: mpsc::Sender<AlertEvent>,
}

impl AppLogger {
    // --- Logging methods (delegate to tracing + send to alert service) ---

    /// Log at INFO level and track the event.
    pub fn info(&self, message: &str) {
        tracing::info!("{}", message);
    }

    /// Log at DEBUG level.
    pub fn debug(&self, message: &str) {
        tracing::debug!("{}", message);
    }

    /// Log at WARN level.
    pub fn warn(&self, message: &str) {
        tracing::warn!("{}", message);
    }

    /// Log at ERROR level and buffer for alert digest.
    pub fn error(&self, message: &str, thread: Option<&str>, context: Option<&str>) {
        tracing::error!(
            thread = thread.unwrap_or("-"),
            "{}",
            message,
        );
        let _ = self.sender.try_send(AlertEvent::Error {
            message: message.to_string(),
            thread: thread.map(|s| s.to_string()),
            context: context.map(|s| s.to_string()),
        });
    }

    // --- Structured event methods (log + track stats) ---

    /// Report a message received.
    pub fn message_received(&self, thread: &str, sender: &str, topic: &str) {
        tracing::info!(thread = %thread, sender = %sender, topic = %topic, "Message received");
        let _ = self.sender.try_send(AlertEvent::MessageReceived {
            thread: thread.to_string(),
        });
    }

    /// Report a pattern match.
    pub fn message_matched(&self, thread: &str, pattern: &str, sender: &str) {
        tracing::info!(thread = %thread, pattern = %pattern, sender = %sender, "Pattern matched");
        let _ = self.sender.try_send(AlertEvent::MessageMatched {
            thread: thread.to_string(),
        });
    }

    /// Report a reply sent by MCP tool.
    pub fn reply_by_tool(&self, thread: &str, model: Option<&str>) {
        tracing::info!(thread = %thread, model = model.unwrap_or("-"), "Reply sent by MCP tool");
        let _ = self.sender.try_send(AlertEvent::ReplyByTool {
            thread: thread.to_string(),
        });
    }

    /// Report a reply sent by fallback.
    pub fn reply_by_fallback(&self, thread: &str) {
        tracing::info!(thread = %thread, "Fallback reply sent");
        let _ = self.sender.try_send(AlertEvent::ReplyByFallback {
            thread: thread.to_string(),
        });
    }

    /// Report a processing error.
    pub fn processing_error(&self, thread: &str, error: &str) {
        tracing::error!(thread = %thread, error = %error, "Failed to process message");
        let _ = self.sender.try_send(AlertEvent::ProcessingError {
            thread: thread.to_string(),
            error: error.to_string(),
        });
    }

    /// Report a dropped message (queue full).
    pub fn queue_dropped(&self, thread: &str) {
        tracing::warn!(thread = %thread, "Queue full, dropping message");
        let _ = self.sender.try_send(AlertEvent::QueueDropped {
            thread: thread.to_string(),
        });
    }

    /// Create a no-op handle (for when alerting is disabled).
    /// Logging still works (delegates to tracing), but no events are sent.
    pub fn noop() -> Self {
        let (tx, _rx) = mpsc::channel(1);
        Self { sender: tx }
    }
}

/// Alert service — buffers errors, sends digest emails, sends health reports.
///
/// Components report events via `AppLogger`. The service runs as a background task.
pub struct AlertService {
    config: AlertingConfig,
    outbound: Arc<dyn OutboundAdapter>,
    cancel: CancellationToken,
}

impl AlertService {
    pub fn new(
        config: AlertingConfig,
        outbound: Arc<dyn OutboundAdapter>,
        cancel: CancellationToken,
    ) -> Self {
        Self { config, outbound, cancel }
    }

    /// Start the alert service. Returns a handle for sending events.
    pub fn start(self) -> (AppLogger, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel::<AlertEvent>(1000);
        let handle = AppLogger { sender: tx };
        let task = tokio::spawn(self.run(rx).instrument(tracing::info_span!("alert")));
        (handle, task)
    }

    /// Background task.
    async fn run(self, mut rx: mpsc::Receiver<AlertEvent>) {
        let flush_interval = std::time::Duration::from_secs(
            self.config.batch_interval_minutes * 60,
        );
        let health_interval = self.config.health_check.as_ref()
            .filter(|h| h.enabled)
            .map(|h| std::time::Duration::from_secs_f64(h.interval_hours * 3600.0));

        let mut error_buffer: Vec<ErrorEntry> = Vec::new();
        let max_errors = self.config.max_errors_per_batch;

        let mut health_stats = HealthStats {
            period_start: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };

        let mut flush_tick = tokio::time::interval(flush_interval);
        flush_tick.tick().await;

        let mut health_tick = if let Some(interval) = health_interval {
            let mut t = tokio::time::interval(interval);
            t.tick().await;
            Some(t)
        } else {
            None
        };

        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(event) => {
                            self.handle_event(event, &mut error_buffer, max_errors, &mut health_stats);
                        }
                        None => break,
                    }
                }

                _ = flush_tick.tick() => {
                    if !error_buffer.is_empty() {
                        self.flush_errors(&mut error_buffer).await;
                    }
                }

                _ = async {
                    if let Some(ref mut tick) = health_tick {
                        tick.tick().await
                    } else {
                        std::future::pending::<tokio::time::Instant>().await
                    }
                } => {
                    self.send_health_report(&mut health_stats).await;
                }

                _ = self.cancel.cancelled() => {
                    if !error_buffer.is_empty() {
                        self.flush_errors(&mut error_buffer).await;
                    }
                    break;
                }
            }
        }

        tracing::debug!("Alert service stopped");
    }

    fn handle_event(
        &self,
        event: AlertEvent,
        error_buffer: &mut Vec<ErrorEntry>,
        max_errors: usize,
        stats: &mut HealthStats,
    ) {
        match event {
            AlertEvent::Error { message, thread, context } => {
                if error_buffer.len() < max_errors {
                    error_buffer.push(ErrorEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        message,
                        thread,
                        context,
                    });
                }
                stats.errors += 1;
            }
            AlertEvent::MessageReceived { ref thread } => {
                stats.messages_received += 1;
                stats.per_thread.entry(thread.clone()).or_default().received += 1;
            }
            AlertEvent::MessageMatched { ref thread } => {
                stats.messages_matched += 1;
                let _ = thread; // tracked via received
            }
            AlertEvent::ReplyByTool { ref thread } => {
                stats.messages_processed += 1;
                stats.replies_by_tool += 1;
                stats.per_thread.entry(thread.clone()).or_default().processed += 1;
            }
            AlertEvent::ReplyByFallback { ref thread } => {
                stats.messages_processed += 1;
                stats.replies_by_fallback += 1;
                stats.per_thread.entry(thread.clone()).or_default().processed += 1;
            }
            AlertEvent::ProcessingError { ref thread, error: _ } => {
                stats.errors += 1;
                stats.per_thread.entry(thread.clone()).or_default().errors += 1;
            }
            AlertEvent::QueueDropped { .. } => {
                stats.dropped += 1;
            }
        }
    }

    async fn flush_errors(&self, buffer: &mut Vec<ErrorEntry>) {
        let errors: Vec<ErrorEntry> = buffer.drain(..).collect();
        if errors.is_empty() {
            return;
        }

        let prefix = self.config.subject_prefix.as_deref().unwrap_or("JYC Alert");
        let subject = format!("{prefix}: {} error(s)", errors.len());

        let mut body = format!(
            "**JYC Error Digest**\n\n{} error(s) in the last {} minute(s):\n\n",
            errors.len(), self.config.batch_interval_minutes
        );

        for (i, error) in errors.iter().enumerate() {
            body.push_str(&format!("---\n### Error {} — {}\n\n", i + 1, error.timestamp));
            if let Some(ref thread) = error.thread {
                body.push_str(&format!("**Thread:** {thread}\n\n"));
            }
            body.push_str(&format!("**Message:** {}\n\n", error.message));
            if let Some(ref ctx) = error.context {
                body.push_str(&format!("**Context:**\n```\n{ctx}\n```\n\n"));
            }
        }

        if let Err(e) = self.outbound.send_alert(&self.config.recipient, &subject, &body).await {
            eprintln!("[AlertService] Failed to send error digest: {e}");
        }
    }

    async fn send_health_report(&self, stats: &mut HealthStats) {
        let prefix = self.config.subject_prefix.as_deref().unwrap_or("JYC Alert");
        let status = if stats.errors > 0 { "DEGRADED" } else { "OK" };
        let subject = format!(
            "{prefix} Health: {status} | {} processed, {} errors",
            stats.messages_processed, stats.errors
        );

        let period_start = stats.period_start.as_deref().unwrap_or("unknown");

        let mut body = format!(
            "**JYC Health Check Report**\n\n\
             Period: {} — {}\n\
             Status: **{status}**\n\n\
             ## Summary\n\n\
             | Metric | Count |\n|--------|-------|\n\
             | Messages received | {} |\n\
             | Messages matched | {} |\n\
             | Messages processed | {} |\n\
             | Replies by MCP tool | {} |\n\
             | Replies by fallback | {} |\n\
             | Errors | {} |\n\
             | Dropped (queue full) | {} |\n",
            period_start, chrono::Utc::now().to_rfc3339(),
            stats.messages_received, stats.messages_matched,
            stats.messages_processed, stats.replies_by_tool,
            stats.replies_by_fallback, stats.errors, stats.dropped,
        );

        if !stats.per_thread.is_empty() {
            body.push_str("\n## Per-Thread Activity\n\n");
            body.push_str("| Thread | Received | Processed | Errors |\n|--------|----------|-----------|--------|\n");
            for (thread, ts) in &stats.per_thread {
                body.push_str(&format!("| {} | {} | {} | {} |\n", thread, ts.received, ts.processed, ts.errors));
            }
        }

        let recipient = self.config.health_check.as_ref()
            .and_then(|h| h.recipient.as_deref())
            .unwrap_or(&self.config.recipient);

        if let Err(e) = self.outbound.send_alert(recipient, &subject, &body).await {
            eprintln!("[AlertService] Failed to send health report: {e}");
        }

        *stats = HealthStats {
            period_start: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };
    }
}
