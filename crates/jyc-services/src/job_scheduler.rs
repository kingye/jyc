//! Background JobScheduler — fires due jobs by injecting InboundMessage
//! into the originating thread via ThreadManager.
//!
//! The scheduler runs as a background async task. It periodically scans
//! the job store for due jobs, fires them, and then sleeps until the
//! next job is due (or until the scan interval elapses, whichever is sooner).

use anyhow::Result;
use chrono::Utc;
use jyc_core::job_store::JobStore;
use jyc_core::thread_manager::ThreadManager;
use jyc_types::{InboundMessage, MessageContent, PatternMatch};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Background job scheduler that fires due jobs.
///
/// Runs as a single async task alongside the per-channel inbound monitors.
/// When a job fires, it creates an `InboundMessage` and enqueues it into
/// the originating thread via `ThreadManager::enqueue`.
pub struct JobScheduler {
    /// Job store for reading/updating job files.
    store: JobStore,

    /// Thread managers indexed by channel name.
    /// The scheduler looks up the correct TM when firing a job.
    thread_managers: Arc<Mutex<HashMap<String, Arc<ThreadManager>>>>,

    /// Scan interval in seconds (from config).
    scan_interval: std::time::Duration,

    /// Whether the scheduler is enabled.
    enabled: bool,
}

impl JobScheduler {
    /// Create a new JobScheduler.
    pub fn new(
        store: JobStore,
        thread_managers: Arc<Mutex<HashMap<String, Arc<ThreadManager>>>>,
        scan_interval_secs: u64,
        enabled: bool,
    ) -> Self {
        Self {
            store,
            thread_managers,
            scan_interval: std::time::Duration::from_secs(scan_interval_secs),
            enabled,
        }
    }

    /// Start the scheduler loop. Runs until the cancellation token is triggered.
    ///
    /// This is the main entry point — spawn it as a background task in the monitor.
    pub async fn run(&self, cancel: CancellationToken) {
        if !self.enabled {
            tracing::info!("Job scheduler is disabled");
            return;
        }

        tracing::info!(
            scan_interval_secs = self.scan_interval.as_secs(),
            "Job scheduler started"
        );

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("Job scheduler cancelled");
                    break;
                }
                _ = self.run_cycle() => {
                    // After running a cycle, sleep until the next
                    // job is due or the scan interval elapses.
                    let sleep_dur = self.next_sleep_duration().await;
                    tokio::select! {
                        _ = tokio::time::sleep(sleep_dur) => {}
                        _ = cancel.cancelled() => {
                            tracing::info!("Job scheduler cancelled during sleep");
                            break;
                        }
                    }
                }
            }
        }

        tracing::info!("Job scheduler stopped");
    }

    /// Run a single scan-and-fire cycle.
    ///
    /// Lists all enabled jobs, checks if any are due (next_fire_at <= now),
    /// fires them, and updates their state.
    async fn run_cycle(&self) {
        let jobs = match self.store.list().await {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::error!(error = %e, "Failed to list jobs during scan");
                return;
            }
        };

        let now = Utc::now();
        let due_jobs: Vec<_> = jobs
            .into_iter()
            .filter(|j| j.enabled && j.next_fire_at.is_some_and(|t| t <= now))
            .collect();

        if due_jobs.is_empty() {
            tracing::trace!("No due jobs found");
            return;
        }

        tracing::info!(count = due_jobs.len(), "Firing due jobs");

        for mut job in due_jobs {
            // Mark as fired (updates last_fired_at and advances next_fire_at)
            job.mark_fired();

            // Save updated state before firing
            if let Err(e) = self.store.update(&job).await {
                tracing::error!(job_id = %job.id, error = %e, "Failed to update job after marking fired");
                // Continue to next job — the job will be re-fired on next scan
                continue;
            }

            // Fire the job: inject InboundMessage into the thread
            if let Err(e) = self.fire_job(&job).await {
                tracing::error!(job_id = %job.id, error = %e, "Failed to fire job");
            }
        }
    }

    /// Fire a single job by injecting an InboundMessage into the originating thread.
    async fn fire_job(&self, job: &jyc_types::JobConfig) -> Result<()> {
        let tms = self.thread_managers.lock().await;
        let tm = tms
            .get(&job.channel_name)
            .ok_or_else(|| anyhow::anyhow!("thread manager not found for channel '{}'", job.channel_name))?;

        let message = InboundMessage {
            id: uuid::Uuid::new_v4().to_string(),
            channel: job.channel.clone(),
            channel_uid: format!("job-{}", job.id),
            sender: "scheduler".to_string(),
            sender_address: "scheduler@jyc".to_string(),
            recipients: vec![],
            topic: format!("Scheduled job: {}", job.prompt.chars().take(80).collect::<String>()),
            content: MessageContent {
                text: Some(job.prompt.clone()),
                html: None,
                markdown: None,
            },
            timestamp: Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("job_id".to_string(), serde_json::Value::String(job.id.clone()));
                m
            },
            matched_pattern: None,
        };

        let pattern_match = PatternMatch {
            pattern_name: String::new(),
            channel: job.channel.clone(),
            matches: std::collections::HashMap::new(),
        };

        tm.enqueue(message, job.thread_name.clone(), pattern_match, None, true)
            .await;

        tracing::info!(
            job_id = %job.id,
            thread = %job.thread_name,
            channel = %job.channel_name,
            "Job fired"
        );

        Ok(())
    }

    /// Compute how long to sleep until the next job is due.
    ///
    /// Returns the scan interval if no jobs are enabled, or the time
    /// until the next due job (capped at the scan interval).
    async fn next_sleep_duration(&self) -> std::time::Duration {
        let jobs = match self.store.list().await {
            Ok(jobs) => jobs,
            Err(_) => return self.scan_interval,
        };

        let now = Utc::now();
        let next_fire = jobs
            .iter()
            .filter(|j| j.enabled)
            .filter_map(|j| j.next_fire_at)
            .min();

        match next_fire {
            Some(t) if t > now => {
                let duration = (t - now).to_std().unwrap_or(self.scan_interval);
                duration.min(self.scan_interval)
            }
            _ => self.scan_interval,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_core::job_store::JobStore;
    use tempfile::tempdir;

    async fn create_test_scheduler(
        store: JobStore,
        enabled: bool,
    ) -> JobScheduler {
        let tms = Arc::new(Mutex::new(HashMap::new()));
        JobScheduler::new(store, tms, 60, enabled)
    }

    #[tokio::test]
    async fn test_disabled_scheduler_returns_immediately() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path()).await.unwrap();
        let scheduler = create_test_scheduler(store, false).await;
        let cancel = CancellationToken::new();

        // Run should return immediately when disabled
        tokio::time::timeout(std::time::Duration::from_millis(100), scheduler.run(cancel))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_next_sleep_duration_no_jobs() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path()).await.unwrap();
        let scheduler = create_test_scheduler(store, true).await;

        let dur = scheduler.next_sleep_duration().await;
        assert_eq!(dur, std::time::Duration::from_secs(60));
    }

    #[tokio::test]
    async fn test_next_sleep_duration_with_future_job() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path()).await.unwrap();

        // Create a job that fires far in the future
        let job = jyc_types::JobConfig::new_one_time(
            Utc::now() + chrono::Duration::hours(1),
            "test".to_string(),
            "email".to_string(),
            "work".to_string(),
            "future task".to_string(),
        );
        store.create(&job).await.unwrap();

        let scheduler = create_test_scheduler(store, true).await;
        let dur = scheduler.next_sleep_duration().await;

        // Should be less than the 60s scan interval since the job is 1 hour away
        // but capped at the scan interval
        assert_eq!(dur, std::time::Duration::from_secs(60));
    }

    #[tokio::test]
    async fn test_next_sleep_duration_with_past_job() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path()).await.unwrap();

        // Create a job that should have already fired
        let job = jyc_types::JobConfig::new_one_time(
            Utc::now() - chrono::Duration::minutes(10),
            "test".to_string(),
            "email".to_string(),
            "work".to_string(),
            "overdue task".to_string(),
        );
        store.create(&job).await.unwrap();

        let scheduler = create_test_scheduler(store, true).await;
        let dur = scheduler.next_sleep_duration().await;

        // Should return scan interval because the job is already due
        // (next_fire_at is in the past, so the min function yields None
        // for the filter condition, falling back to scan_interval)
        assert_eq!(dur, std::time::Duration::from_secs(60));
    }

    #[tokio::test]
    async fn test_run_cycle_fires_due_job() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path()).await.unwrap();

        // Create a job that's due now
        let job = jyc_types::JobConfig::new_one_time(
            Utc::now(),
            "test-thread".to_string(),
            "email".to_string(),
            "nonexistent-channel".to_string(),
            "test fire".to_string(),
        );
        store.create(&job).await.unwrap();

        let scheduler = create_test_scheduler(store, true).await;
        scheduler.run_cycle().await;

        // The job should have been marked as fired (enabled=false, last_fired_at set)
        // even though firing failed (channel manager not found)
        let store = &scheduler.store;
        let updated = store.get(&job.id).await.unwrap().unwrap();
        assert!(!updated.enabled);
        assert!(updated.last_fired_at.is_some());
    }

    // --- Integration tests ---

    /// Full lifecycle test: create a one-time job → scheduler fires it → verify state.
    #[tokio::test]
    async fn test_job_lifecycle_one_time() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path()).await.unwrap();

        // Create a one-time job that fires immediately
        let job = jyc_types::JobConfig::new_one_time(
            Utc::now(),
            "lifecycle-test".to_string(),
            "email".to_string(),
            "test-channel".to_string(),
            "Integration test job".to_string(),
        );
        let job_id = job.id.clone();
        store.create(&job).await.unwrap();

        // Verify job exists in store
        let stored = store.get(&job_id).await.unwrap().unwrap();
        assert!(stored.enabled);
        assert!(stored.next_fire_at.is_some());
        assert!(stored.last_fired_at.is_none());

        // Run the scheduler cycle
        let scheduler = create_test_scheduler(store.clone(), true).await;
        scheduler.run_cycle().await;

        // Verify job was fired and disabled
        let updated = store.get(&job_id).await.unwrap().unwrap();
        assert!(!updated.enabled, "One-time job should be disabled after firing");
        assert!(updated.last_fired_at.is_some(), "Job should have last_fired_at");
        assert!(updated.next_fire_at.is_none(), "One-time job should have no next fire after firing");
    }

    /// Full lifecycle test: create a recurring job → manually fire it → verify state.
    #[tokio::test]
    async fn test_job_lifecycle_recurring() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path()).await.unwrap();

        // Create a recurring job
        let mut job = jyc_types::JobConfig::new_recurring(
            "*/30 * * * * * *",
            "recurring-test".to_string(),
            "email".to_string(),
            "test-channel".to_string(),
            "Recurring integration test".to_string(),
        );
        let job_id = job.id.clone();
        let original_next = job.next_fire_at;
        store.create(&job).await.unwrap();

        // Manually fire the job (simulates what the scheduler does)
        job.mark_fired();
        store.update(&job).await.unwrap();

        // Verify recurring job stays enabled and has a new next_fire_at
        let updated = store.get(&job_id).await.unwrap().unwrap();
        assert!(updated.enabled, "Recurring job should stay enabled after firing");
        assert!(updated.last_fired_at.is_some(), "Job should have last_fired_at");
        assert!(updated.next_fire_at.is_some(), "Recurring job should have next fire");
        // Next fire must be >= original next fire
        assert!(updated.next_fire_at >= original_next);
    }

    /// Test that the scheduler only fires enabled jobs.
    #[tokio::test]
    async fn test_scheduler_skips_disabled_jobs() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path()).await.unwrap();

        // Create a disabled job that is due
        let mut job = jyc_types::JobConfig::new_one_time(
            Utc::now(),
            "disabled-test".to_string(),
            "email".to_string(),
            "test-channel".to_string(),
            "Should not fire".to_string(),
        );
        job.enabled = false;
        store.create(&job).await.unwrap();

        // Run the scheduler cycle
        let scheduler = create_test_scheduler(store.clone(), true).await;
        scheduler.run_cycle().await;

        // Verify disabled job was NOT touched
        let stored = store.get(&job.id).await.unwrap().unwrap();
        assert!(!stored.enabled);
        assert!(stored.last_fired_at.is_none(), "Disabled job should not have been fired");
    }

    /// Test job store CRUD operations in sequence.
    #[tokio::test]
    async fn test_job_crud_workflow() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path()).await.unwrap();

        // Create
        let job = jyc_types::JobConfig::new_recurring(
            "0 0 9 * * * *",
            "crud-test".to_string(),
            "email".to_string(),
            "work".to_string(),
            "Daily at 9 AM".to_string(),
        );
        store.create(&job).await.unwrap();
        assert!(store.get(&job.id).await.unwrap().is_some());

        // List
        let jobs = store.list().await.unwrap();
        assert_eq!(jobs.len(), 1);

        // Update
        let mut updated = job.clone();
        updated.prompt = "Updated prompt".to_string();
        store.update(&updated).await.unwrap();
        let fetched = store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(fetched.prompt, "Updated prompt");

        // Delete
        let deleted = store.delete(&job.id).await.unwrap();
        assert!(deleted);
        assert!(store.get(&job.id).await.unwrap().is_none());

        // List empty
        let jobs = store.list().await.unwrap();
        assert!(jobs.is_empty());
    }

    /// Test job creation through the config interface (new_one_time, new_recurring).
    #[tokio::test]
    async fn test_job_creation_constructors() {
        let future = Utc::now() + chrono::Duration::days(1);

        // One-time
        let one_time = jyc_types::JobConfig::new_one_time(
            future,
            "thread-1".to_string(),
            "wecom_bot".to_string(),
            "general".to_string(),
            "Reminder text".to_string(),
        );
        assert_eq!(one_time.at, Some(future));
        assert!(one_time.cron.is_none());

        // Recurring
        let recurring = jyc_types::JobConfig::new_recurring(
            "0 0 8 * * * *",
            "thread-2".to_string(),
            "email".to_string(),
            "work".to_string(),
            "Daily summary".to_string(),
        );
        assert_eq!(recurring.cron.as_deref(), Some("0 0 8 * * * *"));
        assert!(recurring.at.is_none());
    }

    /// Test that mark_fired correctly advances the recurring job's next_fire_at.
    #[tokio::test]
    async fn test_mark_fired_advances_recurring() {
        let mut job = jyc_types::JobConfig::new_recurring(
            "* * * * * * *",  // Every second
            "advance-test".to_string(),
            "email".to_string(),
            "work".to_string(),
            "Every second".to_string(),
        );

        let before = job.next_fire_at;
        // Ensure time passes so mark_fired's now > creation now
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        job.mark_fired();
        let after = job.next_fire_at;

        assert!(job.enabled, "Recurring job should stay enabled");
        assert!(job.last_fired_at.is_some(), "Should have last_fired_at");
        assert!(after >= before, "Next fire should not go backwards");
    }

    /// Test that multiple due jobs are all fired in one cycle.
    #[tokio::test]
    async fn test_run_cycle_fires_multiple_jobs() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path()).await.unwrap();

        // Create 3 jobs that are due now
        for i in 0..3 {
            let job = jyc_types::JobConfig::new_one_time(
                Utc::now(),
                format!("thread-{i}"),
                "email".to_string(),
                "test-channel".to_string(),
                format!("Job {i}"),
            );
            store.create(&job).await.unwrap();
        }

        let scheduler = create_test_scheduler(store.clone(), true).await;
        scheduler.run_cycle().await;

        // All 3 jobs should be fired
        let jobs = store.list().await.unwrap();
        assert_eq!(jobs.len(), 3);
        for job in &jobs {
            assert!(!job.enabled, "One-time job should be disabled after firing");
            assert!(job.last_fired_at.is_some(), "Job should have been fired");
        }
    }
}
