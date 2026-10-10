use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::imap::client::{ImapClient, MailboxInfo};
use jyc_core::state_manager::StateManager;
use jyc_types::{ImapConfig, InboundMessage, MonitorConfig};

/// Callback invoked for every parsed inbound email.
///
/// Mirrors `jyc_types::InboundAdapterOptions::on_message`: the callback
/// returns immediately (spawning its own task for routing work).
pub type OnEmail = Box<dyn Fn(InboundMessage) -> Result<()> + Send + Sync>;

/// IMAP email monitor — connects to IMAP, fetches new emails, dispatches them.
///
/// Supports two modes:
/// - **IDLE**: Server push — blocks until new mail arrives (recommended)
/// - **Poll**: Periodic check at a configured interval
pub struct ImapMonitor {
    channel_name: String,
    imap_config: ImapConfig,
    monitor_config: MonitorConfig,
    state_manager: StateManager,
    cancel: CancellationToken,
    on_message: OnEmail,
}

impl ImapMonitor {
    pub fn new(
        channel_name: String,
        imap_config: ImapConfig,
        monitor_config: MonitorConfig,
        state_manager: StateManager,
        cancel: CancellationToken,
        on_message: OnEmail,
    ) -> Self {
        Self {
            channel_name,
            imap_config,
            monitor_config,
            state_manager,
            cancel,
            on_message,
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
            let mailbox = match client.select(folder).await {
                Ok(info) => info,
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
                    tracing::warn!(delay_secs = delay.as_secs(), "Retrying after backoff...");
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => continue,
                        _ = self.cancel.cancelled() => break,
                    }
                }
            };

            // Check for new messages
            if let Err(e) = self.check_for_new(&mut client, mailbox).await {
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
                tracing::debug!(interval = poll_interval, "Polling, sleeping...");
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

    /// Check for new messages since the last processed UID.
    ///
    /// Detection is UID-based. The message count is *not* a cursor: a mailbox
    /// that expunges one message and receives another reports the same count
    /// while the newest UID moved, so a count comparison silently misses mail.
    async fn check_for_new(&mut self, client: &mut ImapClient, mailbox: MailboxInfo) -> Result<()> {
        // A changed UIDVALIDITY voids every stored UID — both the cursor and
        // the processed-UID set describe the server's previous UID space.
        let stored_validity = self.state_manager.uid_validity();
        match validity_action(stored_validity, mailbox.uid_validity) {
            ValidityAction::Keep => {}
            ValidityAction::Record(server) => {
                // Persist the first sighting immediately: after a restart an
                // unrecorded epoch is indistinguishable from a changed one.
                self.state_manager.update_uid_validity(server);
                self.state_manager.save().await?;
            }
            ValidityAction::Reset(server) => {
                tracing::warn!(
                    stored = ?stored_validity,
                    server,
                    "UIDVALIDITY changed, resetting UID state"
                );
                self.state_manager.reset().await?;
                self.state_manager.update_uid_validity(server);
                self.state_manager.save().await?;
            }
        }

        // The newest UID comes from the mailbox itself: not every server
        // reports UIDNEXT (163 does not), and UIDNEXT is only a prediction
        // that need not move when the newest message is expunged.
        let newest_uid = if mailbox.exists == 0 {
            None
        } else {
            client.newest_uid().await?
        };
        tracing::debug!(
            exists = mailbox.exists,
            uid_next = ?mailbox.uid_next,
            newest_uid = ?newest_uid,
            "Using the mailbox's newest UID"
        );

        let last_uid = self.state_manager.last_processed_uid();
        let (from, to) = match plan_fetch(last_uid, newest_uid) {
            FetchPlan::Nothing => {
                tracing::debug!(
                    exists = mailbox.exists,
                    newest_uid = ?newest_uid,
                    last_uid = ?last_uid,
                    "No new messages"
                );
                return Ok(());
            }
            FetchPlan::CursorAhead => {
                // With a known UIDVALIDITY a newest UID below the cursor only
                // means the newest message was expunged; a recreated mailbox
                // is the likely story only while the epoch is unknown.
                if self.state_manager.uid_validity().is_some() {
                    tracing::debug!(
                        last_uid = ?last_uid,
                        newest_uid = ?newest_uid,
                        "Newest UID is below the cursor, the newest message was deleted"
                    );
                } else {
                    tracing::warn!(
                        last_uid = ?last_uid,
                        newest_uid = ?newest_uid,
                        "Newest UID is below the cursor and UIDVALIDITY is unknown, the \
                         mailbox may have been recreated; run with --reset to reprocess it"
                    );
                }
                return Ok(());
            }
            FetchPlan::Uids(from, to) => (from, to),
        };

        if to - from + 1 > 50 {
            tracing::warn!(
                from_uid = from,
                to_uid = to,
                "Large UID range, fetching many messages"
            );
        }

        tracing::info!(from_uid = from, to_uid = to, "Fetching new messages");

        let emails = client.fetch_uid_range(from, to).await?;

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

        // Move the cursor past everything just fetched: a message that failed
        // to process is not retried (reprocessing needs --reset). The message
        // count is kept in the state file for observation only.
        self.state_manager.update_sequence(mailbox.exists, Some(to));
        self.state_manager.save().await?;

        Ok(())
    }

    /// Process a single fetched email.
    async fn process_email(&self, email: &crate::imap::client::FetchedEmail) -> Result<()> {
        let mut message = crate::imap::parse_email::parse_raw_email(&email.body, email.uid)?;

        // Set channel to the config channel name (e.g., "jiny283"), not the type ("email")
        message.channel = self.channel_name.clone();

        tracing::info!(
            uid = email.uid,
            sender = %message.sender_address,
            topic = %message.topic,
            "Message received"
        );

        // Note: attachment bytes travel with the message; they are saved by
        // the topic manager of whichever channel the callback routes into.

        // Hand off to the adapter's callback (pattern match → pipe → route)
        (self.on_message)(message)?;

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

/// What the monitor should fetch after reading the mailbox's newest UID.
#[derive(Debug, PartialEq)]
enum FetchPlan {
    /// Empty mailbox, or nothing new since the stored cursor.
    Nothing,
    /// The newest UID is below the stored cursor — the mailbox most likely
    /// restarted its UID space while the stored UIDVALIDITY was unknown.
    CursorAhead,
    /// Inclusive UID range to fetch.
    Uids(u32, u32),
}

/// Decide what to fetch, given the stored cursor and the mailbox's newest UID.
fn plan_fetch(last_uid: Option<u32>, newest_uid: Option<u32>) -> FetchPlan {
    // `None` for an empty mailbox; `Some(0)` for a server that answered
    // without a UID.
    let Some(newest) = newest_uid.filter(|uid| *uid > 0) else {
        return FetchPlan::Nothing;
    };

    match last_uid {
        // First run — only the newest message, don't replay mailbox history.
        None => FetchPlan::Uids(newest, newest),
        Some(last) if newest > last => FetchPlan::Uids(last + 1, newest),
        Some(last) if newest == last => FetchPlan::Nothing,
        Some(_) => FetchPlan::CursorAhead,
    }
}

/// What to do with the stored UID state, given the server's UIDVALIDITY.
#[derive(Debug, PartialEq)]
enum ValidityAction {
    /// Same epoch (or the server reported none): keep the stored UIDs.
    Keep,
    /// First sighting of the epoch: remember it, keep the stored UIDs.
    Record(u32),
    /// The epoch changed: every stored UID describes the previous mailbox.
    Reset(u32),
}

/// Compare the stored UIDVALIDITY against the server's.
fn validity_action(stored: Option<u32>, server: Option<u32>) -> ValidityAction {
    match (stored, server) {
        (Some(stored), Some(server)) if stored != server => ValidityAction::Reset(server),
        (None, Some(server)) => ValidityAction::Record(server),
        _ => ValidityAction::Keep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_fetches_only_the_newest() {
        assert_eq!(plan_fetch(None, Some(100)), FetchPlan::Uids(100, 100));
    }

    #[test]
    fn new_message_after_cursor_is_fetched() {
        assert_eq!(plan_fetch(Some(100), Some(103)), FetchPlan::Uids(101, 103));
    }

    #[test]
    fn same_message_count_but_new_uid_is_detected() {
        // Regression: a mailbox that expunged one message and received
        // another still reports `1 EXISTS` — the count is unchanged while the
        // newest UID moved past the cursor.
        assert_eq!(
            plan_fetch(Some(1_773_715_637), Some(1_773_715_638)),
            FetchPlan::Uids(1_773_715_638, 1_773_715_638)
        );
    }

    #[test]
    fn nothing_when_newest_equals_cursor() {
        assert_eq!(plan_fetch(Some(100), Some(100)), FetchPlan::Nothing);
    }

    #[test]
    fn nothing_for_empty_or_uidless_mailbox() {
        assert_eq!(plan_fetch(None, None), FetchPlan::Nothing);
        assert_eq!(plan_fetch(Some(100), None), FetchPlan::Nothing);
        assert_eq!(plan_fetch(Some(100), Some(0)), FetchPlan::Nothing);
    }

    #[test]
    fn cursor_ahead_of_newest_uid_is_flagged() {
        // The mailbox was recreated (UIDs restarted) while the stored
        // UIDVALIDITY was unknown — reprocessing needs an explicit reset.
        assert_eq!(plan_fetch(Some(100), Some(50)), FetchPlan::CursorAhead);
    }

    #[test]
    fn first_sighting_of_uidvalidity_is_recorded() {
        assert_eq!(
            validity_action(None, Some(1_773_715_637)),
            ValidityAction::Record(1_773_715_637)
        );
    }

    #[test]
    fn unchanged_uidvalidity_keeps_stored_uids() {
        assert_eq!(validity_action(Some(42), Some(42)), ValidityAction::Keep);
        // A server that stops reporting UIDVALIDITY must not reset anything.
        assert_eq!(validity_action(Some(42), None), ValidityAction::Keep);
    }

    #[test]
    fn changed_uidvalidity_resets_stored_uids() {
        assert_eq!(
            validity_action(Some(42), Some(43)),
            ValidityAction::Reset(43)
        );
    }
}
