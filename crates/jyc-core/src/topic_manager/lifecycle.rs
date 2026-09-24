//! `TopicManager` impl block: lifecycle.rs methods.
//!
//! Extracted from the monolithic `topic_manager.rs`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

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
    /// For adopted/pinned topics (a registered state dir outside the topic
    /// dir), the *state dir* is deleted and unregistered while the topic
    /// dir itself — typically a user-owned project checkout — is kept.
    /// Unregistered topics keep the legacy behavior: the whole directory
    /// (with its in-dir `.jyc`) is removed. In-memory state is cleaned up
    /// in both cases.
    pub async fn close_topic(&self, topic_name: &str) -> Result<()> {
        let topic_path = self
            .topic_path(topic_name)
            .await
            .unwrap_or_else(|| self.storage.workspace().join(topic_name));

        // Pinned topic: relocated state is the topic's data; the dir is
        // user property. Remove the former, preserve the latter. The
        // lookup is by *name*, so topics co-pinning the same dir never
        // destroy each other's state. Name-keyed registrations always
        // point outside the topic dir (adopt computes them under
        // `state_root`), so no same-dir guard is needed.
        if let Some(state) = jyc_types::state_dir::registered_state(topic_name) {
            if state.exists() {
                tokio::fs::remove_dir_all(&state)
                    .await
                    .context(format!("Failed to remove topic state dir: {:?}", state))?;
            }
            jyc_types::state_dir::unregister(topic_name);
            tracing::info!(
                topic = %topic_name,
                state = %state.display(),
                topic_dir = %topic_path.display(),
                "Topic state dir deleted; pinned topic dir preserved"
            );
            self.cleanup_topic_state(topic_name).await;
            return Ok(());
        }

        if topic_path.exists() {
            remove_topic_dir(topic_name, &topic_path).await?;
            tracing::info!(topic = %topic_name, "Topic directory deleted");
        }

        self.cleanup_topic_state(topic_name).await;
        Ok(())
    }

    /// Delete the directory a pinned/cloned topic works in — the extra step
    /// behind `/close --force --purge`.
    ///
    /// [`Self::close_topic`] deliberately keeps that directory: for `/fork` and
    /// `/clone` siblings it is *someone else's* — a shared checkout, or a copy
    /// the user made on purpose. Purge is the explicit "and this one too".
    ///
    /// Refuses (deletes nothing) when the dir is not this topic's to remove:
    ///
    /// - another topic works in it — pinned or not: a fork shares its
    ///   parent's dir, and the parent may never have been pinned, so the
    ///   workspace itself is consulted, not just the runtime pin map,
    /// - another topic's dir lives inside it, so deleting it takes them along,
    /// - it *is* one of jyc's own roots, contains one, or sits directly under
    ///   one holding another topic's state (`agents/<name>/.jyc`).
    pub async fn purge_topic_dir(&self, topic_name: &str) -> Result<PurgeOutcome> {
        let agents_root = self.agents_workspace_root();
        let state_root = crate::topic_path::state_root(&self.workdir);
        self.purge_topic_dir_under(topic_name, &agents_root, &state_root)
            .await
    }

    /// Every other topic's working dir: the runtime pins plus whatever the
    /// workspace itself holds. Pins alone cannot answer "who else works
    /// here?" — an unpinned topic lives at `<workspace>/<name>` and a fork
    /// shares exactly that dir.
    async fn other_topic_dirs(&self, self_name: &str) -> Vec<(String, PathBuf)> {
        let mut dirs: Vec<(String, PathBuf)> = self
            .custom_topic_paths()
            .await
            .into_iter()
            .filter(|(name, _)| name != self_name)
            .collect();
        if let Ok(entries) = std::fs::read_dir(self.storage.workspace()) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name != self_name && entry.path().is_dir() {
                    dirs.push((name, entry.path()));
                }
            }
        }
        dirs
    }

    /// Testable core of [`Self::purge_topic_dir`] with jyc's own roots passed
    /// in, so no test can reach the real data home (mirrors
    /// [`Self::auto_close_topic_under`]).
    async fn purge_topic_dir_under(
        &self,
        topic_name: &str,
        agents_root: &Path,
        state_root: &Path,
    ) -> Result<PurgeOutcome> {
        // Not pinned: the dir is inside the workspace and `close_topic` already
        // deletes it, so there is nothing extra to do.
        let Some(topic_path) = self.topic_path(topic_name).await else {
            return Ok(PurgeOutcome::Nothing);
        };
        if !topic_path.exists() {
            return Ok(PurgeOutcome::Nothing);
        }
        let path = crate::topic_path::resolved(&topic_path);

        let mut shared = Vec::new();
        let mut nested = Vec::new();
        for (name, pin) in self.other_topic_dirs(topic_name).await {
            if crate::topic_path::resolved(&pin) == path {
                shared.push(name);
            } else if path_is_under(&pin, &path).await {
                nested.push(name);
            }
        }
        if !shared.is_empty() {
            return Ok(PurgeOutcome::Refused(format!(
                "{} is also the topic dir of {} — deleting it would take that topic with it.",
                path.display(),
                shared.join(", ")
            )));
        }
        if !nested.is_empty() {
            return Ok(PurgeOutcome::Refused(format!(
                "{} contains the topic dir of {} — deleting it would take that topic with it.",
                path.display(),
                nested.join(", ")
            )));
        }
        for root in [agents_root, state_root] {
            let root = crate::topic_path::resolved(root);
            if path.parent().is_some_and(|p| p == root.as_path()) {
                return Ok(PurgeOutcome::Refused(format!(
                    "{} is a state namespace — it holds another topic's `.jyc`, not a workspace.",
                    path.display()
                )));
            }
            if root.starts_with(&path) {
                return Ok(PurgeOutcome::Refused(format!(
                    "{} is jyc's own directory ({} lives in it) — it is not a topic dir.",
                    path.display(),
                    root.display()
                )));
            }
        }

        remove_topic_dir(topic_name, &path).await?;
        tracing::info!(topic = %topic_name, dir = %path.display(), "Topic directory purged");
        Ok(PurgeOutcome::Deleted)
    }

    /// The root that holds every agent's topic subtree (`<data_home>/agents/`).
    ///
    /// A forked topic lands at `<root>/<agent>/<name>` — the same place the
    /// router puts runtime topics, and the same boundary
    /// [`Self::auto_close_topic`] refuses to delete above.
    pub fn agents_workspace_root(&self) -> std::path::PathBuf {
        crate::topic_path::resolve_agents_workspace_root(&self.workdir)
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
        let agents_root = self.agents_workspace_root();
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
    let path = crate::topic_path::resolved(path);
    let root = crate::topic_path::resolved(root);
    path != root && path.starts_with(&root)
}

/// What [`TopicManager::purge_topic_dir`] did, or refused to do.
#[derive(Debug)]
pub enum PurgeOutcome {
    /// Nothing on disk to delete (the topic was never pinned, or the dir is
    /// already gone). `close_topic` still has the state to remove.
    Nothing,
    /// The topic dir was deleted.
    Deleted,
    /// Nothing was deleted; the message says why, for the user.
    Refused(String),
}

/// Delete a topic directory: the `repo` symlink first, then the tree.
///
/// `remove_dir_all` follows symlinks, so a dir holding a link into a shared
/// checkout would delete through it. The link goes first; what remains is a
/// plain tree.
async fn remove_topic_dir(topic_name: &str, path: &Path) -> Result<()> {
    let repo_symlink = path.join("repo");
    if let Ok(meta) = tokio::fs::symlink_metadata(&repo_symlink).await
        && meta.file_type().is_symlink()
    {
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

    tokio::fs::remove_dir_all(path)
        .await
        .context(format!("Failed to remove topic directory: {:?}", path))?;
    Ok(())
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

    /// Pinned topic with a registered state dir: close deletes the state
    /// dir and unregisters, but the topic dir (user-owned repo) survives.
    #[tokio::test]
    async fn close_pinned_topic_removes_state_keeps_dir() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let repo = tmp.path().join("probe-pin-repo");
        let state = tmp.path().join("agents/probe-pin-app/.jyc");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("main.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("chat.jsonl"), "{}").unwrap();
        jyc_types::state_dir::register("probe-pin-app", &state);
        // set_topic_path skips adoption when already registered
        tm.set_topic_path("probe-pin-app", repo.clone())
            .await
            .unwrap();

        tm.close_topic("probe-pin-app").await.unwrap();

        assert!(
            repo.join("main.rs").exists(),
            "pinned topic dir must survive"
        );
        assert!(
            !repo.join(".jyc").exists(),
            "no state should be recreated in dir"
        );
        assert!(!state.exists(), "relocated state dir must be deleted");
        assert!(jyc_types::state_dir::registered_state("probe-pin-app").is_none());
        assert_eq!(
            jyc_types::state_dir::jyc_dir("probe-pin-app", &repo),
            repo.join(".jyc")
        );
    }

    /// Two topics pinning the same dir: closing one deletes only its own
    /// name-keyed state; the sibling's state and the shared dir survive.
    #[tokio::test]
    async fn close_co_pinned_topic_only_deletes_own_state() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let repo = tmp.path().join("probe-co-pin");
        std::fs::create_dir_all(&repo).unwrap();
        let state_a = tmp.path().join("agents/probe-co-a/.jyc");
        let state_b = tmp.path().join("agents/probe-co-b/.jyc");
        std::fs::create_dir_all(&state_a).unwrap();
        std::fs::create_dir_all(&state_b).unwrap();
        jyc_types::state_dir::register("probe-co-a", &state_a);
        jyc_types::state_dir::register("probe-co-b", &state_b);
        tm.set_topic_path("probe-co-a", repo.clone()).await.unwrap();

        tm.close_topic("probe-co-a").await.unwrap();

        assert!(!state_a.exists(), "closed topic's state gone");
        assert!(state_b.exists(), "sibling state untouched");
        assert!(repo.exists(), "shared topic dir untouched");
        assert!(jyc_types::state_dir::registered_state("probe-co-b").is_some());
    }

    /// Unregistered topic dirs (workspace/dynamic) keep the legacy
    /// behavior: everything is deleted.
    #[tokio::test]
    async fn close_unregistered_topic_deletes_dir() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let topic_dir = workspace.join("probe-plain-topic");
        std::fs::create_dir_all(topic_dir.join(".jyc")).unwrap();
        std::fs::write(topic_dir.join("f.txt"), "x").unwrap();

        tm.close_topic("probe-plain-topic").await.unwrap();
        assert!(!topic_dir.exists());
    }

    /// `/close --force --purge` on a pinned topic: the state *and* the dir the
    /// topic worked in go, which is the one thing `close_topic` refuses to do.
    #[tokio::test]
    async fn purge_deletes_a_pinned_topic_dir() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let repo = tmp.path().join("probe-purge-repo");
        let state = tmp.path().join("agents/probe-purge/.jyc");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("main.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(&state).unwrap();
        jyc_types::state_dir::register("probe-purge", &state);
        tm.set_topic_path("probe-purge", repo.clone())
            .await
            .unwrap();

        let outcome = tm
            .purge_topic_dir_under(
                "probe-purge",
                &tmp.path().join("agents"),
                &tmp.path().join("state"),
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, PurgeOutcome::Deleted),
            "the pinned dir is what purge deletes"
        );
        assert!(!repo.exists(), "purge is the explicit 'and this dir too'");
    }

    /// Two topics on one dir: purging from either one refuses — the other
    /// topic's workspace is not on the table.
    #[tokio::test]
    async fn purge_refuses_a_dir_another_topic_also_uses() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let shared = tmp.path().join("probe-shared-repo");
        std::fs::create_dir_all(&shared).unwrap();
        let state = tmp.path().join("agents/probe-one/.jyc");
        std::fs::create_dir_all(&state).unwrap();
        jyc_types::state_dir::register("probe-one", &state);
        tm.set_topic_path("probe-one", shared.clone())
            .await
            .unwrap();
        tm.set_topic_path("probe-two", shared.clone())
            .await
            .unwrap();

        let outcome = tm
            .purge_topic_dir_under(
                "probe-one",
                &tmp.path().join("agents"),
                &tmp.path().join("state"),
            )
            .await
            .unwrap();
        let PurgeOutcome::Refused(reason) = outcome else {
            panic!("a co-pinned dir must be refused, got {outcome:?}");
        };
        assert!(reason.contains("probe-two"), "{reason}");
        assert!(shared.exists(), "nothing may be deleted on a refusal");
    }

    /// A topic dir that contains another topic's dir is refused: deleting it
    /// would take the nested topic with it.
    #[tokio::test]
    async fn purge_refuses_a_dir_that_contains_another_topic() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let outer = tmp.path().join("probe-outer");
        let inner = outer.join("inner-project");
        std::fs::create_dir_all(&inner).unwrap();
        tm.set_topic_path("probe-outer", outer.clone())
            .await
            .unwrap();
        tm.set_topic_path("probe-inner", inner.clone())
            .await
            .unwrap();

        let outcome = tm
            .purge_topic_dir_under(
                "probe-outer",
                &tmp.path().join("agents"),
                &tmp.path().join("state"),
            )
            .await
            .unwrap();
        let PurgeOutcome::Refused(reason) = outcome else {
            panic!("an ancestor of another topic dir must be refused, got {outcome:?}");
        };
        assert!(reason.contains("probe-inner"), "{reason}");
        assert!(outer.exists());
    }

    /// A fork shares its parent's workspace dir, and the parent may never have
    /// been pinned — pins alone cannot see it, so the workspace scan has to.
    #[tokio::test]
    async fn purge_refuses_a_dir_a_workspace_topic_also_uses() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let parent_dir = workspace.join("probe-parent");
        std::fs::create_dir_all(parent_dir.join(".jyc")).unwrap();
        std::fs::write(parent_dir.join("code.rs"), "x").unwrap();
        // The fork adopts the parent's in-dir state out of the parent dir —
        // exactly what /fork does — so no `.jyc` remains there to detect by.
        let fork_state = tmp.path().join("agents/probe-fork/.jyc");
        std::fs::create_dir_all(&fork_state).unwrap();
        jyc_types::state_dir::register("probe-fork", &fork_state);
        tm.set_topic_path("probe-fork", parent_dir.clone())
            .await
            .unwrap();

        let outcome = tm
            .purge_topic_dir_under(
                "probe-fork",
                &tmp.path().join("agents"),
                &tmp.path().join("state"),
            )
            .await
            .unwrap();
        let PurgeOutcome::Refused(reason) = outcome else {
            panic!("the parent's workspace dir must be refused, got {outcome:?}");
        };
        assert!(reason.contains("probe-parent"), "{reason}");
        assert!(parent_dir.exists());
        jyc_types::state_dir::unregister("probe-fork");
    }

    /// A direct child of an agents/state root is a state namespace
    /// (`agents/jin/.jyc` is another topic's data), not a topic dir.
    #[tokio::test]
    async fn purge_refuses_a_state_namespace() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let agents_root = tmp.path().join("agents");
        let namespace = agents_root.join("jin");
        std::fs::create_dir_all(namespace.join(".jyc")).unwrap();
        tm.set_topic_path("probe-ns", namespace.clone())
            .await
            .unwrap();

        let outcome = tm
            .purge_topic_dir_under("probe-ns", &agents_root, &tmp.path().join("state"))
            .await
            .unwrap();
        let PurgeOutcome::Refused(reason) = outcome else {
            panic!("a state namespace must be refused, got {outcome:?}");
        };
        assert!(reason.contains("state namespace"), "{reason}");
        assert!(namespace.exists());
        jyc_types::state_dir::unregister("probe-ns");
    }

    /// jyc's own directories are not topic dirs, however a topic got pinned to
    /// one: the agents tree has to survive, and so does the state root.
    #[tokio::test]
    async fn purge_refuses_jycs_own_directories() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let agents_root = tmp.path().join("agents");
        let state_root = tmp.path().join("state").join("agents");
        for (name, pin) in [
            ("probe-agents-root", agents_root.clone()),
            ("probe-data-home", tmp.path().to_path_buf()),
        ] {
            std::fs::create_dir_all(&pin).unwrap();
            tm.set_topic_path(name, pin.clone()).await.unwrap();

            let outcome = tm
                .purge_topic_dir_under(name, &agents_root, &state_root)
                .await
                .unwrap();
            assert!(
                matches!(outcome, PurgeOutcome::Refused(_)),
                "{name} pins {} which holds jyc's own dirs, got {outcome:?}",
                pin.display()
            );
            assert!(pin.exists(), "{name}: nothing may be deleted on a refusal");
        }
    }

    /// A topic that was never pinned keeps its old behavior — the dir lives in
    /// the workspace and `close_topic` deletes it, so purge has nothing to add.
    #[tokio::test]
    async fn purge_leaves_an_unpinned_topic_to_close() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let topic_dir = workspace.join("probe-plain");
        std::fs::create_dir_all(&topic_dir).unwrap();

        let outcome = tm
            .purge_topic_dir_under(
                "probe-plain",
                &tmp.path().join("agents"),
                &tmp.path().join("state"),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, PurgeOutcome::Nothing), "{outcome:?}");
        assert!(topic_dir.exists(), "purge itself must not guess a dir");
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
