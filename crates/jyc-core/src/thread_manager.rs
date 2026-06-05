use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::thread_event::ThreadEvent;
use crate::thread_event_bus::{SimpleThreadEventBus, ThreadEventBusRef};

use crate::agent::AgentService;
use crate::command::close_handler::CloseCommandHandler;
use crate::command::handler::CommandContext;
use crate::command::mode_handler::{BuildCommandHandler, PlanCommandHandler};
use crate::command::registry::CommandRegistry;
use crate::command::reset_handler::ResetCommandHandler;
use crate::command::template_handler::TemplateCommandHandler;
use crate::message_storage::{MessageStorage, StoreResult};
use crate::metrics::MetricsHandle;
use crate::pending_delivery::watch_pending_deliveries;
use crate::template_utils::copy_template_files;
use crate::thread_json::ThreadJson;
use jyc_types::InboundAttachmentConfig;
use jyc_types::{InboundMessage, OutboundAdapter, PatternMatch, QueueItem};
use jyc_types::{ThreadInfo, ThreadStatus};

/// Per-thread queue stats.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct QueueStats {
    pub active_workers: usize,
    pub total_threads: usize,
    pub pending_messages: usize,
}

/// Manages per-thread message queues with bounded concurrency.
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
pub struct ThreadManager {
    thread_queues: Mutex<HashMap<String, mpsc::Sender<QueueItem>>>,
    semaphore: Arc<Semaphore>,
    max_queue_size: usize,

    // Shared dependencies
    storage: Arc<MessageStorage>,
    outbound: Arc<dyn OutboundAdapter>,
    agent: Arc<dyn AgentService>,

    // Thread-isolated event buses (optional feature)
    event_buses: Mutex<HashMap<String, ThreadEventBusRef>>,
    enable_events: bool,

    // Per-thread cancellation tokens (used by close_thread to stop workers)
    pub(crate) thread_cancels: Mutex<HashMap<String, CancellationToken>>,

    // Template directory for thread initialization
    template_dir: PathBuf,

    // Channel name this ThreadManager belongs to
    channel_name: String,

    // Workspace directory for this channel (<workdir>/<channel>/workspace/)
    workspace_dir: PathBuf,

    // Application config (for command handlers that need channel/pattern info)
    config: Arc<ArcSwap<jyc_types::AppConfig>>,

    // Metrics handle for reporting events to the inspect server
    pub(crate) metrics: MetricsHandle,

    cancel: CancellationToken,
    worker_handles: Mutex<Vec<JoinHandle<()>>>,

    repo_group_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

#[allow(dead_code)]
impl ThreadManager {
    /// Create a new ThreadManager with event support enabled by default.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        max_concurrent: usize,
        max_queue_size: usize,
        storage: Arc<MessageStorage>,
        outbound: Arc<dyn OutboundAdapter>,
        agent: Arc<dyn AgentService>,
        cancel: CancellationToken,
        template_dir: PathBuf,
        config: Arc<ArcSwap<jyc_types::AppConfig>>,
        channel_name: String,
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
            template_dir,
            config,
            channel_name,
            workspace_dir,
            metrics,
        )
    }

    /// Create a new ThreadManager with configurable event support.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_options(
        max_concurrent: usize,
        max_queue_size: usize,
        storage: Arc<MessageStorage>,
        outbound: Arc<dyn OutboundAdapter>,
        agent: Arc<dyn AgentService>,
        cancel: CancellationToken,
        enable_events: bool,
        template_dir: PathBuf,
        config: Arc<ArcSwap<jyc_types::AppConfig>>,
        channel_name: String,
        workspace_dir: PathBuf,
        metrics: MetricsHandle,
    ) -> Self {
        Self {
            thread_queues: Mutex::new(HashMap::new()),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_queue_size,
            storage,
            outbound,
            agent,
            event_buses: Mutex::new(HashMap::new()),
            enable_events,
            thread_cancels: Mutex::new(HashMap::new()),
            template_dir,
            channel_name,
            workspace_dir,
            config,
            metrics,
            cancel: cancel.child_token(),
            worker_handles: Mutex::new(Vec::new()),
            repo_group_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Enqueue a message for processing in the given thread.
    pub async fn enqueue(
        &self,
        message: InboundMessage,
        thread_name: String,
        pattern_match: PatternMatch,
        attachment_config: Option<InboundAttachmentConfig>,
        live_injection: bool,
    ) {
        let mut queues = self.thread_queues.lock().await;

        // Periodic cleanup: remove closed senders to prevent unbounded HashMap growth.
        // This is cheap (O(n) scan) and only retains senders that are still open.
        let mut closed_threads = Vec::new();
        queues.retain(|name, sender| {
            let is_open = !sender.is_closed();
            if !is_open {
                closed_threads.push(name.clone());
            }
            is_open
        });

        // Clean up event buses for closed threads
        if !closed_threads.is_empty() && self.enable_events {
            let mut event_buses = self.event_buses.lock().await;
            for thread_name in closed_threads {
                event_buses.remove(&thread_name);
                tracing::debug!(thread = %thread_name, "Cleaned up event bus for closed thread");
            }
        }

        let template = message
            .metadata
            .get("template")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let item = QueueItem {
            thread_name: thread_name.clone(),
            message,
            pattern_match,
            attachment_config,
            template,
            live_injection,
        };

        self.metrics.message_received(&thread_name);

        if let Some(sender) = queues.get(&thread_name) {
            match sender.try_send(item) {
                Ok(()) => {
                    tracing::debug!(thread = %thread_name, "Message enqueued");
                    return;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(thread = %thread_name, "Queue full, dropping message");
                    self.metrics.queue_dropped(&thread_name);
                    return;
                }
                Err(mpsc::error::TrySendError::Closed(item)) => {
                    queues.remove(&thread_name);
                    // Clean up event bus for this thread
                    if self.enable_events {
                        let mut event_buses = self.event_buses.lock().await;
                        event_buses.remove(&thread_name);
                        tracing::debug!(thread = %thread_name, "Cleaned up event bus for closed queue");
                    }
                    self.create_and_enqueue(&mut queues, thread_name, item)
                        .await;
                    return;
                }
            }
        }

        self.create_and_enqueue(&mut queues, thread_name, item)
            .await;
    }

    async fn create_and_enqueue(
        &self,
        queues: &mut HashMap<String, mpsc::Sender<QueueItem>>,
        thread_name: String,
        item: QueueItem,
    ) {
        let (tx, rx) = mpsc::channel(self.max_queue_size);
        let _ = tx.try_send(item);
        queues.insert(thread_name.clone(), tx);

        // Create event bus for this thread if events are enabled
        let event_bus = if self.enable_events {
            self.get_or_create_event_bus(&thread_name).await
        } else {
            None
        };

        // Create per-thread cancellation token so close_thread can stop this worker
        let thread_cancel = CancellationToken::new();
        {
            let mut cancels = self.thread_cancels.lock().await;
            cancels.insert(thread_name.clone(), thread_cancel.clone());
        }

        let tm = Arc::new(ThreadManager {
            thread_queues: Mutex::new(HashMap::new()),
            semaphore: self.semaphore.clone(),
            max_queue_size: self.max_queue_size,
            storage: self.storage.clone(),
            outbound: self.outbound.clone(),
            agent: self.agent.clone(),
            event_buses: Mutex::new(HashMap::new()),
            enable_events: self.enable_events,
            thread_cancels: Mutex::new(HashMap::new()),
            template_dir: self.template_dir.clone(),
            channel_name: self.channel_name.clone(),
            workspace_dir: self.workspace_dir.clone(),
            config: self.config.clone(),
            metrics: self.metrics.clone(),
            cancel: self.cancel.clone(),
            worker_handles: Mutex::new(vec![]),
            repo_group_locks: self.repo_group_locks.clone(),
        });
        let handle = ThreadManager::spawn_worker(tm, thread_name, rx, event_bus, thread_cancel);

        // Drain completed worker handles to prevent unbounded Vec growth.
        let mut handles = self.worker_handles.lock().await;
        let mut pending = Vec::with_capacity(handles.len() + 1);
        for h in handles.drain(..) {
            if !h.is_finished() {
                pending.push(h);
            }
        }
        pending.push(handle);
        *handles = pending;
    }

    fn spawn_worker(
        thread_manager: Arc<ThreadManager>,
        thread_name: String,
        mut rx: mpsc::Receiver<QueueItem>,
        event_bus: Option<ThreadEventBusRef>,
        thread_cancel: CancellationToken,
    ) -> JoinHandle<()> {
        let semaphore = thread_manager.semaphore.clone();
        let cancel = thread_manager.cancel.clone();
        let storage = thread_manager.storage.clone();
        let outbound = thread_manager.outbound.clone();
        let agent = thread_manager.agent.clone();
        let template_dir = thread_manager.template_dir.clone();
        let config = thread_manager.config.clone();
        let tm = thread_manager;
        let tm_span = tracing::info_span!("tm", t = %thread_name);

        tokio::spawn(async move {
            let mut _permit = tokio::select! {
                permit = semaphore.clone().acquire_owned() => match permit {
                    Ok(p) => p,
                    Err(_) => return,
                },
                _ = cancel.cancelled() => return,
                _ = thread_cancel.cancelled() => return,
            };

            tracing::info!("Worker started");

            // Set thread event bus for agent service
            let _ = agent.set_thread_event_bus(&thread_name, event_bus.clone()).await;

            // Keep event_bus available for error propagation in the dispatch path below
            let event_bus_for_error = event_bus.clone();

            let mut pending: Option<QueueItem> = None;

            loop {
                let mut item = match pending.take() {
                    Some(item) => item,
                    None => tokio::select! {
                        item = rx.recv() => match item {
                            Some(item) => item,
                            None => break,
                        },
                        _ = cancel.cancelled() => {
                            tracing::info!("Worker cancelled");
                            break;
                        }
                        _ = thread_cancel.cancelled() => {
                            tracing::info!("Worker cancelled (thread closed)");
                            break;
                        }
                    },
                };

                // Initialize thread from template if needed
                if let Some(ref template_name) = item.template {
                    let workspace = storage.workspace();
                    let thread_path = workspace.join(&thread_name);

                    match initialize_thread_from_template(
                        &thread_path,
                        template_name,
                        &template_dir,
                    ).await {
                        Ok(()) => {}
                        Err(e) => {
                            // Distinguish template-mismatch from generic init
                            // failures: mismatch is a hard configuration error
                            // we refuse to silently recover from.
                            if e.downcast_ref::<TemplateMismatch>().is_some() {
                                tracing::error!(
                                    error = %e,
                                    thread = %thread_name,
                                    template = %template_name,
                                    "Template mismatch on existing thread; dropping message. \
                                     Two patterns likely share a thread_prefix but use different templates."
                                );
                                tm.metrics.processing_error(&thread_name, "template_mismatch");
                                continue;
                            }
                            tracing::warn!(
                                error = %e,
                                template = %template_name,
                                "Failed to initialize thread from template"
                            );
                        }
                    }

                    if let Some(repo_group_key) = item.message.metadata.get("repo_group_key").and_then(|v| v.as_str()) {
                        let shared_repo_dir = crate::thread_path::resolve_shared_repo_dir(workspace, repo_group_key);
                        let symlink_path = thread_path.join("repo");

                        if let Err(e) = tokio::fs::create_dir_all(&shared_repo_dir).await {
                            tracing::warn!(
                                error = %e,
                                path = %shared_repo_dir.display(),
                                "Failed to create shared repo directory"
                            );
                        }

                        if std::fs::symlink_metadata(&symlink_path).is_err() {
                            if let Err(e) = std::os::unix::fs::symlink(&shared_repo_dir, &symlink_path) {
                                tracing::warn!(
                                    error = %e,
                                    target = %shared_repo_dir.display(),
                                    link = %symlink_path.display(),
                                    "Failed to create repo symlink"
                                );
                            } else {
                                tracing::info!(
                                    thread = %thread_name,
                                    group_key = %repo_group_key,
                                    shared_repo = %shared_repo_dir.display(),
                                    "Created shared repo symlink"
                                );
                            }
                        }
                    }

                    let pattern_file = thread_path.join(".jyc").join("pattern");
                    if let Err(e) = tokio::fs::create_dir_all(thread_path.join(".jyc")).await {
                        tracing::warn!(error = %e, "Failed to create .jyc directory");
                    }
                    if let Err(e) = tokio::fs::write(&pattern_file, &item.pattern_match.pattern_name).await {
                        tracing::warn!(error = %e, "Failed to write pattern file");
                    }
                }

                // Acquire repo group lock to prevent concurrent initialization
                // of the shared repo directory. If the shared dir is already
                // non-empty (a previous agent initialized it), skip the wait.
                // Otherwise, hold the lock for a fixed delay so the first
                // agent's clone can complete before the second agent starts.
                if let Some(repo_group_key) = item.message.metadata.get("repo_group_key").and_then(|v| v.as_str()) {
                    let lock = {
                        let mut locks = tm.repo_group_locks.lock().await;
                        locks.entry(repo_group_key.to_string())
                            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                            .clone()
                    };

                    let workspace = storage.workspace();
                    let shared_repo_dir = crate::thread_path::resolve_shared_repo_dir(workspace, repo_group_key);

                    if let Ok(guard) = lock.clone().try_lock_owned() {
                        let is_empty = match tokio::fs::read_dir(&shared_repo_dir).await {
                            Ok(mut entries) => entries.next_entry().await.unwrap_or(None).is_none(),
                            Err(_) => true,
                        };

                        if is_empty {
                            tracing::info!(
                                thread = %thread_name,
                                group_key = %repo_group_key,
                                "Shared repo dir empty, holding repo group lock for 120s"
                            );
                            let key = repo_group_key.to_string();
                            tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                                drop(guard);
                                tracing::debug!(group_key = %key, "Repo group lock released after delay");
                            });
                        } else {
                            tracing::debug!(
                                thread = %thread_name,
                                group_key = %repo_group_key,
                                "Shared repo dir already initialized, proceeding immediately"
                            );
                            drop(guard);
                        }
                    } else {
                        tracing::info!(
                            thread = %thread_name,
                            group_key = %repo_group_key,
                            "Repo group lock held by another worker, waiting..."
                        );
                        let _guard = lock.lock().await;
                        tracing::info!(
                            thread = %thread_name,
                            group_key = %repo_group_key,
                            "Repo group lock acquired, proceeding"
                        );
                    }
                }

                if let Err(e) = process_message(
                    &mut item,
                    &thread_name,
                    &storage,
                    outbound.clone(),
                    agent.clone(),
                    &mut rx,
                    &template_dir,
                    &config,
                    tm.clone(),
                    thread_cancel.clone(),
                ).await {
                    let err_display = format!("{:#}", e);
                    tracing::error!(
                        error = %err_display,
                        "Failed to process message"
                    );
                    tm.metrics.processing_error(&thread_name, &err_display);

                    if let Some(event_bus) = event_bus_for_error.clone() {
                        let truncated: String = err_display.chars().take(200).collect();
                        let thread_name_clone = thread_name.clone();
                        tokio::spawn(async move {
                            let event = ThreadEvent::SessionStatus {
                                thread_name: thread_name_clone,
                                status_type: "error".to_string(),
                                attempt: None,
                                message: Some(truncated),
                                timestamp: chrono::Utc::now(),
                            };
                            if let Err(publish_err) = event_bus.publish(event).await {
                                tracing::trace!("Failed to publish error event: {}", publish_err);
                            }
                        });
                    }
                }

                // Check symlink integrity after AI processing completes
                if let Some(repo_group_key) = item.message.metadata.get("repo_group_key").and_then(|v| v.as_str()) {
                    let thread_path = storage.workspace().join(&thread_name);
                    let symlink_path = thread_path.join("repo");
                    match tokio::fs::symlink_metadata(&symlink_path).await {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            // Symlink intact — good
                        }
                        Ok(_) => {
                            tracing::warn!(
                                thread = %thread_name,
                                group_key = %repo_group_key,
                                path = %symlink_path.display(),
                                "Shared repo symlink was replaced by a regular directory (agent likely ran rm -rf repo && mkdir repo)"
                            );
                        }
                        Err(_) => {
                            tracing::warn!(
                                thread = %thread_name,
                                group_key = %repo_group_key,
                                path = %symlink_path.display(),
                                "Shared repo symlink is missing after processing"
                            );
                        }
                    }
                }

                // Clear current message after processing
                drop(_permit);

                let next = tokio::select! {
                    item = rx.recv() => match item {
                        Some(item) => item,
                        None => break,
                    },
                    _ = cancel.cancelled() => {
                        tracing::info!("Worker cancelled");
                        break;
                    }
                    _ = thread_cancel.cancelled() => {
                        tracing::info!("Worker cancelled (thread closed)");
                        break;
                    }
                };

                _permit = tokio::select! {
                    permit = semaphore.clone().acquire_owned() => match permit {
                        Ok(p) => p,
                        Err(_) => break,
                    },
                    _ = cancel.cancelled() => {
                        tracing::trace!("Worker cancelled while waiting for permit after receiving message");
                        break;
                    }
                    _ = thread_cancel.cancelled() => {
                        tracing::trace!("Worker cancelled (thread closed) while waiting for permit after receiving message");
                        break;
                    }
                };

                pending = Some(next);
            }

            tracing::info!("Worker finished");
        }.instrument(tm_span))
    }

    #[allow(dead_code)]
    pub async fn get_stats(&self) -> QueueStats {
        let queues = self.thread_queues.lock().await;
        let total_threads = queues.len();
        let active_workers = self.active_worker_count();
        QueueStats {
            active_workers,
            total_threads,
            pending_messages: 0,
        }
    }

    /// Return the channel name this ThreadManager belongs to.
    pub fn channel_name(&self) -> &str {
        &self.channel_name
    }

    /// Return the max concurrent threads (semaphore capacity).
    pub fn max_concurrent(&self) -> usize {
        self.semaphore.available_permits() + self.active_worker_count()
    }

    /// Number of active workers (holding semaphore permits).
    pub fn active_worker_count(&self) -> usize {
        // This is an approximation: semaphore total - available = active
        // We stored the capacity in the constructor but Semaphore doesn't expose it.
        // We use config's max_concurrent_threads as the total.
        self.config
            .load()
            .general
            .max_concurrent_threads
            .saturating_sub(self.semaphore.available_permits())
    }

    /// List all open threads with their info, reading state from disk.
    ///
    /// Scans the workspace directory for thread directories containing `.jyc/pattern`.
    /// This includes both actively queued threads and idle threads that have been
    /// created but have no messages pending.
    pub async fn list_threads(&self) -> Vec<ThreadInfo> {
        use crate::session_state::{read_input_tokens, read_mode_override, read_model_override};

        // Collect names of actively queued threads
        let queues = self.thread_queues.lock().await;
        let active_names: std::collections::HashSet<String> = queues.keys().cloned().collect();
        drop(queues);

        // Scan workspace for all thread directories with .jyc/ subdirectory
        let mut thread_names: Vec<String> = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&self.workspace_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_dir()
                    && path.join(".jyc").is_dir()
                    && let Some(name) = entry.file_name().to_str()
                {
                    thread_names.push(name.to_string());
                }
            }
        }
        thread_names.sort();

        let mut threads = Vec::with_capacity(thread_names.len());

        for name in thread_names {
            let thread_path = self.workspace_dir.join(&name);

            // Read pattern from .jyc/pattern
            let pattern = tokio::fs::read_to_string(thread_path.join(".jyc").join("pattern"))
                .await
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            // Read session state
            let (input_tokens, max_tokens) = read_input_tokens(&thread_path).await;

            // Resolve effective model with priority:
            // 1. .jyc/model-override file (manual runtime override)
            // 2. Pattern-level model from config
            // 3. Channel-level model from config
            // 4. Global agent model from config
            let model = read_model_override(&thread_path)
                .await
                .or_else(|| {
                    let pattern_name = pattern.as_ref()?;
                    let cfg = self.config.load();
                    let channel_cfg = cfg.channels.get(&self.channel_name)?;
                    let patterns = channel_cfg.patterns.as_ref()?;
                    let matched = patterns.iter().find(|p| p.name == *pattern_name)?;
                    matched.model.clone()
                })
                .or_else(|| {
                    let cfg = self.config.load();
                    let channel_cfg = cfg.channels.get(&self.channel_name)?;
                    channel_cfg.model.clone()
                })
                .or_else(|| self.config.load().agent.model.clone());

            let mode = read_mode_override(&thread_path).await;

            // Read skills from .jyc/skills.json
            let skills = read_skills(&thread_path).await;

            // Determine status
            let status = if thread_path.join(".jyc").join("question-sent.flag").exists() {
                ThreadStatus::WaitingForAnswer
            } else if active_names.contains(&name) {
                // Thread has an active queue — it's either processing or waiting for messages
                ThreadStatus::Idle
            } else {
                // Thread exists on disk but has no active queue — it's dormant
                ThreadStatus::Idle
            };

            // Fallback: read .jyc directory mtime if no activity tracker data
            let last_active_at = match tokio::fs::metadata(thread_path.join(".jyc")).await {
                Ok(meta) => match meta.modified() {
                    Ok(mtime) => {
                        let dt: chrono::DateTime<chrono::Utc> = mtime.into();
                        Some(dt.to_rfc3339())
                    }
                    Err(_) => None,
                },
                Err(_) => None,
            };

            threads.push(ThreadInfo {
                name,
                channel: self.channel_name.clone(),
                pattern,
                status,
                model,
                mode,
                input_tokens,
                max_tokens,
                activity: vec![], // Filled by InspectServer from event bus
                last_active_at,   // Filled by activity tracker; falls back to .jyc mtime
                skills,
            });
        }

        threads
    }

    /// Get the event bus for a specific thread.
    ///
    /// Returns None if event support is disabled or the thread doesn't have an event bus.
    pub async fn get_event_bus(&self, thread_name: &str) -> Option<ThreadEventBusRef> {
        if !self.enable_events {
            return None;
        }

        let event_buses = self.event_buses.lock().await;
        event_buses.get(thread_name).cloned()
    }

    /// Create a new event bus for a thread if one doesn't exist.
    ///
    /// Returns the event bus for the thread, or None if event support is disabled.
    async fn get_or_create_event_bus(&self, thread_name: &str) -> Option<ThreadEventBusRef> {
        if !self.enable_events {
            return None;
        }

        let mut event_buses = self.event_buses.lock().await;

        // Check if event bus already exists
        if let Some(event_bus) = event_buses.get(thread_name) {
            return Some(event_bus.clone());
        }

        // Create new event bus
        let event_bus = Arc::new(SimpleThreadEventBus::new(10)); // Capacity of 10 events

        event_buses.insert(thread_name.to_string(), event_bus.clone());
        Some(event_bus)
    }

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

    /// Close and delete a thread's directory.
    ///
    /// This is channel-agnostic — all threads use the same cleanup logic.
    /// Removes the thread directory from disk and cleans up in-memory state.
    pub async fn close_thread(&self, thread_name: &str) -> Result<()> {
        let thread_path = self.storage.workspace().join(thread_name);

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

/// Process a single message within a worker.
///
/// Flow:
/// 1. STORE → chat log
/// 2. COMMAND PROCESS → parse, execute, strip
/// 3. REPLY COMMAND RESULTS → direct reply (if commands found)
/// 4. CHECK BODY → if empty after commands + quoted history stripping → stop
/// 5. DISPATCH TO AGENT → agent.process() handles everything
#[allow(clippy::too_many_arguments)]
async fn process_message(
    item: &mut QueueItem,
    thread_name: &str,
    storage: &MessageStorage,
    outbound: Arc<dyn OutboundAdapter>,
    agent: Arc<dyn AgentService>,
    pending_rx: &mut mpsc::Receiver<QueueItem>,
    template_dir: &Path,
    config: &Arc<ArcSwap<jyc_types::AppConfig>>,
    thread_manager: Arc<ThreadManager>,
    thread_cancel: CancellationToken,
) -> Result<()> {
    // ── 1. STORE ──────────────────────────────────────────────────────
    let is_matched = !item.pattern_match.pattern_name.is_empty();
    let store_result: StoreResult = storage
        .store_with_match(
            &item.message,
            thread_name,
            is_matched,
            item.attachment_config.as_ref(),
        )
        .await?;

    tracing::info!(
        sender = %item.message.sender_address,
        topic = %item.message.topic,
        "Message stored"
    );

    // ── 1.1. WRITE THREAD.JSON (if channel provides metadata) ─────────
    // Channels like wecomkf embed customer info in message metadata.
    // Persist it once per thread so subsequent messages can read cached data.
    if item.message.channel == "wecomkf" {
        let thread_json_path = store_result.thread_path.join(".jyc").join("thread.json");
        if !thread_json_path.exists() {
            write_wecomkf_thread_json(&item.message, &store_result.thread_path, thread_name).await;
        }
    }

    // ── 1.5. SAVE ATTACHMENTS ─────────────────────────────────────────
    // Save attachments AFTER thread name resolution (not before).
    // This ensures attachments go to the correct thread directory when
    // thread_name override is configured on the pattern.
    //
    // The save populates `MessageAttachment.saved_path` on every saved
    // entry — required by the agent's `build_user_blocks` so it can read
    // image bytes from disk and inject them as multimodal content blocks.
    // The previous `&mut message.clone()` here mutated a temporary that
    // was immediately dropped, so `saved_path` never reached the agent
    // and image-only WeChat messages were silently text-only.
    if !item.message.attachments.is_empty()
        && let Err(e) = crate::attachment_storage::save_attachments_to_dir(
            &mut item.message,
            &store_result.thread_path,
            item.attachment_config.as_ref(),
        )
        .await
    {
        tracing::warn!(error = %e, "Failed to save attachments");
    }

    // From here on we only need a shared borrow of the message.
    let message = &item.message;

    // ── 2. COMMAND PROCESS ────────────────────────────────────────────
    let raw_body = message
        .content
        .text
        .as_deref()
        .or(message.content.markdown.as_deref())
        .unwrap_or("");

    let mut command_registry = CommandRegistry::new();
    command_registry.register(Box::new(PlanCommandHandler));
    command_registry.register(Box::new(BuildCommandHandler));
    command_registry.register(Box::new(ResetCommandHandler));
    command_registry.register(Box::new(TemplateCommandHandler));
    command_registry.register(Box::new(CloseCommandHandler::new(thread_manager.clone())));

    let cmd_context = CommandContext {
        args: vec![],
        thread_path: store_result.thread_path.clone(),
        config: config.load_full(),
        channel: message.channel.clone(),
        agent: Some(agent.clone()),
        template_dir: template_dir.to_path_buf(),
    };

    let cmd_output = command_registry
        .process_commands(raw_body, &cmd_context)
        .await?;

    // ── 3. REPLY COMMAND RESULTS (always, if commands found) ──────────
    if !cmd_output.results.is_empty() {
        let summary = cmd_output.results_summary();
        tracing::info!(
            commands = cmd_output.results.len(),
            "Sending command results"
        );

        // Outbound adapter handles formatting + sending + storing
        outbound
            .send_reply(
                message,
                &summary,
                &store_result.thread_path,
                &store_result.message_dir,
                None,
            )
            .await?;
    }

    // ── 4. CHECK BODY ─────────────────────────────────────────────────
    let cleaned_body = outbound.clean_body(&cmd_output.cleaned_body);
    let effective_body_empty = cleaned_body.trim().is_empty();
    let has_attachments = !message.attachments.is_empty();

    tracing::debug!(
        body_empty = effective_body_empty,
        cleaned_len = cleaned_body.trim().len(),
        attachments = message.attachments.len(),
        "Body check after command + quote stripping"
    );

    // Bypass the no-AI short-circuit when the message carries attachments.
    //
    // An attachment-only message is a legitimate AI trigger:
    //   - Image attachments on a vision-capable model with
    //     `inject_inbound_images = true` ride the user turn directly as
    //     multimodal content blocks.
    //   - Non-image attachments (PDF, docx, etc.) are picked up by the
    //     agent via the `read` / `bash` / `read_image` tools — the
    //     invoice-processing skill is the canonical example.
    //
    // Without this bypass the WeChat path silently dropped image-only
    // messages because OpenILink delivers `[image]` as a placeholder body
    // that the channel correctly strips, leaving `cleaned_body` empty.
    if effective_body_empty && !has_attachments {
        tracing::info!("No message body and no attachments, stopping (no AI)");
        return Ok(());
    }
    if effective_body_empty {
        tracing::info!(
            attachments = message.attachments.len(),
            "Empty body but attachments present — proceeding to AI"
        );
    }

    // ── 4.5. CHECK IF THREAD IS WAITING FOR QUESTION ANSWER ──────────
    // If the AI previously asked a question via the ask_user MCP tool,
    // the next user message is the answer — route it to the answer file
    // instead of creating a new AI prompt.
    let question_flag = store_result
        .thread_path
        .join(".jyc")
        .join("question-sent.flag");
    if question_flag.exists() {
        tracing::info!("Thread is waiting for question answer, routing response");
        let answer_file = store_result
            .thread_path
            .join(".jyc")
            .join("question-answer.json");
        let answer = serde_json::json!({
            "answer": cleaned_body.trim(),
            "sender": message.sender_address,
            "answered_at": chrono::Utc::now().to_rfc3339(),
        });
        tokio::fs::write(
            &answer_file,
            serde_json::to_string_pretty(&answer).unwrap_or_default(),
        )
        .await
        .ok();
        tracing::info!(
            answer_len = cleaned_body.trim().len(),
            "Question answer written, MCP tool will pick it up"
        );
        return Ok(());
    }

    // ── 4.75. SEND PROCESSING INDICATOR ───────────────────────────────
    // For channels that support streaming (e.g., wecom_bot), send a
    // "thinking..." indicator before AI processing begins so the user
    // knows the message is being handled.
    let indicator_handle = outbound
        .send_processing_indicator(message)
        .await
        .ok()
        .flatten();

    // ── 5. DISPATCH TO AGENT ──────────────────────────────────────────
    // Build message with cleaned body for agent processing
    let message = {
        let mut m = message.clone();
        m.content.text = Some(cleaned_body);
        m
    };

    // Spawn a background task to watch for pending question deliveries.
    // The question MCP tool writes reply.md + reply-sent.flag during the SSE stream.
    // This watcher detects them and delivers immediately via the outbound adapter,
    // without waiting for the SSE stream to complete.
    let delivery_cancel = tokio_util::sync::CancellationToken::new();
    let delivery_cancel_child = delivery_cancel.clone();
    let delivery_thread_path = store_result.thread_path.clone();
    let delivery_message_dir = store_result.message_dir.clone();
    let delivery_message = message.clone();
    let delivery_outbound = outbound.clone();
    let delivery_handle = tokio::spawn(async move {
        watch_pending_deliveries(
            &delivery_thread_path,
            &delivery_message_dir,
            &delivery_message,
            &*delivery_outbound,
            delivery_cancel_child,
        )
        .await;
    });

    let result = if item.live_injection {
        // Live injection enabled: pass real queue receiver so new messages
        // arriving during AI processing get injected into the active session.
        agent
            .process(
                &message,
                thread_name,
                &store_result.thread_path,
                &store_result.message_dir,
                pending_rx,
                thread_cancel.clone(),
            )
            .await?
    } else {
        // Live injection disabled: pass a dummy receiver that never yields.
        // Messages stay in the real queue and are processed sequentially
        // after the current AI call completes.
        tracing::debug!("Live injection disabled for this pattern, using sequential processing");
        let (_dummy_tx, mut dummy_rx) = mpsc::channel::<QueueItem>(1);
        // Drop _dummy_tx immediately so dummy_rx.recv() returns None instantly,
        // making the SSE select loop skip the injection arm.
        drop(_dummy_tx);
        agent
            .process(
                &message,
                thread_name,
                &store_result.thread_path,
                &store_result.message_dir,
                &mut dummy_rx,
                thread_cancel.clone(),
            )
            .await?
    };

    // Stop the delivery watcher
    delivery_cancel.cancel();
    let _ = delivery_handle.await;

    // ── 5.5. GUARD: skip reply if thread directory no longer exists ──
    // If the thread was closed while AI was processing, the directory gets
    // deleted. Even with SSE cancellation, there's a small race window.
    // This guard prevents posting comments to closed issues/PRs.
    if !store_result.thread_path.exists() {
        tracing::warn!(
            thread_path = %store_result.thread_path.display(),
            "Thread directory no longer exists — skipping reply delivery"
        );
        return Ok(());
    }

    // ── 6. HANDLE AGENT RESULT ────────────────────────────────────────
    // The MCP reply tool stores the reply in the chat log and writes a signal file.
    // The monitor process (this code) handles actual delivery using its
    // pre-warmed outbound adapter with cached connections/tokens.
    if result.reply_sent_by_tool {
        // Check if the background delivery watcher already delivered the reply.
        // The watcher deletes reply-sent.flag after successful delivery.
        let signal_path = store_result
            .thread_path
            .join(".jyc")
            .join("reply-sent.flag");
        if !signal_path.exists() {
            tracing::info!(
                "Reply already delivered by background watcher, skipping post-SSE delivery"
            );
        } else {
            // Reply text comes from the SSE tool input (extracted by service layer).
            // If not available (e.g., question tool), try reading from reply.md.
            let reply_text = result
                .reply_text
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .map(|t| t.to_string());

            let reply_text = match reply_text {
                Some(t) => Some(t),
                None => {
                    // Fallback: read from .jyc/reply.md (written by question tool or other MCP tools)
                    let reply_md = store_result.thread_path.join(".jyc").join("reply.md");
                    if reply_md.exists() {
                        tokio::fs::read_to_string(&reply_md)
                            .await
                            .ok()
                            .filter(|t| !t.trim().is_empty())
                    } else {
                        None
                    }
                }
            };

            if let Some(ref reply_text) = reply_text {
                tracing::info!(
                    text_len = reply_text.len(),
                    "Delivering reply from MCP tool"
                );

                // Read signal file for attachment info
                let attachments =
                    read_signal_attachments(&signal_path, &store_result.thread_path).await;

                outbound
                    .send_reply(
                        &message,
                        reply_text,
                        &store_result.thread_path,
                        &store_result.message_dir,
                        attachments.as_deref(),
                    )
                    .await?;
                tracing::info!("Reply delivered via outbound adapter");
                // Clean up signal files after successful delivery to prevent re-delivery on restart
                tokio::fs::remove_file(&signal_path).await.ok();
                let reply_md_path = store_result.thread_path.join(".jyc").join("reply.md");
                tokio::fs::remove_file(&reply_md_path).await.ok();
                thread_manager.metrics.reply_by_tool(thread_name);
            } else {
                tracing::warn!("MCP tool signaled reply but no reply text available");
            }
        }
    } else if let Some(ref text) = result.reply_text {
        tracing::info!(
            text_len = text.len(),
            "Fallback: sending AI text via outbound"
        );
        outbound
            .send_reply(
                &message,
                text,
                &store_result.thread_path,
                &store_result.message_dir,
                None,
            )
            .await?;
        tracing::info!("Fallback reply sent");
        thread_manager.metrics.reply_by_fallback(thread_name);
    } else {
        tracing::warn!("No reply text from AI");
        // Clear the processing indicator so it doesn't remain stuck
        // in an intermediate state (e.g., "正在思考中..." forever).
        if let Err(e) = outbound.clear_processing_indicator(indicator_handle).await {
            tracing::warn!(error = %format!("{:#}", e), "Failed to clear processing indicator");
        }
    }

    Ok(())
}

/// Read attachment filenames from the reply-sent.flag signal file.
/// Returns OutboundAttachment list, or None if no attachments.
async fn read_signal_attachments(
    signal_path: &std::path::Path,
    thread_path: &std::path::Path,
) -> Option<Vec<jyc_types::OutboundAttachment>> {
    let content = tokio::fs::read_to_string(signal_path).await.ok()?;
    let signal: serde_json::Value = serde_json::from_str(&content).ok()?;

    let filenames = signal.get("attachments")?.as_array()?;
    if filenames.is_empty() {
        return None;
    }

    let attachments: Vec<jyc_types::OutboundAttachment> = filenames
        .iter()
        .filter_map(|v| v.as_str())
        .map(|filename| {
            let path = thread_path.join(filename);
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let content_type = match ext.as_str() {
                "pdf" => "application/pdf",
                "pptx" => {
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                }
                "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "txt" | "md" => "text/plain",
                _ => "application/octet-stream",
            };
            jyc_types::OutboundAttachment {
                filename: filename.to_string(),
                path,
                content_type: content_type.to_string(),
            }
        })
        .collect();

    Some(attachments)
}

/// Read skills from thread's .jyc/skills.json file.
async fn read_skills(thread_path: &Path) -> Vec<String> {
    let skills_path = thread_path.join(".jyc").join("skills.json");
    match tokio::fs::read_to_string(&skills_path).await {
        Ok(content) => serde_json::from_str::<Vec<String>>(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Error returned when an existing thread directory was created from a
/// different template than the one the current message is requesting. The
/// thread manager surfaces this and drops the message rather than risk
/// overwriting AGENTS.md / template files in place.
#[derive(Debug, thiserror::Error)]
#[error(
    "thread '{thread}' was initialized from template '{existing}' but pattern requires template '{requested}'; refusing to overwrite. Configure distinct `thread_prefix` values for these patterns."
)]
pub struct TemplateMismatch {
    pub thread: String,
    pub existing: String,
    pub requested: String,
}

async fn initialize_thread_from_template(
    thread_path: &Path,
    template_name: &str,
    template_dir: &Path,
) -> Result<()> {
    let jyc_dir = thread_path.join(".jyc");
    let template_marker = jyc_dir.join("template");

    if thread_path.exists() {
        // Thread already exists. Verify the recorded template matches the
        // one the current pattern requests; refuse if they differ to avoid
        // silently running with the wrong AGENTS.md.
        match tokio::fs::read_to_string(&template_marker).await {
            Ok(existing) => {
                let existing = existing.trim();
                if existing == template_name {
                    return Ok(());
                }
                let thread_label = thread_path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| thread_path.display().to_string());
                return Err(TemplateMismatch {
                    thread: thread_label,
                    existing: existing.to_string(),
                    requested: template_name.to_string(),
                }
                .into());
            }
            Err(_) => {
                // No marker file. Either the thread was created before this
                // mechanism existed or by a path that doesn't use templates.
                // Don't overwrite — preserve existing behavior.
                return Ok(());
            }
        }
    }

    let template_src = template_dir.join(template_name);
    if !template_src.exists() {
        tracing::warn!(
            template = %template_name,
            path = %template_src.display(),
            "Template directory does not exist"
        );
        return Ok(());
    }

    copy_template_files(&template_src, thread_path).await?;

    tokio::fs::create_dir_all(&jyc_dir).await?;

    tokio::fs::write(&template_marker, template_name)
        .await
        .context("failed to write template name")?;

    tracing::info!(template = %template_name, "Thread initialized from template");

    Ok(())
}

#[cfg(test)]
mod template_init_tests {
    use super::*;
    use tempfile::tempdir;

    async fn make_template(template_dir: &Path, name: &str, body: &str) {
        let dir = template_dir.join(name);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("AGENTS.md"), body).await.unwrap();
    }

    #[tokio::test]
    async fn fresh_thread_writes_marker() {
        let tmp = tempdir().unwrap();
        let template_dir = tmp.path().join("templates");
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        make_template(&template_dir, "github-planner", "PLANNER").await;

        let thread_path = workspace.join("issue-1");
        initialize_thread_from_template(&thread_path, "github-planner", &template_dir)
            .await
            .unwrap();

        let marker = tokio::fs::read_to_string(thread_path.join(".jyc/template"))
            .await
            .unwrap();
        assert_eq!(marker.trim(), "github-planner");
        assert_eq!(
            tokio::fs::read_to_string(thread_path.join("AGENTS.md"))
                .await
                .unwrap(),
            "PLANNER"
        );
    }

    #[tokio::test]
    async fn matching_template_is_idempotent() {
        let tmp = tempdir().unwrap();
        let template_dir = tmp.path().join("templates");
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        make_template(&template_dir, "github-planner", "PLANNER").await;

        let thread_path = workspace.join("issue-1");
        initialize_thread_from_template(&thread_path, "github-planner", &template_dir)
            .await
            .unwrap();

        // Second call with the same template is a no-op.
        initialize_thread_from_template(&thread_path, "github-planner", &template_dir)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn template_mismatch_is_refused() {
        let tmp = tempdir().unwrap();
        let template_dir = tmp.path().join("templates");
        let workspace = tmp.path().join("workspace");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        make_template(&template_dir, "github-high-level-planner", "HLP").await;
        make_template(&template_dir, "github-planner", "PLANNER").await;

        let thread_path = workspace.join("issue-1");
        // First, init with HLP.
        initialize_thread_from_template(&thread_path, "github-high-level-planner", &template_dir)
            .await
            .unwrap();

        // Then, request a different template for the same thread → must error.
        let err = initialize_thread_from_template(&thread_path, "github-planner", &template_dir)
            .await
            .expect_err("expected TemplateMismatch");
        assert!(
            err.downcast_ref::<TemplateMismatch>().is_some(),
            "expected TemplateMismatch, got: {:#}",
            err
        );

        // AGENTS.md must not have been overwritten.
        let body = tokio::fs::read_to_string(thread_path.join("AGENTS.md"))
            .await
            .unwrap();
        assert_eq!(body, "HLP");
    }
}

#[cfg(test)]
mod thread_json_tests {
    use super::*;
    use jyc_types::{InboundMessage, MessageContent};
    use std::collections::HashMap;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_write_wecomkf_thread_json_creates_file() {
        let tmp = tempdir().unwrap();
        let thread_path = tmp.path().join("test-thread");

        let mut metadata = HashMap::new();
        metadata.insert(
            "external_userid".to_string(),
            serde_json::Value::String("wm123".to_string()),
        );
        metadata.insert(
            "user_name".to_string(),
            serde_json::Value::String("张三".to_string()),
        );
        metadata.insert(
            "open_kfid".to_string(),
            serde_json::Value::String("kf001".to_string()),
        );

        let message = InboundMessage {
            id: "test-1".to_string(),
            channel: "wecomkf".to_string(),
            channel_uid: "uid".to_string(),
            sender: "wm123".to_string(),
            sender_address: "wecomkf:wm123".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent {
                text: Some("Hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata,
            matched_pattern: None,
        };

        write_wecomkf_thread_json(&message, &thread_path, "test-thread").await;

        let thread_json = ThreadJson::read(&thread_path).await.unwrap().unwrap();
        assert_eq!(thread_json.channel_type, "wecomkf");
        assert_eq!(thread_json.version, 1);

        let data = thread_json.data_as::<serde_json::Value>().unwrap().unwrap();
        assert_eq!(
            data.get("external_userid").and_then(|v| v.as_str()),
            Some("wm123")
        );
        assert_eq!(data.get("user_name").and_then(|v| v.as_str()), Some("张三"));
        assert_eq!(
            data.get("open_kfid").and_then(|v| v.as_str()),
            Some("kf001")
        );
        assert!(data.get("first_message_at").is_some());
    }

    #[tokio::test]
    async fn test_write_wecomkf_thread_json_skips_without_external_userid() {
        let tmp = tempdir().unwrap();
        let thread_path = tmp.path().join("test-thread");

        let message = InboundMessage {
            id: "test-1".to_string(),
            channel: "wecomkf".to_string(),
            channel_uid: "uid".to_string(),
            sender: "user".to_string(),
            sender_address: "wecomkf:user".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent {
                text: Some("Hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        };

        write_wecomkf_thread_json(&message, &thread_path, "test-thread").await;

        // No external_userid in metadata → file should not be created
        assert!(!thread_path.join(".jyc/thread.json").exists());
    }

    #[tokio::test]
    async fn test_write_wecomkf_thread_json_fallback_user_name() {
        let tmp = tempdir().unwrap();
        let thread_path = tmp.path().join("test-thread");

        let mut metadata = HashMap::new();
        metadata.insert(
            "external_userid".to_string(),
            serde_json::Value::String("wm456".to_string()),
        );
        // No user_name in metadata → should fallback to external_userid

        let message = InboundMessage {
            id: "test-1".to_string(),
            channel: "wecomkf".to_string(),
            channel_uid: "uid".to_string(),
            sender: "wm456".to_string(),
            sender_address: "wecomkf:wm456".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent {
                text: Some("Hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata,
            matched_pattern: None,
        };

        write_wecomkf_thread_json(&message, &thread_path, "test-thread").await;

        let thread_json = ThreadJson::read(&thread_path).await.unwrap().unwrap();
        let data = thread_json.data_as::<serde_json::Value>().unwrap().unwrap();
        assert_eq!(
            data.get("user_name").and_then(|v| v.as_str()),
            Some("wm456")
        );
    }
}

/// Write `thread.json` for a WeCom KF thread from message metadata.
///
/// Extracts `external_userid`, `user_name`, and `open_kfid` from the
/// message metadata and persists them in `.jyc/thread.json`.
async fn write_wecomkf_thread_json(
    message: &InboundMessage,
    thread_path: &Path,
    thread_name: &str,
) {
    if let Some(external_userid) = message
        .metadata
        .get("external_userid")
        .and_then(|v| v.as_str())
    {
        let user_name = message
            .metadata
            .get("user_name")
            .and_then(|v| v.as_str())
            .unwrap_or(external_userid);
        let open_kfid = message
            .metadata
            .get("open_kfid")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let thread_json = ThreadJson {
            channel_type: "wecomkf".to_string(),
            version: 1,
            data: Some(serde_json::json!({
                "external_userid": external_userid,
                "user_name": user_name,
                "open_kfid": open_kfid,
                "first_message_at": chrono::Utc::now().to_rfc3339(),
            })),
        };
        if let Err(e) = thread_json.write(thread_path).await {
            tracing::warn!(
                error = %e,
                thread = %thread_name,
                "Failed to write thread.json"
            );
        } else {
            tracing::info!(thread = %thread_name, "Wrote thread.json");
        }
    }
}
