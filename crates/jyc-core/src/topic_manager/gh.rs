//! `TopicManager` impl block: GitHub status watcher lifecycle.
//!
//! Provides `/gh on|off` support: a per-topic background task that runs
//! `gh pr status` and `gh run list --status in_progress` inside the topic
//! directory every 10 seconds and publishes the result as a dashboard
//! event via `inspect_broadcast`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use super::TopicManager;
use crate::gh_watcher::{GhSnapshot, fetch_snapshot};

const GH_POLL_INTERVAL: Duration = Duration::from_secs(10);

impl TopicManager {
    /// Start the GitHub status watcher for `topic` if it is not already running.
    ///
    /// Persists `enabled: true` to `<topic_path>/.jyc/gh-watcher.json`,
    /// fetches an initial snapshot, broadcasts it, and spawns the background
    /// poll loop. Returns the initial snapshot so the command reply can
    /// render it immediately.
    pub async fn start_gh_watcher(&self, topic: &str) -> Result<GhSnapshot> {
        let topic_path = self
            .topic_path(topic)
            .await
            .context("topic path not found")?;

        {
            let watchers = self.gh_watchers.lock().await;
            if watchers.contains_key(topic) {
                anyhow::bail!("GitHub status watcher already running for '{}'", topic);
            }
        }

        self.set_gh_watcher_enabled(topic, true).await?;

        let snapshot = fetch_snapshot(&topic_path, Path::new("gh")).await;
        self.broadcast_gh_status(topic, true, &snapshot);

        let token = CancellationToken::new();
        {
            let mut watchers = self.gh_watchers.lock().await;
            watchers.insert(topic.to_string(), token.clone());
        }

        let broadcast = self.inspect_broadcast();
        let channel = self.channel_name.clone();
        let topic_name = topic.to_string();
        let path = topic_path.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(GH_POLL_INTERVAL) => {
                        let snapshot = fetch_snapshot(&path, Path::new("gh")).await;
                        if let Some(ref bus) = broadcast {
                            let payload =
                                Self::build_gh_event(&channel, &topic_name, true, &snapshot);
                            let _ = bus.send(payload);
                        }
                    }
                }
            }
        });

        Ok(snapshot)
    }

    /// Stop the GitHub status watcher for `topic`.
    ///
    /// Persists `enabled: false` to disk and cancels the background poll
    /// loop. Broadcasts a disabled event so the dashboard clears the panel.
    pub async fn stop_gh_watcher(&self, topic: &str) -> Result<()> {
        self.set_gh_watcher_enabled(topic, false).await?;

        let token = {
            let mut watchers = self.gh_watchers.lock().await;
            watchers.remove(topic)
        };

        if let Some(token) = token {
            token.cancel();
            let snapshot = GhSnapshot::empty();
            self.broadcast_gh_status(topic, false, &snapshot);
            Ok(())
        } else {
            anyhow::bail!("GitHub status watcher not running for '{}'", topic);
        }
    }

    /// Resume a previously-enabled watcher on the first message for a topic.
    ///
    /// Idempotent: if a watcher is already running or the disk flag is false,
    /// this is a no-op.
    pub async fn maybe_resume_gh_watcher(&self, topic: &str) {
        if self.gh_watchers.lock().await.contains_key(topic) {
            return;
        }
        if self.gh_watcher_enabled_on_disk(topic).await {
            if let Err(e) = self.start_gh_watcher(topic).await {
                tracing::warn!(error = %e, topic, "failed to resume gh watcher");
            }
        }
    }

    /// Read the persisted watcher flag for `topic`.
    pub async fn gh_watcher_enabled_on_disk(&self, topic: &str) -> bool {
        if let Some(path) = self.gh_watcher_state_path(topic).await {
            if let Ok(bytes) = tokio::fs::read(&path).await {
                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    return json
                        .get("enabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                }
            }
        }
        false
    }

    fn broadcast_gh_status(&self, topic: &str, enabled: bool, snapshot: &GhSnapshot) {
        if let Some(bus) = self.inspect_broadcast() {
            let payload = Self::build_gh_event(&self.channel_name, topic, enabled, snapshot);
            let _ = bus.send(payload);
        }
    }

    pub(crate) fn build_gh_event(
        channel: &str,
        topic: &str,
        enabled: bool,
        snapshot: &GhSnapshot,
    ) -> String {
        serde_json::json!({
            "type": "gh_status",
            "channel": channel,
            "topic": topic,
            "enabled": enabled,
            "snapshot": snapshot,
        })
        .to_string()
    }

    async fn gh_watcher_state_path(&self, topic: &str) -> Option<PathBuf> {
        self.topic_path(topic)
            .await
            .map(|p| p.join(".jyc").join("gh-watcher.json"))
    }

    async fn set_gh_watcher_enabled(&self, topic: &str, enabled: bool) -> Result<()> {
        let path = self
            .gh_watcher_state_path(topic)
            .await
            .context("topic path not found")?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let json = serde_json::json!({ "enabled": enabled });
        tokio::fs::write(&path, serde_json::to_string_pretty(&json)?).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentService;
    use crate::message_storage::MessageStorage;
    use crate::metrics::MetricsCollector;
    use crate::static_agent::StaticAgentService;
    use crate::topic_manager::TopicManager;
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

    fn make_topic_manager(workspace: &std::path::Path) -> TopicManager {
        let storage = Arc::new(MessageStorage::new(workspace));
        let cancel = CancellationToken::new();
        let metrics_cancel = CancellationToken::new();
        let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();
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

        TopicManager::new_with_options(
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
        )
    }

    #[tokio::test]
    async fn test_gh_watcher_persistence_roundtrip() {
        let tmp = tempdir().unwrap();
        let tm = make_topic_manager(tmp.path());
        let topic_dir = tmp.path().join("my-topic");
        tokio::fs::create_dir_all(&topic_dir).await.unwrap();

        assert!(!tm.gh_watcher_enabled_on_disk("my-topic").await);

        tm.set_gh_watcher_enabled("my-topic", true).await.unwrap();
        assert!(tm.gh_watcher_enabled_on_disk("my-topic").await);

        tm.set_gh_watcher_enabled("my-topic", false).await.unwrap();
        assert!(!tm.gh_watcher_enabled_on_disk("my-topic").await);
    }

    #[tokio::test]
    async fn test_gh_watcher_start_stop() {
        let tmp = tempdir().unwrap();
        let tm = make_topic_manager(tmp.path());
        let topic_dir = tmp.path().join("my-topic");
        tokio::fs::create_dir_all(&topic_dir).await.unwrap();

        // gh is not installed in CI, so start should fail at fetch but the
        // watcher token should still be registered and then stoppable.
        let start_result = tm.start_gh_watcher("my-topic").await;
        assert!(start_result.is_err() || start_result.unwrap().error.is_some());

        // Token should exist even when gh fails; stop must not error.
        tm.stop_gh_watcher("my-topic").await.unwrap();

        // Second stop should fail.
        assert!(tm.stop_gh_watcher("my-topic").await.is_err());
    }

    #[test]
    fn test_build_gh_event() {
        let snap = GhSnapshot::empty();
        let json = TopicManager::build_gh_event("test", "my-topic", true, &snap);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "gh_status");
        assert_eq!(value["channel"], "test");
        assert_eq!(value["topic"], "my-topic");
        assert_eq!(value["enabled"], true);
    }
}
