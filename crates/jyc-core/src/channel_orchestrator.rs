use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use jyc_types::AppConfig;
use jyc_types::ChannelInfo;

use crate::thread_manager::ThreadManager;

/// Handle for a running channel, used by the orchestrator to manage lifecycle.
pub struct ChannelHandle {
    pub cancel: CancellationToken,
    pub task: tokio::task::JoinHandle<()>,
    pub thread_manager: Arc<ThreadManager>,
    pub channel_info: ChannelInfo,
    pub workspace_dir: std::path::PathBuf,
}

/// Manages the lifecycle of all channels (spawn, stop, reload).
///
/// When `reload()` is called, it diffs the new config against the running
/// channels and performs the minimal set of operations:
/// - **new channel** → `spawn_channel()`
/// - **removed channel** → cancel token, wait for task exit, cleanup
/// - **existing channel** → patterns are read dynamically by `MessageRouter`,
///   no restart needed for pattern-only changes. Connection parameter changes
///   (host, port, credentials) require restart — detected by comparing the
///   channel config hash.
pub struct ChannelOrchestrator {
    channels: Mutex<HashMap<String, ChannelHandle>>,
    config: Arc<ArcSwap<AppConfig>>,
    thread_managers: Arc<ArcSwap<Vec<Arc<ThreadManager>>>>,
    channel_infos: Arc<ArcSwap<Vec<ChannelInfo>>>,
    workspace_dirs: Arc<ArcSwap<Vec<std::path::PathBuf>>>,
    workdir: std::path::PathBuf,
}

impl ChannelOrchestrator {
    pub fn new(config: Arc<ArcSwap<AppConfig>>, workdir: &Path) -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            config,
            thread_managers: Arc::new(ArcSwap::from_pointee(Vec::new())),
            channel_infos: Arc::new(ArcSwap::from_pointee(Vec::new())),
            workspace_dirs: Arc::new(ArcSwap::from_pointee(Vec::new())),
            workdir: workdir.to_path_buf(),
        }
    }

    /// Shared view of thread managers for InspectContext.
    pub fn thread_managers(&self) -> Arc<ArcSwap<Vec<Arc<ThreadManager>>>> {
        self.thread_managers.clone()
    }

    /// Shared view of channel infos for InspectContext.
    pub fn channel_infos(&self) -> Arc<ArcSwap<Vec<ChannelInfo>>> {
        self.channel_infos.clone()
    }

    /// Shared view of workspace dirs for InspectContext.
    pub fn workspace_dirs(&self) -> Arc<ArcSwap<Vec<std::path::PathBuf>>> {
        self.workspace_dirs.clone()
    }

    /// Start all channels from the current config.
    pub async fn start_all(&self) -> anyhow::Result<()> {
        let cfg = self.config.load();
        for (name, channel_config) in &cfg.channels {
            if let Err(e) = self.spawn_channel(name, channel_config).await {
                tracing::error!(channel = %name, error = %e, "Failed to spawn channel");
            }
        }
        self.update_shared_state().await;
        Ok(())
    }

    /// Reload: diff current config against running channels and apply changes.
    pub async fn reload(&self) -> anyhow::Result<()> {
        let cfg = self.config.load();
        let mut channels = self.channels.lock().await;

        let old_names: std::collections::HashSet<String> = channels.keys().cloned().collect();
        let new_names: std::collections::HashSet<String> = cfg.channels.keys().cloned().collect();

        // Stop removed channels
        for name in old_names.difference(&new_names) {
            if let Some(handle) = channels.remove(name) {
                tracing::info!(channel = %name, "Stopping channel (removed from config)");
                handle.cancel.cancel();
                // Wait briefly for graceful shutdown
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    handle.task,
                )
                .await;
            }
        }

        // Start new channels
        for name in new_names.difference(&old_names) {
            if let Some(channel_config) = cfg.channels.get(name) {
                tracing::info!(channel = %name, "Starting new channel");
                drop(channels); // release lock before await
                if let Err(e) = self.spawn_channel(name, channel_config).await {
                    tracing::error!(channel = %name, error = %e, "Failed to spawn new channel");
                }
                channels = self.channels.lock().await;
            }
        }

        // For existing channels, patterns are read dynamically by MessageRouter.
        // Connection parameter changes require restart — not handled in this step
        // (they are treated as a separate concern and require a full stop+spawn).

        drop(channels);
        self.update_shared_state().await;
        Ok(())
    }

    /// Spawn a single channel. This is a placeholder — the full implementation
    /// extracts the per-channel spawn logic from `monitor.rs::run()`.
    async fn spawn_channel(
        &self,
        _name: &str,
        _channel_config: &jyc_types::ChannelConfig,
    ) -> anyhow::Result<()> {
        let _workdir = &self.workdir;
        // TODO: extract per-channel spawn logic from monitor.rs
        // This will be filled in during Step 2 integration.
        Ok(())
    }

    /// Update the shared ArcSwap views for InspectContext.
    async fn update_shared_state(&self) {
        let channels = self.channels.lock().await;
        let tms: Vec<Arc<ThreadManager>> = channels
            .values()
            .map(|h| h.thread_manager.clone())
            .collect();
        let infos: Vec<ChannelInfo> = channels
            .values()
            .map(|h| h.channel_info.clone())
            .collect();
        let dirs: Vec<std::path::PathBuf> = channels
            .values()
            .map(|h| h.workspace_dir.clone())
            .collect();
        drop(channels);

        self.thread_managers.store(Arc::new(tms));
        self.channel_infos.store(Arc::new(infos));
        self.workspace_dirs.store(Arc::new(dirs));

        tracing::info!(
            channel_count = self.thread_managers.load().len(),
            "Updated shared state after reload"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn test_config() -> Arc<ArcSwap<AppConfig>> {
        let config = AppConfig {
            general: jyc_types::GeneralConfig::default(),
            channels: HashMap::new(),
            agent: jyc_types::AgentConfig::default(),
            inspect: None,
            attachments: None,
            wecom: None,
            mcps: Vec::new(),
            scheduler: jyc_types::SchedulerConfig::default(),
        };
        Arc::new(ArcSwap::from_pointee(config))
    }

    #[tokio::test]
    async fn test_orchestrator_new_channels() {
        let tmpdir = TempDir::new().unwrap();
        let config = test_config();
        let orch = ChannelOrchestrator::new(config.clone(), tmpdir.path());

        // Initially empty
        let tms = orch.thread_managers.load();
        assert!(tms.is_empty());
    }

    #[tokio::test]
    async fn test_orchestrator_update_shared_state() {
        let tmpdir = TempDir::new().unwrap();
        let config = test_config();
        let orch = ChannelOrchestrator::new(config, tmpdir.path());

        // update_shared_state with empty channels should produce empty vecs
        orch.update_shared_state().await;

        let tms = orch.thread_managers.load();
        assert!(tms.is_empty());

        let infos = orch.channel_infos.load();
        assert!(infos.is_empty());
    }
}
