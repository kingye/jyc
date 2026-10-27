use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore, mpsc};
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
mod live_status;
mod managers;
mod queue;
mod template;
pub mod topic_tree;
mod topics;
mod worker;

pub use lifecycle::PurgeOutcome;
pub use live_status::LiveStatus;
pub use managers::TopicManagers;
pub use queue::QueueFull;
pub use topics::TopicDisplayState;

/// Per-topic queue stats.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct QueueStats {
    pub active_workers: usize,
    pub total_topics: usize,
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

    // Template directories for topic initialization (layered: L1 global < L2 data_dir)
    template_dirs: crate::template_dirs::TemplateDirs,

    // Channel name this TopicManager belongs to
    channel_name: String,
    // Channel type (e.g., "email", "wecom_bot")
    channel_type: String,

    // Workdir (data root) for this channel.
    data_dir: PathBuf,

    // Workspace directory for this channel (<data dir>/<channel>/workspace/)
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

    // Authoritative source for "what pattern/agent does this topic belong
    // to". Worker writes after every processed message; readers (incl.
    // the inspect server's `list_topics`) read this first and fall back
    // to `.jyc/pattern` on disk for cold-start entries that haven't been
    // touched this session. Shared via Arc so worker clones (which are
    // drop-in TMs) write to the same map the main TM reads from. Uses
    // `Mutex` to match the existing `topic_paths` field above; reads
    // and writes here are infrequent (one per processed message, one
    // per overview poll), so the read-parallelism of `RwLock` is not
    // worth the extra type complexity.
    pub(crate) topic_patterns: Arc<Mutex<HashMap<String, String>>>,

    // Per-topic live status — the single source of truth for `TopicStatus`.
    // The runtime writes it as turns start and end (see `live_status.rs`),
    // and every reader — `list_topics`, `topic_info`, the inspect server —
    // reports the value it holds. Shared via Arc so the worker clones that
    // write it and the main manager that reads it see the same map.
    pub(crate) live_status: Arc<live_status::LiveStatusStore>,

    // The set of managers this one is part of, for resolving a topic name to
    // the manager that serves it (see `managers.rs`). Weak: the registry holds
    // the managers, so a strong pointer here would be an Arc cycle. Set once,
    // by whoever builds the managers (the orchestrator); unset in tests that
    // only exercise one manager.
    //
    // An `Arc` so that the cell is shared, not copied: a worker clone taken
    // before the wiring still sees it afterwards, which keeps the resolution a
    // command handler depends on from hinging on the order of the two.
    managers: Arc<std::sync::OnceLock<std::sync::Weak<TopicManagers>>>,

    /// Test/override hook for the central billing ledger directory.
    /// When unset, `billing_dir()` falls back to
    /// `BillingLogStore::billing_dir()` (`<data dir>/billing`).
    billing_dir_override: std::sync::OnceLock<PathBuf>,
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
        data_dir: PathBuf,
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
            data_dir,
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
        data_dir: PathBuf,
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
            data_dir,
            workspace_dir,
            config,
            config_path,
            metrics,
            cancel: cancel.child_token(),
            worker_handles: Mutex::new(Vec::new()),
            topic_paths: Arc::new(Mutex::new(HashMap::new())),
            topic_patterns: Arc::new(Mutex::new(HashMap::new())),
            live_status: Arc::new(live_status::LiveStatusStore::default()),
            managers: Arc::new(std::sync::OnceLock::new()),
            billing_dir_override: std::sync::OnceLock::new(),
        }
    }

    pub async fn get_stats(&self) -> QueueStats {
        let queues = self.topic_queues.lock().await;
        let total_topics = queues.len();
        let active_workers = self.active_worker_count();
        QueueStats {
            active_workers,
            total_topics,
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

    /// Point this manager at the set it belongs to, so it can resolve a topic
    /// name to the manager that serves it. Called by whoever builds the
    /// managers (`ChannelOrchestrator::update_shared_state`); later calls keep
    /// the first wiring, so a reload does not re-point anything.
    pub fn set_managers(&self, managers: &Arc<TopicManagers>) {
        let _ = self.managers.set(Arc::downgrade(managers));
    }

    /// The set of managers this one is part of, when it has been wired up.
    pub fn managers(&self) -> Option<Arc<TopicManagers>> {
        self.managers.get().and_then(|weak| weak.upgrade())
    }

    /// Whether this manager currently runs `topic_name` — it holds a queue for
    /// it. The queue is created by the first `enqueue` for a topic, so this
    /// answers "is the topic being served here right now", which is what
    /// `TopicManagers::serving` resolves an address against.
    pub async fn serves_topic(&self, topic_name: &str) -> bool {
        self.topic_queues.lock().await.contains_key(topic_name)
    }

    /// Return the data_dir (data root) for this channel.
    pub fn data_root(&self) -> &Path {
        &self.data_dir
    }

    /// Override the central billing ledger directory (tests).
    pub fn set_billing_dir(&self, dir: PathBuf) {
        let _ = self.billing_dir_override.set(dir);
    }

    /// Central billing ledger directory: the override when set, else
    /// `<data dir>/billing` (`None` when no data home is resolvable).
    pub(crate) fn billing_dir(&self) -> Option<PathBuf> {
        self.billing_dir_override
            .get()
            .cloned()
            .or_else(crate::billing_log_store::BillingLogStore::billing_dir)
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
}
