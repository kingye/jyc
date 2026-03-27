use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::channels::email::outbound::EmailOutboundAdapter;
use crate::channels::types::InboundMessage;
use crate::config::types::ProgressConfig;

/// Progress tracker — sends periodic "still working" emails during long AI operations.
///
/// Timing:
/// - First update after `initial_delay_secs` (default: 180s / 3 min)
/// - Subsequent updates every `interval_secs` (default: 180s / 3 min)
/// - At most `max_messages` updates total (default: 5)
///
/// The tracker doesn't know about email/SMTP — it calls `outbound.send_progress_update()`.
pub struct ProgressTracker {
    config: ProgressConfig,
    outbound: Arc<EmailOutboundAdapter>,
    original_message: InboundMessage,
    start_time: Instant,
    sent_count: u32,
    last_update: Option<Instant>,
    activity: Arc<Mutex<String>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl ProgressTracker {
    pub fn new(
        config: ProgressConfig,
        outbound: Arc<EmailOutboundAdapter>,
        original_message: InboundMessage,
    ) -> Self {
        Self {
            config,
            outbound,
            original_message,
            start_time: Instant::now(),
            sent_count: 0,
            last_update: None,
            activity: Arc::new(Mutex::new("processing".to_string())),
            handle: None,
        }
    }

    /// Start the progress tracking background task.
    pub fn start(&mut self) {
        if !self.config.enabled {
            return;
        }

        let config = self.config.clone();
        let outbound = self.outbound.clone();
        let message = self.original_message.clone();
        let activity = self.activity.clone();
        let start_time = self.start_time;

        let handle = tokio::spawn(async move {
            let check_interval = Duration::from_secs(5);
            let initial_delay = Duration::from_secs(config.initial_delay_secs);
            let update_interval = Duration::from_secs(config.interval_secs);
            let max_messages = config.max_messages as u32;

            let mut sent_count = 0u32;
            let mut last_update: Option<Instant> = None;
            let mut tick = tokio::time::interval(check_interval);

            loop {
                tick.tick().await;

                if sent_count >= max_messages {
                    break;
                }

                let elapsed = start_time.elapsed();
                let should_send = if sent_count == 0 {
                    elapsed >= initial_delay
                } else {
                    last_update
                        .map(|lu| lu.elapsed() >= update_interval)
                        .unwrap_or(true)
                };

                if should_send {
                    let current_activity = activity.lock().await.clone();
                    let elapsed_ms = elapsed.as_millis() as u64;

                    tracing::info!(
                        elapsed_secs = elapsed.as_secs(),
                        count = sent_count + 1,
                        max = max_messages,
                        "Sending progress update"
                    );

                    if let Err(e) = outbound
                        .send_progress_update(&message, elapsed_ms, &current_activity)
                        .await
                    {
                        tracing::warn!(error = %e, "Failed to send progress update");
                    }

                    sent_count += 1;
                    last_update = Some(Instant::now());
                }
            }
        });

        self.handle = Some(handle);
    }

    /// Update the current activity label.
    pub async fn update_activity(&self, new_activity: &str) {
        let mut activity = self.activity.lock().await;
        *activity = new_activity.to_string();
    }

    /// Stop the progress tracker.
    pub fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl Drop for ProgressTracker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_config_defaults() {
        let config = ProgressConfig {
            enabled: true,
            initial_delay_secs: 180,
            interval_secs: 180,
            max_messages: 5,
        };
        assert_eq!(config.initial_delay_secs, 180);
        assert_eq!(config.max_messages, 5);
    }
}
