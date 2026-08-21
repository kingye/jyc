//! `TopicManager` impl block: lifecycle.rs methods.
//!
//! Extracted from the monolithic `topic_manager.rs`.

use anyhow::{Context, Result};
use std::path::Path;

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

        self.cleanup_topic_state(topic_name).await;
        Ok(())
    }

    /// Close a topic in response to an upstream close event (issue/PR
    /// closed, chat disbanded, ...).
    ///
    /// Hard safety rule: the topic directory is only deleted when it
    /// resolves UNDER the agents workspace root (`<data_home>/agents/`).
    /// Topics pinned to a custom `topic_path` (e.g. a real project
    /// checkout) are never deleted by automation — they are skipped with
    /// an info log.
    ///
    /// Returns `true` when the topic was actually closed.
    pub async fn auto_close_topic(&self, topic_name: &str) -> Result<bool> {
        let agents_root = crate::topic_path::resolve_agents_workspace_root(&self.workdir);
        self.auto_close_topic_under(topic_name, &agents_root).await
    }

    /// Testable core of `auto_close_topic` with an explicit agents root.
    async fn auto_close_topic_under(&self, topic_name: &str, agents_root: &Path) -> Result<bool> {
        let topic_path = self
            .topic_path(topic_name)
            .await
            .unwrap_or_else(|| self.storage.workspace().join(topic_name));
        if !path_is_under(&topic_path, agents_root).await {
            tracing::info!(
                topic = %topic_name,
                path = %topic_path.display(),
                "Auto-close skipped: topic path is not under the agents workspace root"
            );
            return Ok(false);
        }
        self.close_topic(topic_name).await?;
        Ok(true)
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
}

/// Canonicalized containment check: `path` must be strictly under `root`.
///
/// Canonicalization resolves symlinks and `..`, so a topic dir symlinked
/// out of the agents tree is still refused. Non-existent paths fall back
/// to a literal comparison (nothing to delete in that case anyway).
async fn path_is_under(path: &Path, root: &Path) -> bool {
    let path = tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf());
    let root = tokio::fs::canonicalize(root)
        .await
        .unwrap_or_else(|_| root.to_path_buf());
    path != root && path.starts_with(&root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_storage::MessageStorage;
    use crate::metrics::MetricsCollector;
    use crate::static_agent::StaticAgentService;
    use arc_swap::ArcSwap;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    struct NoopOutbound;

    #[async_trait::async_trait]
    impl jyc_types::OutboundAdapter for NoopOutbound {
        fn channel_type(&self) -> &str {
            "test"
        }
        async fn connect(&self) -> Result<()> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<()> {
            Ok(())
        }
        fn clean_body(&self, raw_body: &str) -> String {
            raw_body.to_string()
        }
        async fn send_reply(
            &self,
            _original: &jyc_types::InboundMessage,
            _reply_text: &str,
            _topic_path: &std::path::Path,
            _message_dir: &str,
            _attachments: Option<&[jyc_types::OutboundAttachment]>,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "noop".to_string(),
            })
        }
        async fn send_message(
            &self,
            _recipient: &str,
            _subject: &str,
            _body: &str,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "noop".to_string(),
            })
        }
    }

    fn make_tm(workspace: &Path) -> Arc<TopicManager> {
        let storage = Arc::new(MessageStorage::new(workspace));
        let cancel = CancellationToken::new();
        let (metrics, _stats, _task) = MetricsCollector::new(CancellationToken::new()).start();
        let config = Arc::new(ArcSwap::from_pointee(
            jyc_types::load_config_from_str(
                r#"
[general]
[channels.test]
type = "email"
[channels.test.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.test.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
"#,
            )
            .unwrap(),
        ));
        Arc::new(TopicManager::new_with_options(
            1,
            10,
            storage,
            Arc::new(NoopOutbound),
            Arc::new(StaticAgentService::new("ok")),
            cancel,
            false,
            workspace.join("templates"),
            config,
            "test".to_string(),
            "websocket".to_string(),
            workspace.parent().unwrap_or(workspace).to_path_buf(),
            workspace.to_path_buf(),
            metrics,
            None,
        ))
    }

    /// Topic inside the agents root: auto-close deletes it.
    #[tokio::test]
    async fn auto_close_deletes_topic_under_agents_root() {
        let tmp = tempdir().unwrap();
        let agents_root = tmp.path().join("agents");
        let workspace = agents_root.join("agent-x");
        let topic_dir = workspace.join("plan-42");
        std::fs::create_dir_all(&topic_dir).unwrap();

        let tm = make_tm(&workspace);
        let closed = tm
            .auto_close_topic_under("plan-42", &agents_root)
            .await
            .unwrap();
        assert!(closed);
        assert!(!topic_dir.exists());
    }

    /// Topic pinned to a custom path outside the agents root: never deleted.
    #[tokio::test]
    async fn auto_close_skips_custom_path_outside_agents_root() {
        let tmp = tempdir().unwrap();
        let agents_root = tmp.path().join("agents");
        let workspace = agents_root.join("agent-x");
        std::fs::create_dir_all(&workspace).unwrap();
        let outside = tmp.path().join("projects").join("jyc");
        std::fs::create_dir_all(&outside).unwrap();

        let tm = make_tm(&workspace);
        tm.topic_paths
            .lock()
            .await
            .insert("jyc".to_string(), outside.clone());

        let closed = tm
            .auto_close_topic_under("jyc", &agents_root)
            .await
            .unwrap();
        assert!(!closed);
        assert!(outside.exists());
    }

    /// A symlinked topic dir pointing outside the agents root must not be
    /// deleted — canonicalization resolves the link before the check.
    #[cfg(unix)]
    #[tokio::test]
    async fn auto_close_refuses_symlink_escape() {
        let tmp = tempdir().unwrap();
        let agents_root = tmp.path().join("agents");
        let workspace = agents_root.join("agent-x");
        std::fs::create_dir_all(&workspace).unwrap();
        let real = tmp.path().join("real-project");
        std::fs::create_dir_all(&real).unwrap();
        let link = workspace.join("evil");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let tm = make_tm(&workspace);
        let closed = tm
            .auto_close_topic_under("evil", &agents_root)
            .await
            .unwrap();
        assert!(!closed);
        assert!(real.exists());
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    }

    /// The agents root itself must never match (guard against catastrophic
    /// deletion of the whole tree).
    #[tokio::test]
    async fn path_is_under_rejects_root_itself() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("agents");
        std::fs::create_dir_all(&root).unwrap();
        assert!(!path_is_under(&root, &root).await);
        assert!(path_is_under(&root.join("a").join("t"), &root).await);
        assert!(!path_is_under(&tmp.path().join("other"), &root).await);
    }
}
