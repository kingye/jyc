use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore, broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::topic_event_bus::TopicEventBusRef;

use crate::agent::AgentService;
use crate::message_storage::MessageStorage;
use crate::metrics::MetricsHandle;
use jyc_types::{OutboundAdapter, QueueItem};

mod events;
mod git;
mod lifecycle;
mod queue;
mod template;
mod topics;
mod worker;

/// Per-topic queue stats.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct QueueStats {
    pub active_workers: usize,
    pub total_topics: usize,
    pub pending_messages: usize,
}

/// Manages per-topic message queues with bounded concurrency.
///
/// Responsible for:
/// - Queue management, concurrency control (semaphore + mpsc)
/// - Storing received messages (via chat log)
/// - Command processing (parse, execute, strip, reply results)
/// - Checking body emptiness (after commands + quoted history stripping)
/// - Dispatching to the agent service (via AgentService trait)
///
/// NOT responsible for: AI logic, sessions, prompts, reply building, sending —
/// those are owned by the AgentService implementation.
pub struct TopicManager {
    topic_queues: Mutex<HashMap<String, mpsc::Sender<QueueItem>>>,
    semaphore: Arc<Semaphore>,
    max_queue_size: usize,

    // Shared dependencies
    storage: Arc<MessageStorage>,
    outbound: Arc<dyn OutboundAdapter>,
    agent: Arc<dyn AgentService>,

    // Topic-isolated event buses (optional feature)
    event_buses: Mutex<HashMap<String, TopicEventBusRef>>,
    enable_events: bool,

    // Per-topic cancellation tokens (used by close_topic/cancel_topic to
    // stop workers). Shared via Arc so the per-worker TopicManager clone
    // sees the same map — otherwise /cancel and /close look up an empty map
    // and silently fail to cancel the running worker.
    pub(crate) topic_cancels: Arc<Mutex<HashMap<String, CancellationToken>>>,

    // Template directories for topic initialization (layered: L1 global < L2 workdir)
    template_dirs: crate::template_dirs::TemplateDirs,

    // Channel name this TopicManager belongs to
    channel_name: String,
    // Channel type (e.g., "email", "wecom_bot")
    channel_type: String,

    // Workdir (data root) for this channel.
    workdir: PathBuf,

    // Workspace directory for this channel (<workdir>/<channel>/workspace/)
    workspace_dir: PathBuf,

    // Application config (for command handlers that need channel/pattern info)
    config: Arc<ArcSwap<jyc_types::AppConfig>>,

    // Path to the config.toml file (for commands that write config, like /pin)
    pub(crate) config_path: Option<PathBuf>,

    // Metrics handle for reporting events to the inspect server
    pub(crate) metrics: MetricsHandle,

    cancel: CancellationToken,
    worker_handles: Mutex<Vec<JoinHandle<()>>>,

    // Custom topic paths (from pattern topic_path override), shared with
    // worker clones so list_topics() on the main TM sees paths from workers.
    pub(crate) topic_paths: Arc<Mutex<HashMap<String, PathBuf>>>,

    // Broadcast bus used by /gh to push live GitHub status events to the
    // dashboard. Set once after construction by the server bootstrap.
    // Wrapped in Arc so the per-worker TopicManager clone shares the same
    // sender without requiring OnceLock to be Clone.
    inspect_broadcast: Arc<std::sync::OnceLock<Arc<broadcast::Sender<String>>>>,
}

#[allow(dead_code)]
impl TopicManager {
    /// Create a new TopicManager with event support enabled by default.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        max_concurrent: usize,
        max_queue_size: usize,
        storage: Arc<MessageStorage>,
        outbound: Arc<dyn OutboundAdapter>,
        agent: Arc<dyn AgentService>,
        cancel: CancellationToken,
        template_dirs: impl Into<crate::template_dirs::TemplateDirs>,
        config: Arc<ArcSwap<jyc_types::AppConfig>>,
        channel_name: String,
        channel_type: String,
        workdir: PathBuf,
        workspace_dir: PathBuf,
        metrics: MetricsHandle,
    ) -> Self {
        Self::new_with_options(
            max_concurrent,
            max_queue_size,
            storage,
            outbound,
            agent,
            cancel,
            true,
            template_dirs,
            config,
            channel_name,
            channel_type,
            workdir,
            workspace_dir,
            metrics,
            None,
        )
    }

    /// Create a new TopicManager with configurable event support.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_options(
        max_concurrent: usize,
        max_queue_size: usize,
        storage: Arc<MessageStorage>,
        outbound: Arc<dyn OutboundAdapter>,
        agent: Arc<dyn AgentService>,
        cancel: CancellationToken,
        enable_events: bool,
        template_dirs: impl Into<crate::template_dirs::TemplateDirs>,
        config: Arc<ArcSwap<jyc_types::AppConfig>>,
        channel_name: String,
        channel_type: String,
        workdir: PathBuf,
        workspace_dir: PathBuf,
        metrics: MetricsHandle,
        config_path: Option<PathBuf>,
    ) -> Self {
        Self {
            topic_queues: Mutex::new(HashMap::new()),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_queue_size,
            storage,
            outbound,
            agent,
            event_buses: Mutex::new(HashMap::new()),
            enable_events,
            topic_cancels: Arc::new(Mutex::new(HashMap::new())),
            template_dirs: template_dirs.into(),
            channel_name,
            channel_type,
            workdir,
            workspace_dir,
            config,
            config_path,
            metrics,
            cancel: cancel.child_token(),
            worker_handles: Mutex::new(Vec::new()),
            topic_paths: Arc::new(Mutex::new(HashMap::new())),
            inspect_broadcast: Arc::new(std::sync::OnceLock::new()),
        }
    }

    pub async fn get_stats(&self) -> QueueStats {
        let queues = self.topic_queues.lock().await;
        let total_topics = queues.len();
        let active_workers = self.active_worker_count();
        QueueStats {
            active_workers,
            total_topics,
            pending_messages: 0,
        }
    }

    /// Return the channel name this TopicManager belongs to.
    pub fn channel_name(&self) -> &str {
        &self.channel_name
    }

    /// Return the channel type (e.g., "email", "wecom_bot").
    pub fn channel_type(&self) -> &str {
        &self.channel_type
    }

    /// Return the workdir (data root) for this channel.
    pub fn data_root(&self) -> &Path {
        &self.workdir
    }

    /// Return this channel's workspace directory (the parent of its
    /// default per-topic directories).
    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }

    /// Return the max concurrent topics (semaphore capacity).
    pub fn max_concurrent(&self) -> usize {
        self.semaphore.available_permits() + self.active_worker_count()
    }

    /// Number of active workers (holding semaphore permits).
    pub fn active_worker_count(&self) -> usize {
        // This is an approximation: semaphore total - available = active
        // We stored the capacity in the constructor but Semaphore doesn't expose it.
        // We use config's max_concurrent_topics as the total.
        self.config
            .load()
            .general
            .max_concurrent_topics
            .saturating_sub(self.semaphore.available_permits())
    }

    /// Set the inspect broadcast bus used by the dashboard. Called once
    /// during server startup; later calls are ignored.
    pub fn set_inspect_broadcast(&self, bus: Arc<broadcast::Sender<String>>) {
        let _ = self.inspect_broadcast.set(bus);
    }

    /// Return the inspect broadcast bus, if it has been wired up.
    pub fn inspect_broadcast(&self) -> Option<&Arc<broadcast::Sender<String>>> {
        self.inspect_broadcast.get()
    }
}
