use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use jyc_types::AppConfig;
use jyc_types::ChannelInfo;

use crate::topic_manager::TopicManager;

/// Handle for a running channel, used by the orchestrator to manage lifecycle.
pub struct ChannelHandle {
    pub cancel: CancellationToken,
    pub topic_manager: Arc<TopicManager>,
    pub channel_info: ChannelInfo,
    pub workspace_dir: std::path::PathBuf,
}

/// Manages the lifecycle of all channels and shared state updates.
///
/// When `reload()` is called, it diffs the new config against the running
/// channels and performs the minimal set of operations:
/// - **removed channel** → cancel token, wait for task exit, cleanup
/// - **new channel** → logs warning that restart is required (full spawn
///   requires restart of the monitor process)
/// - **existing channel** → patterns are read dynamically by `MessageRouter`,
///   no restart needed for pattern-only changes.
pub struct ChannelOrchestrator {
    channels: Mutex<HashMap<String, ChannelHandle>>,
    /// Agent-keyed TopicManagers (migration PR-5): not channels, so they are
    /// exempt from the reload diff — registered for shared state only.
    agents: Mutex<Vec<ChannelHandle>>,
    config: Arc<ArcSwap<AppConfig>>,
    topic_managers: Arc<ArcSwap<Vec<Arc<TopicManager>>>>,
    channel_infos: Arc<ArcSwap<Vec<ChannelInfo>>>,
    workspace_dirs: Arc<ArcSwap<Vec<std::path::PathBuf>>>,
}

impl ChannelOrchestrator {
    pub fn new(config: Arc<ArcSwap<AppConfig>>, _workdir: &Path) -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            agents: Mutex::new(Vec::new()),
            config,
            topic_managers: Arc::new(ArcSwap::from_pointee(Vec::new())),
            channel_infos: Arc::new(ArcSwap::from_pointee(Vec::new())),
            workspace_dirs: Arc::new(ArcSwap::from_pointee(Vec::new())),
        }
    }

    /// Shared view of topic managers for InspectContext.
    pub fn topic_managers(&self) -> Arc<ArcSwap<Vec<Arc<TopicManager>>>> {
        self.topic_managers.clone()
    }

    /// Shared view of channel infos for InspectContext.
    pub fn channel_infos(&self) -> Arc<ArcSwap<Vec<ChannelInfo>>> {
        self.channel_infos.clone()
    }

    /// Shared view of workspace dirs for InspectContext.
    pub fn workspace_dirs(&self) -> Arc<ArcSwap<Vec<std::path::PathBuf>>> {
        self.workspace_dirs.clone()
    }

    /// Register a running channel with the orchestrator.
    pub async fn register_channel(&self, name: String, handle: ChannelHandle) {
        let mut channels = self.channels.lock().await;
        channels.insert(name, handle);
        drop(channels);
        self.update_shared_state().await;
    }

    /// Register an agent-keyed TopicManager (migration PR-5).
    ///
    /// Agents are not channels: they appear in the shared TopicManager /
    /// ChannelInfo views (inspect, scheduler, cross-topic injection) but are
    /// exempt from the reload diff. Their topics dirs are NOT added to
    /// `workspace_dirs` — the scheduler finds them via the per-TopicManager
    /// scan (custom topic paths), which avoids deriving a wrong channel name
    /// from the `agents/<agent>` parent.
    pub async fn register_agent(&self, handle: ChannelHandle) {
        let mut agents = self.agents.lock().await;
        agents.push(handle);
        drop(agents);
        self.update_shared_state().await;
    }

    /// Reload: diff current config against running channels and apply changes.
    pub async fn reload(&self) -> anyhow::Result<()> {
        let cfg = self.config.load();
        let mut channels = self.channels.lock().await;

        let old_names: std::collections::HashSet<String> = channels.keys().cloned().collect();
        let new_names: std::collections::HashSet<String> = cfg.channels.keys().cloned().collect();

        // Stop removed channels: cancel the per-channel token, then give workers
        // up to 5s to exit gracefully (via the cancel token). The inbound task
        // should shut down its adapter and call TopicManager::shutdown().
        for name in old_names.difference(&new_names) {
            if let Some(handle) = channels.remove(name) {
                tracing::info!(channel = %name, "Stopping channel (removed from config)");
                handle.cancel.cancel();
                // Allow time for the task to see the cancellation and shut down.
                // The task is responsible for cleaning up (adapter disconnect,
                // TopicManager shutdown, etc.) after the token fires.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }

        // New channels require restart
        for name in new_names.difference(&old_names) {
            tracing::warn!(
                channel = %name,
                "New channel detected after reload — restart jyc serve to activate"
            );
        }

        // For existing channels, patterns are read dynamically by MessageRouter.
        // Connection parameter changes require restart — not handled here
        // (they are treated as a separate concern and require a full stop+spawn).

        drop(channels);
        self.update_shared_state().await;
        Ok(())
    }

    /// Update the shared ArcSwap views for InspectContext.
    async fn update_shared_state(&self) {
        let channels = self.channels.lock().await;
        let agents = self.agents.lock().await;
        let tms: Vec<Arc<TopicManager>> = channels
            .values()
            .map(|h| h.topic_manager.clone())
            .chain(agents.iter().map(|h| h.topic_manager.clone()))
            .collect();
        let infos: Vec<ChannelInfo> = channels
            .values()
            .map(|h| h.channel_info.clone())
            .chain(agents.iter().map(|h| h.channel_info.clone()))
            .collect();
        let dirs: Vec<std::path::PathBuf> =
            channels.values().map(|h| h.workspace_dir.clone()).collect();
        drop(channels);
        drop(agents);

        self.topic_managers.store(Arc::new(tms));
        self.channel_infos.store(Arc::new(infos));
        self.workspace_dirs.store(Arc::new(dirs));

        tracing::info!(
            channel_count = self.topic_managers.load().len(),
            "Updated shared state after reload"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config() -> Arc<ArcSwap<AppConfig>> {
        let config: AppConfig = toml::from_str(
            r#"
[agent]
mode = "agent"
system_prompt = "test"

[agent.providers.test]
type = "openai-compatible"
base_url = "http://test"
api_key_env = "TEST_KEY"
"#,
        )
        .unwrap();
        Arc::new(ArcSwap::from_pointee(config))
    }

    #[tokio::test]
    async fn test_orchestrator_new_channels() {
        let tmpdir = TempDir::new().unwrap();
        let config = test_config();
        let orch = ChannelOrchestrator::new(config.clone(), tmpdir.path());

        // Initially empty
        let tms = orch.topic_managers.load();
        assert!(tms.is_empty());
    }

    #[tokio::test]
    async fn test_orchestrator_update_shared_state() {
        let tmpdir = TempDir::new().unwrap();
        let config = test_config();
        let orch = ChannelOrchestrator::new(config, tmpdir.path());

        // update_shared_state with empty channels should produce empty vecs
        orch.update_shared_state().await;

        let tms = orch.topic_managers.load();
        assert!(tms.is_empty());

        let infos = orch.channel_infos.load();
        assert!(infos.is_empty());
    }

    #[tokio::test]
    async fn test_orchestrator_reload_detects_new_channels() {
        let tmpdir = TempDir::new().unwrap();
        let config = test_config();
        let orch = ChannelOrchestrator::new(config.clone(), tmpdir.path());

        // Reload with no changes should succeed
        orch.reload().await.unwrap();

        let tms = orch.topic_managers.load();
        assert!(tms.is_empty());
    }
}
