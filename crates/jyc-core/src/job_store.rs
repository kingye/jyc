//! File-based store for scheduled jobs.
//!
//! Each job is stored as `<thread_path>/.jyc/jobs/<id>.json`. The store
//! provides CRUD operations and is used by both the `JobScheduler` and
//! the agent job management tools. Each thread has its own isolated store.

use anyhow::{Context, Result};
use jyc_types::JobConfig;
use std::path::{Path, PathBuf};
use tokio::fs;

/// File-based job store with per-file JSON persistence.
///
/// Jobs are stored as individual JSON files under the thread's
/// `.jyc/jobs/` directory. This avoids SQLite dependency while
/// providing atomic per-file operations.
#[derive(Clone)]
pub struct JobStore {
    /// Thread directory path. Jobs are stored at `{.jyc/jobs/<id>.json`.
    thread_path: PathBuf,
    /// Maximum number of jobs allowed per thread.
    max_jobs: usize,
}

impl JobStore {
    /// Create a new JobStore for the given thread path.
    ///
    /// The jobs directory (`.jyc/jobs/`) is created if it doesn't exist.
    pub async fn new(thread_path: &Path, max_jobs: usize) -> Result<Self> {
        let jobs_dir = thread_path.join(".jyc").join("jobs");
        fs::create_dir_all(&jobs_dir)
            .await
            .with_context(|| format!("failed to create jobs directory: {}", jobs_dir.display()))?;

        Ok(Self {
            thread_path: thread_path.to_path_buf(),
            max_jobs,
        })
    }

    /// Return the path to the jobs directory.
    pub fn jobs_dir(&self) -> PathBuf {
        self.thread_path.join(".jyc").join("jobs")
    }

    /// Return the path to the job file for the given ID.
    fn job_path(&self, id: &str) -> PathBuf {
        self.jobs_dir().join(format!("{id}.json"))
    }

    /// List all jobs in the store.
    ///
    /// Reads and deserializes every `<id>.json` file in the jobs directory.
    /// Returns an empty Vec if the directory doesn't exist or has no files.
    pub async fn list(&self) -> Result<Vec<JobConfig>> {
        let mut jobs = Vec::new();
        let jobs_dir = self.jobs_dir();
        let mut entries = match fs::read_dir(&jobs_dir).await {
            Ok(entries) => entries,
            Err(_) => return Ok(Vec::new()),
        };

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                match self.read_job_file(&path).await {
                    Ok(job) => jobs.push(job),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            path = %path.display(),
                            "Failed to read job file, skipping"
                        );
                    }
                }
            }
        }

        Ok(jobs)
    }

    /// Get a single job by ID. Returns `None` if the job file doesn't exist.
    pub async fn get(&self, id: &str) -> Result<Option<JobConfig>> {
        let path = self.job_path(id);
        if !path.exists() {
            return Ok(None);
        }
        self.read_job_file(&path).await.map(Some)
    }

    /// Create a new job by writing its JSON file.
    ///
    /// Returns an error if a job with the same ID already exists or if
    /// the job count for this thread would exceed `max_jobs`.
    pub async fn create(&self, job: &JobConfig) -> Result<()> {
        let path = self.job_path(&job.id);
        if path.exists() {
            anyhow::bail!("job '{}' already exists", job.id);
        }

        // Enforce per-thread job limit
        let current = self.list().await?.len();
        if current >= self.max_jobs {
            anyhow::bail!(
                "job limit per thread reached ({} max). Delete an existing job first.",
                self.max_jobs
            );
        }

        self.write_job_file(&path, job).await
    }

    /// Update an existing job by overwriting its JSON file.
    ///
    /// Returns an error if the job doesn't exist.
    pub async fn update(&self, job: &JobConfig) -> Result<()> {
        let path = self.job_path(&job.id);
        if !path.exists() {
            anyhow::bail!("job '{}' not found", job.id);
        }
        self.write_job_file(&path, job).await
    }

    /// Upsert a job — create or update depending on whether it exists.
    pub async fn upsert(&self, job: &JobConfig) -> Result<()> {
        let path = self.job_path(&job.id);
        self.write_job_file(&path, job).await
    }

    /// Delete a job by ID. Returns `Ok(true)` if deleted, `Ok(false)` if not found.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let path = self.job_path(id);
        if !path.exists() {
            return Ok(false);
        }
        fs::remove_file(&path)
            .await
            .with_context(|| format!("failed to delete job '{}'", id))?;
        tracing::info!(job_id = %id, "Job deleted");
        Ok(true)
    }

    /// Read and deserialize a job from a JSON file path.
    async fn read_job_file(&self, path: &Path) -> Result<JobConfig> {
        let content = fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read job file: {}", path.display()))?;
        let job: JobConfig = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse job file: {}", path.display()))?;
        Ok(job)
    }

    /// Serialize and write a job to a JSON file path (atomic via temp + rename).
    async fn write_job_file(&self, path: &Path, job: &JobConfig) -> Result<()> {
        let content = serde_json::to_string_pretty(job)
            .with_context(|| format!("failed to serialize job '{}'", job.id))?;

        // Atomic write: write to temp file, then rename
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, &content)
            .await
            .with_context(|| format!("failed to write job '{}'", job.id))?;
        fs::rename(&tmp_path, path)
            .await
            .with_context(|| format!("failed to rename temp file for job '{}'", job.id))?;

        tracing::debug!(job_id = %job.id, "Job saved");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::tempdir;

    fn sample_job(id: &str) -> JobConfig {
        JobConfig {
            id: id.to_string(),
            cron: Some("0 0 8 * * * *".to_string()),
            at: None,
            enabled: true,
            thread_name: "test-thread".to_string(),
            channel: "email".to_string(),
            channel_name: "work".to_string(),
            prompt: "Send daily summary".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_fired_at: None,
            next_fire_at: Some(Utc::now()),
        }
    }

    #[tokio::test]
    async fn test_create_and_get() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path(), 10).await.unwrap();
        let job = sample_job("job-1");

        store.create(&job).await.unwrap();
        let retrieved = store.get("job-1").await.unwrap().unwrap();
        assert_eq!(retrieved.id, "job-1");
        assert_eq!(retrieved.prompt, "Send daily summary");
    }

    #[tokio::test]
    async fn test_create_duplicate_fails() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path(), 10).await.unwrap();
        let job = sample_job("job-1");

        store.create(&job).await.unwrap();
        let result = store.create(&job).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_update() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path(), 10).await.unwrap();
        let mut job = sample_job("job-1");
        store.create(&job).await.unwrap();

        job.prompt = "Updated prompt".to_string();
        store.update(&job).await.unwrap();

        let retrieved = store.get("job-1").await.unwrap().unwrap();
        assert_eq!(retrieved.prompt, "Updated prompt");
    }

    #[tokio::test]
    async fn test_update_nonexistent_fails() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path(), 10).await.unwrap();
        let job = sample_job("nonexistent");
        let result = store.update(&job).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_delete() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path(), 10).await.unwrap();
        let job = sample_job("job-1");
        store.create(&job).await.unwrap();

        let deleted = store.delete("job-1").await.unwrap();
        assert!(deleted);

        let retrieved = store.get("job-1").await.unwrap();
        assert!(retrieved.is_none());
    }

    #[tokio::test]
    async fn test_delete_nonexistent_returns_false() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path(), 10).await.unwrap();
        let deleted = store.delete("nonexistent").await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test]
    async fn test_list() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path(), 10).await.unwrap();

        let job1 = sample_job("job-1");
        let job2 = sample_job("job-2");
        store.create(&job1).await.unwrap();
        store.create(&job2).await.unwrap();

        let jobs = store.list().await.unwrap();
        assert_eq!(jobs.len(), 2);
    }

    #[tokio::test]
    async fn test_list_empty() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path(), 10).await.unwrap();
        let jobs = store.list().await.unwrap();
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn test_upsert_creates_new() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path(), 10).await.unwrap();
        let job = sample_job("job-1");

        store.upsert(&job).await.unwrap();
        assert!(store.get("job-1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_upsert_updates_existing() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path(), 10).await.unwrap();
        let mut job = sample_job("job-1");
        store.upsert(&job).await.unwrap();

        job.prompt = "Upserted prompt".to_string();
        store.upsert(&job).await.unwrap();

        let retrieved = store.get("job-1").await.unwrap().unwrap();
        assert_eq!(retrieved.prompt, "Upserted prompt");
    }

    #[tokio::test]
    async fn test_max_jobs_limit() {
        let tmp = tempdir().unwrap();
        let store = JobStore::new(tmp.path(), 2).await.unwrap();

        let job1 = sample_job("job-1");
        let job2 = sample_job("job-2");
        let job3 = sample_job("job-3");

        store.create(&job1).await.unwrap();
        store.create(&job2).await.unwrap();

        // Third job should be rejected
        let result = store.create(&job3).await;
        assert!(result.is_err(), "Should reject when limit reached");
        assert!(
            result.unwrap_err().to_string().contains("job limit"),
            "Error should mention job limit"
        );
    }
}
