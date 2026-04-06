use anyhow::Result;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::channels::email::inbound::{self, EmailMatcher};
use crate::channels::types::ChannelPattern;
use crate::config::types::{ImapConfig, MonitorConfig};
use crate::core::message_router::MessageRouter;
use crate::core::state_manager::StateManager;
use crate::services::imap::client::ImapClient;

/// IMAP email monitor — connects to IMAP, fetches new emails, dispatches them.
///
/// Supports two modes:
/// - **IDLE**: Server push — blocks until new mail arrives (recommended)
/// - **Poll**: Periodic check at a configured interval
///
/// Includes recovery mode for message deletions and suspicious jumps.
pub struct ImapMonitor {
    channel_name: String,
    imap_config: ImapConfig,
    monitor_config: MonitorConfig,
    patterns: Vec<ChannelPattern>,
    router: Arc<MessageRouter>,
    state_manager: StateManager,
    cancel: CancellationToken,
}

impl ImapMonitor {
    pub fn new(
        channel_name: String,
        imap_config: ImapConfig,
        monitor_config: MonitorConfig,
        patterns: Vec<ChannelPattern>,
        router: Arc<MessageRouter>,
        state_manager: StateManager,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            channel_name,
            imap_config,
            monitor_config,
            patterns,
            router,
            state_manager,
            cancel,
        }
    }

    /// Start the monitoring loop.
    pub async fn start(&mut self) -> Result<()> {
        let mut client = ImapClient::new(self.imap_config.clone());
        let mut reconnect_attempts = 0u32;
        let max_retries = self.monitor_config.max_retries;
        let use_idle = self.monitor_config.mode == "idle";
        let poll_interval = self.monitor_config.poll_interval_secs;
        let folder = &self.monitor_config.folder.clone();

        tracing::info!(
            mode = if use_idle { "IDLE" } else { "poll" },
            folder = %folder,
            "Starting IMAP monitor"
        );

        loop {
            if self.cancel.is_cancelled() {
                break;
            }

            // Connect if needed
            if !client.is_connected() {
                match client.connect().await {
                    Ok(()) => {
                        reconnect_attempts = 0;
                    }
                    Err(e) => {
                        reconnect_attempts += 1;
                        if reconnect_attempts as usize > max_retries {
                            // Don't give up — cap the counter and keep retrying at max backoff.
                            // The monitor must survive extended outages (server maintenance,
                            // network partitions) and recover automatically.
                            tracing::error!(
                                error = %e,
                                attempts = reconnect_attempts,
                                "IMAP connect failed after {max_retries} retries, will keep retrying at max backoff"
                            );
                            reconnect_attempts = max_retries as u32;
                        }
                        let delay = backoff_delay(reconnect_attempts);
                        tracing::warn!(
                            error = %e,
                            attempt = reconnect_attempts,
                            delay_secs = delay.as_secs(),
                            "IMAP connect failed, retrying..."
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => continue,
                            _ = self.cancel.cancelled() => break,
                        }
                    }
                }
            }

            // Select mailbox
            let current_count = match client.select(folder).await {
                Ok(count) => count,
                Err(e) => {
                    reconnect_attempts += 1;
                    tracing::error!(
                        error = %e,
                        attempt = reconnect_attempts,
                        "Failed to select mailbox"
                    );
                    client.disconnect().await.ok();
                    if reconnect_attempts as usize > max_retries {
                        // Don't give up — cap the counter and keep retrying at max backoff.
                        tracing::error!(
                            "IMAP select failed after {max_retries} retries, will keep retrying at max backoff"
                        );
                        reconnect_attempts = max_retries as u32;
                    }
                    let delay = backoff_delay(reconnect_attempts);
                    tracing::warn!(
                        delay_secs = delay.as_secs(),
                        "Retrying after backoff..."
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => continue,
                        _ = self.cancel.cancelled() => break,
                    }
                }
            };

            // Check for new messages
            if let Err(e) = self
                .check_for_new(&mut client, current_count, folder)
                .await
            {
                tracing::error!(error = %e, "Error checking for new messages, forcing disconnect");
                // Force disconnect so the next iteration reconnects cleanly
                // instead of entering IDLE on a potentially dead connection.
                client.disconnect().await.ok();
                continue;
            }

            // Wait for next check
            if self.cancel.is_cancelled() {
                break;
            }

            if use_idle && client.is_connected() {
                tracing::debug!("Entering IDLE mode");
                // Wrap IDLE in a hard timeout to guard against half-open TCP connections.
                // The IMAP-level timeout (29 min) may not fire if the TCP socket is dead.
                // Use 2 min to detect dead connections quickly while allowing normal IDLE
                // to return on new mail. The monitor re-enters IDLE on the next loop iteration.
                let idle_timeout = std::time::Duration::from_secs(2 * 60); // 2 min hard limit
                tokio::select! {
                    result = tokio::time::timeout(idle_timeout, client.idle()) => {
                        match result {
                            Ok(Ok(())) => {} // IDLE returned normally (new mail or IMAP timeout)
                            Ok(Err(e)) => {
                                tracing::warn!(error = %e, "IDLE error, reconnecting");
                                client.disconnect().await.ok();
                            }
                            Err(_) => {
                                tracing::warn!("IDLE hard timeout (2 min), connection likely dead, reconnecting");
                                client.disconnect().await.ok();
                            }
                        }
                    }
                    _ = self.cancel.cancelled() => break,
                }
            } else {
                tracing::trace!(interval = poll_interval, "Polling, sleeping...");
                tokio::select! {
                    _ = tokio::time::sleep(
                        std::time::Duration::from_secs(poll_interval)
                    ) => {}
                    _ = self.cancel.cancelled() => break,
                }
            }
        }

        // Cleanup
        client.disconnect().await.ok();
        self.state_manager.save().await.ok();
        tracing::info!("IMAP monitor stopped");
        Ok(())
    }

    /// Check for new messages since last known sequence number.
    async fn check_for_new(
        &mut self,
        client: &mut ImapClient,
        current_count: u32,
        _folder: &str,
    ) -> Result<()> {
        let last_seq = self.state_manager.last_sequence_number();

        if current_count == 0 {
            tracing::debug!("Mailbox empty");
            return Ok(());
        }

        if current_count == last_seq {
            tracing::trace!(count = current_count, "No new messages");
            return Ok(());
        }

        if current_count < last_seq {
            // Messages were deleted — recovery needed
            tracing::warn!(
                current = current_count,
                last = last_seq,
                "Message count decreased, possible deletion"
            );
            self.state_manager.update_sequence(current_count, None);
            self.state_manager.save().await?;
            return Ok(());
        }

        // Suspicious jump check
        let jump = current_count - last_seq;
        if jump > 50 {
            tracing::warn!(
                jump = jump,
                "Large sequence jump detected ({}→{})",
                last_seq,
                current_count
            );
        }

        // Fetch new messages (from last_seq+1 to current_count)
        let from = if last_seq == 0 {
            // First run — only process the latest message (don't flood)
            current_count
        } else {
            last_seq + 1
        };

        tracing::info!(
            from = from,
            to = current_count,
            "Fetching new messages"
        );

        let emails = client.fetch_range(from, current_count).await?;

        for email in &emails {
            if self.cancel.is_cancelled() {
                break;
            }

            if self.state_manager.is_processed(email.uid) {
                tracing::debug!(uid = email.uid, "Already processed, skipping");
                continue;
            }

            match self.process_email(email).await {
                Ok(()) => {
                    self.state_manager.track_uid(email.uid).await?;
                    tracing::debug!(uid = email.uid, seq = email.seq, "Email processed");
                }
                Err(e) => {
                    tracing::error!(
                        uid = email.uid,
                        error = %e,
                        "Failed to process email"
                    );
                }
            }
        }

        // Update sequence number
        self.state_manager
            .update_sequence(current_count, emails.last().map(|e| e.uid));
        self.state_manager.save().await?;

        Ok(())
    }

    /// Process a single fetched email.
    async fn process_email(
        &self,
        email: &crate::services::imap::client::FetchedEmail,
    ) -> Result<()> {
        let mut message = inbound::parse_raw_email(&email.body, email.uid)?;

        // Set channel to the config channel name (e.g., "jiny283"), not the type ("email")
        message.channel = self.channel_name.clone();

        tracing::info!(
            uid = email.uid,
            sender = %message.sender_address,
            topic = %message.topic,
            "Message received"
        );

        // Route through the message router (pattern match → thread queue)
        self.router.route(&EmailMatcher, message, &self.patterns).await;

        Ok(())
    }
}

/// Exponential backoff: base_delay * 2^(attempt-1), capped at 5 minutes.
fn backoff_delay(attempt: u32) -> std::time::Duration {
    let base = 5u64; // seconds
    let delay = base * 2u64.pow(attempt.saturating_sub(1));
    let capped = delay.min(300);
    std::time::Duration::from_secs(capped)
}
