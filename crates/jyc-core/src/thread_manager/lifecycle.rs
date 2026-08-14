//! `ThreadManager` impl block: lifecycle.rs methods.
//!
//! Extracted from the monolithic `thread_manager.rs`.

use anyhow::{Context, Result};

/// Per-thread queue stats.
use super::ThreadManager;

impl ThreadManager {
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        {
            // Cancel all per-thread tokens
            let mut cancels = self.thread_cancels.lock().await;
            for (_, token) in cancels.drain() {
                token.cancel();
            }
        }
        {
            let mut queues = self.thread_queues.lock().await;
            queues.clear();
        }
        {
            // Clear event buses
            let mut event_buses = self.event_buses.lock().await;
            event_buses.clear();
        }
        let mut handles = self.worker_handles.lock().await;
        for handle in handles.drain(..) {
            let _ = handle.await;
        }
        tracing::info!("All workers shut down");
    }

    /// Cancel the AI processing for a thread without deleting its directory.
    ///
    /// Triggers the per-thread cancellation token so the agent loop breaks
    /// at the next iteration. The worker exits, and the next message will
    /// spawn a new worker automatically. Thread directory and queue are preserved.
    ///
    /// Returns `true` if an active token was found and cancelled, `false` if
    /// the thread had no running worker (callers can report this honestly
    /// instead of claiming success).
    pub async fn cancel_thread(&self, thread_name: &str) -> bool {
        let cancels = self.thread_cancels.lock().await;
        if let Some(token) = cancels.get(thread_name) {
            token.cancel();
            tracing::info!(thread = %thread_name, "Thread AI processing cancelled via cancel_thread");
            true
        } else {
            tracing::warn!(thread = %thread_name, "cancel_thread: no cancellation token found (thread may not be processing)");
            false
        }
    }

    /// Close and delete a thread's directory.
    ///
    /// This is channel-agnostic — all threads use the same cleanup logic.
    /// Removes the thread directory from disk and cleans up in-memory state.
    pub async fn close_thread(&self, thread_name: &str) -> Result<()> {
        let thread_path = self
            .thread_path(thread_name)
            .await
            .unwrap_or_else(|| self.storage.workspace().join(thread_name));

        if thread_path.exists() {
            // Check for symlinks (e.g., repo/) and remove them before remove_dir_all
            // to prevent remove_dir_all from following symlinks into shared directories
            let repo_symlink = thread_path.join("repo");
            match tokio::fs::symlink_metadata(&repo_symlink).await {
                Ok(meta) if meta.file_type().is_symlink() => {
                    if let Err(e) = tokio::fs::remove_file(&repo_symlink).await {
                        tracing::warn!(
                            error = %e,
                            path = %repo_symlink.display(),
                            "Failed to remove repo symlink before thread deletion"
                        );
                    } else {
                        tracing::debug!(
                            thread = %thread_name,
                            "Removed repo symlink before thread deletion"
                        );
                    }
                }
                _ => {}
            }

            tokio::fs::remove_dir_all(&thread_path)
                .await
                .context(format!(
                    "Failed to remove thread directory: {:?}",
                    thread_path
                ))?;
            tracing::info!(thread = %thread_name, "Thread directory deleted");
        }

        // Clean up orphaned shared repos (repos/ dirs no longer referenced by any thread)
        self.cleanup_orphaned_shared_repos().await;

        self.cleanup_thread_state(thread_name).await;
        Ok(())
    }

    /// Reset the session for a thread with configurable compression.
    ///
    /// Delegates to the agent service's `reset_session` method.
    pub async fn reset_session(
        &self,
        thread_name: &str,
        config: &jyc_types::channel::ResetCompressionConfig,
    ) -> Result<()> {
        let thread_path = self
            .thread_path(thread_name)
            .await
            .unwrap_or_else(|| self.storage.workspace().join(thread_name));
        self.agent
            .reset_session(&thread_path, thread_name, config)
            .await?;

        // Publish SessionStatus event for dashboard visibility
        if self.enable_events {
            let event_bus = self.get_or_create_event_bus(thread_name).await;
            if let Some(bus) = event_bus {
                let mode_str = match config.mode {
                    jyc_types::channel::CompressionMode::None => "none",
                    jyc_types::channel::CompressionMode::Heuristic => "heuristic",
                    jyc_types::channel::CompressionMode::Llm => "llm",
                };
                let _ = bus
                    .publish(crate::thread_event::ThreadEvent::SessionStatus {
                        thread_name: thread_name.to_string(),
                        status_type: "session_reset".to_string(),
                        attempt: None,
                        message: Some(format!("mode={mode_str}")),
                        timestamp: chrono::Utc::now(),
                    })
                    .await;
            }
        }

        Ok(())
    }

    /// Clean up in-memory state (queues, event buses) for a closed thread.
    async fn cleanup_thread_state(&self, thread_name: &str) {
        // Cancel the per-thread token so the worker + event listener exit promptly
        {
            let mut cancels = self.thread_cancels.lock().await;
            if let Some(token) = cancels.remove(thread_name) {
                token.cancel();
                tracing::debug!(thread = %thread_name, "Per-thread cancellation token cancelled");
            }
        }

        // Remove from thread_queues
        {
            let mut queues = self.thread_queues.lock().await;
            queues.remove(thread_name);
        }

        // Remove from event_buses
        if self.enable_events {
            let mut event_buses = self.event_buses.lock().await;
            event_buses.remove(thread_name);
        }

        tracing::debug!(thread = %thread_name, "Thread in-memory state cleaned up");
    }

    /// Clean up shared repos that are no longer referenced by any active thread.
    ///
    /// Scans `<workspace>/repos/` and checks if any thread directory still has
    /// a symlink pointing to each shared repo. Orphaned shared repos are deleted.
    async fn cleanup_orphaned_shared_repos(&self) {
        let workspace = self.storage.workspace();
        let repos_dir = workspace.join("repos");

        let mut repos_entries = match tokio::fs::read_dir(&repos_dir).await {
            Ok(entries) => entries,
            Err(_) => return,
        };

        while let Ok(Some(entry)) = repos_entries.next_entry().await {
            let shared_repo_path = entry.path();
            if !shared_repo_path.is_dir() {
                continue;
            }

            let group_key = match entry.file_name().to_str() {
                Some(name) => name.to_string(),
                None => continue,
            };

            let mut is_referenced = false;
            if let Ok(mut thread_entries) = tokio::fs::read_dir(&workspace).await {
                while let Ok(Some(thread_entry)) = thread_entries.next_entry().await {
                    let thread_path = thread_entry.path();
                    if !thread_path.is_dir() {
                        continue;
                    }
                    let repo_link = thread_path.join("repo");
                    match tokio::fs::symlink_metadata(&repo_link).await {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            if let Ok(target) = std::fs::read_link(&repo_link)
                                && target == shared_repo_path
                            {
                                is_referenced = true;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }

            if !is_referenced {
                if let Err(e) = tokio::fs::remove_dir_all(&shared_repo_path).await {
                    tracing::warn!(
                        error = %e,
                        path = %shared_repo_path.display(),
                        "Failed to remove orphaned shared repo"
                    );
                } else {
                    tracing::info!(
                        group_key = %group_key,
                        "Removed orphaned shared repo"
                    );
                }
            }
        }
    }
}
