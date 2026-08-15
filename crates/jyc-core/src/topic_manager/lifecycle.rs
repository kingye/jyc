//! `TopicManager` impl block: lifecycle.rs methods.
//!
//! Extracted from the monolithic `topic_manager.rs`.

use anyhow::{Context, Result};

/// Per-topic queue stats.
use super::TopicManager;

impl TopicManager {
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        {
            // Cancel all per-topic tokens
            let mut cancels = self.topic_cancels.lock().await;
            for (_, token) in cancels.drain() {
                token.cancel();
            }
        }
        {
            let mut queues = self.topic_queues.lock().await;
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

    /// Cancel the AI processing for a topic without deleting its directory.
    ///
    /// Triggers the per-topic cancellation token so the agent loop breaks
    /// at the next iteration. The worker exits, and the next message will
    /// spawn a new worker automatically. Topic directory and queue are preserved.
    ///
    /// Returns `true` if an active token was found and cancelled, `false` if
    /// the topic had no running worker (callers can report this honestly
    /// instead of claiming success).
    pub async fn cancel_topic(&self, topic_name: &str) -> bool {
        let cancels = self.topic_cancels.lock().await;
        if let Some(token) = cancels.get(topic_name) {
            token.cancel();
            tracing::info!(topic = %topic_name, "Topic AI processing cancelled via cancel_topic");
            true
        } else {
            tracing::warn!(topic = %topic_name, "cancel_topic: no cancellation token found (topic may not be processing)");
            false
        }
    }

    /// Close and delete a topic's directory.
    ///
    /// This is channel-agnostic — all topics use the same cleanup logic.
    /// Removes the topic directory from disk and cleans up in-memory state.
    pub async fn close_topic(&self, topic_name: &str) -> Result<()> {
        let topic_path = self
            .topic_path(topic_name)
            .await
            .unwrap_or_else(|| self.storage.workspace().join(topic_name));

        if topic_path.exists() {
            // Check for symlinks (e.g., repo/) and remove them before remove_dir_all
            // to prevent remove_dir_all from following symlinks into shared directories
            let repo_symlink = topic_path.join("repo");
            match tokio::fs::symlink_metadata(&repo_symlink).await {
                Ok(meta) if meta.file_type().is_symlink() => {
                    if let Err(e) = tokio::fs::remove_file(&repo_symlink).await {
                        tracing::warn!(
                            error = %e,
                            path = %repo_symlink.display(),
                            "Failed to remove repo symlink before topic deletion"
                        );
                    } else {
                        tracing::debug!(
                            topic = %topic_name,
                            "Removed repo symlink before topic deletion"
                        );
                    }
                }
                _ => {}
            }

            tokio::fs::remove_dir_all(&topic_path)
                .await
                .context(format!(
                    "Failed to remove topic directory: {:?}",
                    topic_path
                ))?;
            tracing::info!(topic = %topic_name, "Topic directory deleted");
        }

        // Clean up orphaned shared repos (repos/ dirs no longer referenced by any topic)
        self.cleanup_orphaned_shared_repos().await;

        self.cleanup_topic_state(topic_name).await;
        Ok(())
    }

    /// Reset the session for a topic with configurable compression.
    ///
    /// Delegates to the agent service's `reset_session` method.
    pub async fn reset_session(
        &self,
        topic_name: &str,
        config: &jyc_types::channel::ResetCompressionConfig,
    ) -> Result<()> {
        let topic_path = self
            .topic_path(topic_name)
            .await
            .unwrap_or_else(|| self.storage.workspace().join(topic_name));
        self.agent
            .reset_session(&topic_path, topic_name, config)
            .await?;

        // Publish SessionStatus event for dashboard visibility
        if self.enable_events {
            let event_bus = self.get_or_create_event_bus(topic_name).await;
            if let Some(bus) = event_bus {
                let mode_str = match config.mode {
                    jyc_types::channel::CompressionMode::None => "none",
                    jyc_types::channel::CompressionMode::Heuristic => "heuristic",
                    jyc_types::channel::CompressionMode::Llm => "llm",
                };
                let _ = bus
                    .publish(crate::topic_event::TopicEvent::SessionStatus {
                        topic_name: topic_name.to_string(),
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

    /// Clean up in-memory state (queues, event buses) for a closed topic.
    async fn cleanup_topic_state(&self, topic_name: &str) {
        // Cancel the per-topic token so the worker + event listener exit promptly
        {
            let mut cancels = self.topic_cancels.lock().await;
            if let Some(token) = cancels.remove(topic_name) {
                token.cancel();
                tracing::debug!(topic = %topic_name, "Per-topic cancellation token cancelled");
            }
        }

        // Remove from topic_queues
        {
            let mut queues = self.topic_queues.lock().await;
            queues.remove(topic_name);
        }

        // Remove from event_buses
        if self.enable_events {
            let mut event_buses = self.event_buses.lock().await;
            event_buses.remove(topic_name);
        }

        tracing::debug!(topic = %topic_name, "Topic in-memory state cleaned up");
    }

    /// Clean up shared repos that are no longer referenced by any active topic.
    ///
    /// Scans `<workspace>/repos/` and checks if any topic directory still has
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
            if let Ok(mut topic_entries) = tokio::fs::read_dir(&workspace).await {
                while let Ok(Some(topic_entry)) = topic_entries.next_entry().await {
                    let topic_path = topic_entry.path();
                    if !topic_path.is_dir() {
                        continue;
                    }
                    let repo_link = topic_path.join("repo");
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
