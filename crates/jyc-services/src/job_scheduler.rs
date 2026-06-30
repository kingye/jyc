//! Background JobScheduler — fires due jobs by injecting InboundMessage
//! into the originating thread via ThreadManager.
//!
//! Jobs are stored per-thread in `<thread>/.jyc/jobs/<id>.json`. The scheduler
//! scans all channel workspace directories for thread directories that contain
//! a `.jyc/jobs/` subdirectory, discovers due jobs, fires them, and updates
//! their state.

use anyhow::Result;
use chrono::Utc;
use jyc_core::job_store::JobStore;
use jyc_core::thread_manager::ThreadManager;
use jyc_types::{InboundMessage, MessageContent, PatternMatch};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// A due job discovered during a scan cycle, scoped to its thread.
struct DueJob {
    job: jyc_types::JobConfig,
    store: JobStore,
    thread_name: String,
    channel_name: String,
}

/// Background job scheduler that fires due jobs.
///
/// Runs as a single async task alongside the per-channel inbound monitors.
/// Scans workspace directories for threads with `.jyc/jobs/` subdirectories,
/// discovers due jobs, fires them via ThreadManager::enqueue, and persists
/// the updated state.
pub struct JobScheduler {
    /// Thread managers indexed by channel name.
    /// The scheduler looks up the correct TM when firing a job.
    thread_managers: Arc<Mutex<HashMap<String, Arc<ThreadManager>>>>,

    /// Workspace directories to scan for threads (one per channel).
    workspace_dirs: Vec<PathBuf>,

    /// Scan interval in seconds (from config).
    scan_interval: std::time::Duration,

    /// Maximum jobs per thread (from config).
    max_jobs_per_thread: usize,

    /// Whether the scheduler is enabled.
    enabled: bool,
}

impl JobScheduler {
    /// Create a new JobScheduler.
    pub fn new(
        thread_managers: Arc<Mutex<HashMap<String, Arc<ThreadManager>>>>,
        workspace_dirs: Vec<PathBuf>,
        scan_interval_secs: u64,
        max_jobs_per_thread: usize,
        enabled: bool,
    ) -> Self {
        Self {
            thread_managers,
            workspace_dirs,
            scan_interval: std::time::Duration::from_secs(scan_interval_secs),
            max_jobs_per_thread,
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
                    // job is due or the scan interval elapses, whichever is sooner.
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
    /// Scans all workspace directories for threads with `.jyc/jobs/`,
    /// discovers due (enabled + next_fire_at <= now) jobs, fires them,
    /// and updates their state.
    async fn run_cycle(&self) {
        let due = self.discover_due_jobs().await;
        if due.is_empty() {
            tracing::trace!("No due jobs found");
            return;
        }

        tracing::info!(count = due.len(), "Firing due jobs");

        for mut item in due {
            // Fire first: inject InboundMessage into the thread.
            // If this fails (e.g. ThreadManager not found), we do NOT
            // mark the job as fired — it will be retried on the next scan.
            if let Err(e) = self.fire_job(&item.job).await {
                tracing::error!(job_id = %item.job.id, error = %e, "Failed to fire job");
                continue;
            }

            // Only after successful enqueue, mark as fired and persist.
            // This prevents message loss: if fire_job fails, the job's
            // state remains unchanged so it re-fires later.
            item.job.mark_fired();
            if let Err(e) = item.store.update(&item.job).await {
                tracing::error!(
                    job_id = %item.job.id, error = %e,
                    "Job fired but failed to persist state — may re-fire on next scan"
                );
                continue;
            }

            tracing::info!(
                job_id = %item.job.id,
                thread = %item.thread_name,
                channel = %item.channel_name,
                "Job fired successfully"
            );
        }
    }

    /// Discover all due (enabled + next_fire_at <= now) jobs across all threads.
    async fn discover_due_jobs(&self) -> Vec<DueJob> {
        let now = Utc::now();
        let mut due = Vec::new();

        for workspace_dir in &self.workspace_dirs {
            let mut thread_entries = match tokio::fs::read_dir(workspace_dir).await {
                Ok(entries) => entries,
                Err(_) => continue,
            };

            while let Ok(Some(entry)) = thread_entries.next_entry().await {
                let thread_path = entry.path();
                if !thread_path.is_dir() {
                    continue;
                }

                let jobs_dir = thread_path.join(".jyc").join("jobs");
                if !jobs_dir.exists() {
                    continue;
                }

                let thread_name = match thread_path.file_name().and_then(|n| n.to_str()) {
                    Some(name) => name.to_string(),
                    None => continue,
                };

                // Extract channel_name from workspace directory structure:
                // <workdir>/<channel_name>/workspace/<thread_name>/
                let channel_name = match workspace_dir.parent() {
                    Some(p) => p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                    None => String::new(),
                };

                // Build scoped JobStore for this thread
                let store = match JobStore::new(&thread_path, self.max_jobs_per_thread).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            thread = %thread_name,
                            error = %e,
                            "Failed to open job store for thread"
                        );
                        continue;
                    }
                };

                let jobs = match store.list().await {
                    Ok(jobs) => jobs,
                    Err(e) => {
                        tracing::warn!(
                            thread = %thread_name,
                            error = %e,
                            "Failed to list jobs for thread"
                        );
                        continue;
                    }
                };

                for job in jobs {
                    if !job.enabled {
                        continue;
                    }
                    if job.next_fire_at.is_none_or(|t| t > now) {
                        continue;
                    }
                    due.push(DueJob {
                        job: job.clone(),
                        store: store.clone(),
                        thread_name: thread_name.clone(),
                        channel_name: channel_name.clone(),
                    });
                }
            }
        }

        due
    }

    /// Fire a single job by injecting an InboundMessage into the originating thread.
    async fn fire_job(&self, job: &jyc_types::JobConfig) -> Result<()> {
        let tms = self.thread_managers.lock().await;
        let tm = tms.get(&job.channel_name).ok_or_else(|| {
            anyhow::anyhow!(
                "thread manager not found for channel '{}'",
                job.channel_name
            )
        })?;

        let message = InboundMessage {
            id: uuid::Uuid::new_v4().to_string(),
            channel: job.channel.clone(),
            channel_uid: format!("job-{}", job.id),
            sender: "scheduler".to_string(),
            sender_address: "scheduler@jyc".to_string(),
            recipients: vec![],
            topic: format!(
                "Scheduled job: {}",
                job.prompt.chars().take(80).collect::<String>()
            ),
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
                m.insert(
                    "job_id".to_string(),
                    serde_json::Value::String(job.id.clone()),
                );
                m
            },
            matched_pattern: None,
        };

        let pattern_match = PatternMatch {
            pattern_name: String::new(),
            channel: job.channel.clone(),
            matches: std::collections::HashMap::new(),
        };

        tm.enqueue(
            message,
            job.thread_name.clone(),
            pattern_match,
            None,
            true,
            None,
        )
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
        let now = Utc::now();

        let mut earliest_next: Option<chrono::DateTime<Utc>> = None;

        for workspace_dir in &self.workspace_dirs {
            let mut thread_entries = match tokio::fs::read_dir(workspace_dir).await {
                Ok(entries) => entries,
                Err(_) => continue,
            };

            while let Ok(Some(entry)) = thread_entries.next_entry().await {
                let thread_path = entry.path();
                if !thread_path.is_dir() {
                    continue;
                }

                let jobs_dir = thread_path.join(".jyc").join("jobs");
                if !jobs_dir.exists() {
                    continue;
                }

                let store = match JobStore::new(&thread_path, self.max_jobs_per_thread).await {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                let jobs = match store.list().await {
                    Ok(jobs) => jobs,
                    Err(_) => continue,
                };

                for job in &jobs {
                    if !job.enabled {
                        continue;
                    }
                    if let Some(next) = job.next_fire_at.filter(|t| *t > now) {
                        earliest_next = match earliest_next {
                            Some(current) => Some(current.min(next)),
                            None => Some(next),
                        };
                    }
                }
            }
        }

        match earliest_next {
            Some(t) => {
                let duration = (t - now).to_std().unwrap_or(self.scan_interval);
                duration.min(self.scan_interval)
            }
            None => self.scan_interval,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_core::job_store::JobStore;
    use std::path::Path;
    use tempfile::tempdir;

    async fn create_test_scheduler(workspace_dirs: Vec<PathBuf>, enabled: bool) -> JobScheduler {
        let tms = Arc::new(Mutex::new(HashMap::new()));
        JobScheduler::new(tms, workspace_dirs, 60, 10, enabled)
    }

    /// Create a thread directory with a jobs store.
    async fn make_thread_with_jobs(
        workspace: &Path,
        thread_name: &str,
        jobs: Vec<jyc_types::JobConfig>,
    ) {
        let thread_path = workspace.join(thread_name);
        tokio::fs::create_dir_all(&thread_path).await.unwrap();
        let store = JobStore::new(&thread_path, 10).await.unwrap();
        for job in jobs {
            store.create(&job).await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_disabled_scheduler_returns_immediately() {
        let scheduler = create_test_scheduler(vec![], false).await;
        let cancel = CancellationToken::new();

        tokio::time::timeout(std::time::Duration::from_millis(100), scheduler.run(cancel))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_next_sleep_duration_no_jobs() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        let scheduler = create_test_scheduler(vec![workspace], true).await;
        let dur = scheduler.next_sleep_duration().await;
        assert_eq!(dur, std::time::Duration::from_secs(60));
    }

    #[tokio::test]
    async fn test_next_sleep_duration_with_future_job() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        let job = jyc_types::JobConfig::new_one_time(
            Utc::now() + chrono::Duration::hours(1),
            "test".to_string(),
            "email".to_string(),
            "work".to_string(),
            "future task".to_string(),
        );
        make_thread_with_jobs(&workspace, "thread-1", vec![job]).await;

        let scheduler = create_test_scheduler(vec![workspace], true).await;
        let dur = scheduler.next_sleep_duration().await;
        assert_eq!(dur, std::time::Duration::from_secs(60));
    }

    #[tokio::test]
    async fn test_run_cycle_skips_disabled_jobs() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        let mut job = jyc_types::JobConfig::new_one_time(
            Utc::now(),
            "disabled-test".to_string(),
            "email".to_string(),
            "test-channel".to_string(),
            "Should not fire".to_string(),
        );
        job.enabled = false;
        make_thread_with_jobs(&workspace, "thread-1", vec![job]).await;

        let scheduler = create_test_scheduler(vec![workspace], true).await;
        scheduler.run_cycle().await;
    }

    #[tokio::test]
    async fn test_run_cycle_skips_pending_thread_without_jobs_dir() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        // Create a thread directory WITHOUT .jyc/jobs/ — should be silently skipped
        let thread_path = workspace.join("no-jobs-thread");
        tokio::fs::create_dir_all(&thread_path).await.unwrap();

        let scheduler = create_test_scheduler(vec![workspace], true).await;
        // Should not panic
        scheduler.run_cycle().await;
    }

    /// Happy path: multiple threads with a mix of due, future, and disabled jobs.
    /// The scheduler should discover due jobs in the right threads, attempt to
    /// fire them, and since no ThreadManager is registered, leave them unchanged
    /// for retry.
    #[tokio::test]
    async fn test_run_cycle_discovery_across_threads() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        // Thread 1: one due job (one-time, now)
        let due_job = jyc_types::JobConfig::new_one_time(
            Utc::now(),
            "thread-1".to_string(),
            "email".to_string(),
            "ch-1".to_string(),
            "due job".to_string(),
        );
        let due_id = due_job.id.clone();
        make_thread_with_jobs(&workspace, "thread-1", vec![due_job]).await;

        // Thread 2: one future job (should not be due)
        let future_job = jyc_types::JobConfig::new_one_time(
            Utc::now() + chrono::Duration::hours(2),
            "thread-2".to_string(),
            "email".to_string(),
            "ch-2".to_string(),
            "future job".to_string(),
        );
        let future_id = future_job.id.clone();
        make_thread_with_jobs(&workspace, "thread-2", vec![future_job]).await;

        // Thread 3: one disabled due job (should be skipped)
        let mut disabled_job = jyc_types::JobConfig::new_one_time(
            Utc::now(),
            "thread-3".to_string(),
            "email".to_string(),
            "ch-3".to_string(),
            "disabled due job".to_string(),
        );
        disabled_job.enabled = false;
        let disabled_id = disabled_job.id.clone();
        make_thread_with_jobs(&workspace, "thread-3", vec![disabled_job]).await;

        // Thread 4: no jobs dir at all (should be silently skipped)
        let no_jobs_thread = workspace.join("thread-4");
        tokio::fs::create_dir_all(&no_jobs_thread).await.unwrap();

        let scheduler = create_test_scheduler(vec![workspace], true).await;
        scheduler.run_cycle().await;

        // Due job in thread-1: fire_job fails (no TM), so it stays unchanged
        let thread1_path = tmp.path().join("workspace/thread-1");
        let store1 = JobStore::new(&thread1_path, 10).await.unwrap();
        let updated_due = store1.get(&due_id).await.unwrap().unwrap();
        assert!(
            updated_due.enabled,
            "Due job should remain enabled (fire_job failed)"
        );
        assert!(
            updated_due.last_fired_at.is_none(),
            "Due job should not be marked fired"
        );

        // Future job in thread-2: untouched
        let thread2_path = tmp.path().join("workspace/thread-2");
        let store2 = JobStore::new(&thread2_path, 10).await.unwrap();
        let updated_future = store2.get(&future_id).await.unwrap().unwrap();
        assert!(updated_future.enabled, "Future job should remain enabled");
        assert!(
            updated_future.last_fired_at.is_none(),
            "Future job should not be fired"
        );

        // Disabled job in thread-3: untouched
        let thread3_path = tmp.path().join("workspace/thread-3");
        let store3 = JobStore::new(&thread3_path, 10).await.unwrap();
        let updated_disabled = store3.get(&disabled_id).await.unwrap().unwrap();
        assert!(
            !updated_disabled.enabled,
            "Disabled job should stay disabled"
        );
        assert!(
            updated_disabled.last_fired_at.is_none(),
            "Disabled job should not be fired"
        );
    }

    /// Happy path: due job in one thread, non-due in another, across two
    /// workspace directories (simulating multiple channels).
    #[tokio::test]
    async fn test_run_cycle_multi_workspace_discovery() {
        let tmp = tempdir().unwrap();

        // Workspace A: one due job
        let ws_a = tmp.path().join("channel-a/workspace");
        tokio::fs::create_dir_all(&ws_a).await.unwrap();
        let due = jyc_types::JobConfig::new_one_time(
            Utc::now(),
            "thread-a1".to_string(),
            "email".to_string(),
            "channel-a".to_string(),
            "A-due".to_string(),
        );
        let due_id_a = due.id.clone();
        make_thread_with_jobs(&ws_a, "thread-a1", vec![due]).await;

        // Workspace B: one future job
        let ws_b = tmp.path().join("channel-b/workspace");
        tokio::fs::create_dir_all(&ws_b).await.unwrap();
        let future = jyc_types::JobConfig::new_one_time(
            Utc::now() + chrono::Duration::hours(3),
            "thread-b1".to_string(),
            "email".to_string(),
            "channel-b".to_string(),
            "B-future".to_string(),
        );
        let future_id_b = future.id.clone();
        make_thread_with_jobs(&ws_b, "thread-b1", vec![future]).await;

        let scheduler = create_test_scheduler(vec![ws_a, ws_b], true).await;
        scheduler.run_cycle().await;

        // A's due job should have been discovered (but fire_job fails)
        let store_a = JobStore::new(&tmp.path().join("channel-a/workspace/thread-a1"), 10)
            .await
            .unwrap();
        let a_job = store_a.get(&due_id_a).await.unwrap().unwrap();
        assert!(a_job.enabled, "A's job stays enabled (no TM)");

        // B's future job should be untouched
        let store_b = JobStore::new(&tmp.path().join("channel-b/workspace/thread-b1"), 10)
            .await
            .unwrap();
        let b_job = store_b.get(&future_id_b).await.unwrap().unwrap();
        assert!(b_job.enabled, "B's job stays enabled");
        assert!(
            b_job.last_fired_at.is_none(),
            "B's future job should not fire"
        );
    }

    /// Verify that next_sleep_duration accounts for jobs across all threads.
    #[tokio::test]
    async fn test_next_sleep_duration_earliest_across_threads() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();

        // Job in thread-1 fires in 30 minutes
        let soon = jyc_types::JobConfig::new_one_time(
            Utc::now() + chrono::Duration::minutes(30),
            "thread-1".to_string(),
            "email".to_string(),
            "work".to_string(),
            "soon".to_string(),
        );
        make_thread_with_jobs(&workspace, "thread-1", vec![soon]).await;

        // Job in thread-2 fires in 2 hours
        let later = jyc_types::JobConfig::new_one_time(
            Utc::now() + chrono::Duration::hours(2),
            "thread-2".to_string(),
            "email".to_string(),
            "work".to_string(),
            "later".to_string(),
        );
        make_thread_with_jobs(&workspace, "thread-2", vec![later]).await;

        let scheduler = create_test_scheduler(vec![workspace], true).await;
        let dur = scheduler.next_sleep_duration().await;

        // Should cap at scan interval (60s) since both are > 60s away
        assert_eq!(dur, std::time::Duration::from_secs(60));
    }
}
