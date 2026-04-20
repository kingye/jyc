use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::core::thread_event_bus::{ThreadEventBusRef, SimpleThreadEventBus};

use crate::channels::types::{InboundMessage, OutboundAdapter, PatternMatch};
use crate::config::types::{HeartbeatConfig, InboundAttachmentConfig};
use crate::core::command::handler::CommandContext;
use crate::core::command::model_handler::ModelCommandHandler;
use crate::core::command::mode_handler::{BuildCommandHandler, PlanCommandHandler};
use crate::core::command::registry::CommandRegistry;
use crate::core::command::close_handler::CloseCommandHandler;
use crate::core::command::reset_handler::ResetCommandHandler;
use crate::core::command::template_handler::TemplateCommandHandler;
use crate::core::message_storage::{MessageStorage, StoreResult};
use crate::core::metrics::MetricsHandle;
use crate::core::pending_delivery::watch_pending_deliveries;
use crate::core::template_utils::copy_template_files;
use crate::inspect::types::{ThreadInfo, ThreadStatus};
use crate::services::agent::AgentService;

/// An item in a thread's message queue.
pub struct QueueItem {
    pub message: InboundMessage,
    #[allow(dead_code)]
    pub pattern_match: PatternMatch,
    pub attachment_config: Option<InboundAttachmentConfig>,
    pub template: Option<String>,
    /// Whether live message injection is enabled for this item's pattern.
    /// When `true`, new messages arriving during AI processing are injected
    /// into the active session. When `false`, messages queue sequentially.
    pub live_injection: bool,
}

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
    thread_cancels: Mutex<HashMap<String, CancellationToken>>,

    // Heartbeat configuration
    heartbeat_config: HeartbeatConfig,

    // Per-channel heartbeat message template (supports {elapsed} placeholder)
    heartbeat_template: String,

    // Template directory for thread initialization
    template_dir: PathBuf,

    // Channel name this ThreadManager belongs to
    channel_name: String,

    // Workspace directory for this channel (<workdir>/<channel>/workspace/)
    workspace_dir: PathBuf,

    // Application config (for command handlers that need channel/pattern info)
    config: Arc<crate::config::types::AppConfig>,

    // Metrics handle for reporting events to the inspect server
    pub(crate) metrics: MetricsHandle,

    cancel: CancellationToken,
    worker_handles: Mutex<Vec<JoinHandle<()>>>,
}

#[allow(dead_code)]
impl ThreadManager {
    /// Create a new ThreadManager with event support enabled by default.
    pub fn new(
        max_concurrent: usize,
        max_queue_size: usize,
        storage: Arc<MessageStorage>,
        outbound: Arc<dyn OutboundAdapter>,
        agent: Arc<dyn AgentService>,
        cancel: CancellationToken,
        heartbeat_config: HeartbeatConfig,
        heartbeat_template: String,
        template_dir: PathBuf,
        config: Arc<crate::config::types::AppConfig>,
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
            heartbeat_config,
            heartbeat_template,
            template_dir,
            config,
            channel_name,
            workspace_dir,
            metrics,
        )
    }
    
    /// Create a new ThreadManager with configurable event support.
    pub fn new_with_options(
        max_concurrent: usize,
        max_queue_size: usize,
        storage: Arc<MessageStorage>,
        outbound: Arc<dyn OutboundAdapter>,
        agent: Arc<dyn AgentService>,
        cancel: CancellationToken,
        enable_events: bool,
        heartbeat_config: HeartbeatConfig,
        heartbeat_template: String,
        template_dir: PathBuf,
        config: Arc<crate::config::types::AppConfig>,
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
            heartbeat_config,
            heartbeat_template,
            template_dir,
            channel_name,
            workspace_dir,
            config,
            metrics,
            cancel: cancel.child_token(),
            worker_handles: Mutex::new(Vec::new()),
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

        let template = message.metadata.get("template")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        
        let item = QueueItem {
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
                    self.create_and_enqueue(&mut queues, thread_name, item).await;
                    return;
                }
            }
        }

        self.create_and_enqueue(&mut queues, thread_name, item).await;
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
            heartbeat_config: self.heartbeat_config.clone(),
            heartbeat_template: self.heartbeat_template.clone(),
            template_dir: self.template_dir.clone(),
            channel_name: self.channel_name.clone(),
            workspace_dir: self.workspace_dir.clone(),
            config: self.config.clone(),
            metrics: self.metrics.clone(),
            cancel: self.cancel.clone(),
            worker_handles: Mutex::new(vec![]),
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
        let heartbeat_config = thread_manager.heartbeat_config.clone();
        let heartbeat_template = thread_manager.heartbeat_template.clone();
        let template_dir = thread_manager.template_dir.clone();
        let config = thread_manager.config.clone();
        let tm = thread_manager;
        let tm_span = tracing::info_span!("tm", t = %thread_name);

        tokio::spawn(async move {
            let _permit = tokio::select! {
                permit = semaphore.acquire_owned() => match permit {
                    Ok(p) => p,
                    Err(_) => return,
                },
                _ = cancel.cancelled() => return,
                _ = thread_cancel.cancelled() => return,
            };

            tracing::info!("Worker started");

            // Set thread event bus for agent service
            let _ = agent.set_thread_event_bus(&thread_name, event_bus.clone()).await;

            // Create a channel to pass the current message to the event listener
            let (current_message_tx, current_message_rx) = tokio::sync::watch::channel(None);

            // Start event listener if event bus is provided and heartbeat is enabled
            let thread_cancel_for_listener = thread_cancel.clone();
            let event_listener_handle = if heartbeat_config.enabled {
                if let Some(event_bus) = event_bus {
                    tracing::trace!(thread = %thread_name, "Creating event listener with heartbeat control");
                    let outbound_clone = outbound.clone();
                    let thread_name_clone = thread_name.clone();
                    let current_message_rx_clone = current_message_rx.clone();
                    let hb_config = heartbeat_config.clone();
                    let hb_template = heartbeat_template.clone();
                    
                    {
                        let thread_name_for_finish = thread_name_clone.clone();
                        Some(tokio::spawn(async move {
                            tracing::trace!(thread = %thread_name_clone, "Event listener started");
                            // Start event listener with heartbeat timing control
                            Self::event_listener_with_heartbeat(
                                event_bus,
                                thread_name_clone,
                                outbound_clone,
                                current_message_rx_clone,
                                hb_config,
                                hb_template,
                                thread_cancel_for_listener,
                            ).await;
                             tracing::trace!(thread = %thread_name_for_finish, "Event listener finished");
                        }))
                    }
                } else {
                    tracing::trace!(thread = %thread_name, "No event bus provided, event listener disabled");
                    None
                }
            } else {
                tracing::trace!(thread = %thread_name, "Heartbeat disabled by config");
                None
            };

            loop {
                let item = tokio::select! {
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

                // Initialize thread from template if needed
                if let Some(ref template_name) = item.template {
                    let workspace = storage.workspace();
                    let thread_path = workspace.join(&thread_name);
                    
                    // Initialize template first (before creating .jyc to avoid exists check failing)
                    if let Err(e) = initialize_thread_from_template(
                        &thread_path,
                        template_name,
                        &template_dir,
                    ).await {
                        tracing::warn!(
                            error = %e,
                            template = %template_name,
                            "Failed to initialize thread from template"
                        );
                    }
                    
                    // Save pattern name for /template command (after template init)
                    let pattern_file = thread_path.join(".jyc").join("pattern");
                    if let Err(e) = tokio::fs::create_dir_all(thread_path.join(".jyc")).await {
                        tracing::warn!(error = %e, "Failed to create .jyc directory");
                    }
                    if let Err(e) = tokio::fs::write(&pattern_file, &item.pattern_match.pattern_name).await {
                        tracing::warn!(error = %e, "Failed to write pattern file");
                    }
                }

                // Update current message for event listeners
                let _ = current_message_tx.send(Some(item.message.clone()));
                
                if let Err(e) = process_message(
                    &item,
                    &thread_name,
                    &storage,
                    outbound.clone(),
                    agent.clone(),
                    &mut rx,
                    &template_dir,
                    &config,
                    tm.clone(),
                ).await {
                    tracing::error!(
                        error = %format!("{:#}", e),
                        "Failed to process message"
                    );
                    tm.metrics.processing_error(&thread_name, &format!("{:#}", e));
                }
                
                // Clear current message after processing
                let _ = current_message_tx.send(None);
            }

            // Wait for event listener to finish
            if let Some(handle) = event_listener_handle {
                let _ = handle.await;
                tracing::debug!("Event listener finished");
            }

            tracing::info!("Worker finished");
        }.instrument(tm_span))
    }

    #[allow(dead_code)]
    pub async fn get_stats(&self) -> QueueStats {
        let queues = self.thread_queues.lock().await;
        let total_threads = queues.len();
        let active_workers = self.semaphore.available_permits();
        QueueStats {
            active_workers: total_threads.saturating_sub(active_workers),
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
    fn active_worker_count(&self) -> usize {
        // This is an approximation: semaphore total - available = active
        // We stored the capacity in the constructor but Semaphore doesn't expose it.
        // We use config's max_concurrent_threads as the total.
        self.config.general.max_concurrent_threads
            .saturating_sub(self.semaphore.available_permits())
    }

    /// List all open threads with their info, reading state from disk.
    ///
    /// Scans the workspace directory for thread directories containing `.jyc/pattern`.
    /// This includes both actively queued threads and idle threads that have been
    /// created but have no messages pending.
    pub async fn list_threads(&self) -> Vec<ThreadInfo> {
        use crate::services::opencode::session::{
            read_input_tokens, read_model_override, read_mode_override,
        };

        // Collect names of actively queued threads
        let queues = self.thread_queues.lock().await;
        let active_names: std::collections::HashSet<String> = queues.keys().cloned().collect();
        drop(queues);

        // Scan workspace for all thread directories with .jyc/ subdirectory
        let mut thread_names: Vec<String> = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&self.workspace_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_dir() && path.join(".jyc").is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        thread_names.push(name.to_string());
                    }
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
            let model = read_model_override(&thread_path).await;
            let mode = read_mode_override(&thread_path).await;

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

            threads.push(ThreadInfo {
                name,
                channel: self.channel_name.clone(),
                pattern,
                status,
                model,
                mode,
                input_tokens,
                max_tokens,
                activity: vec![],  // Filled by InspectServer from event bus
            });
        }

        threads
    }
    
    /// Event listener that controls heartbeat timing based on HeartbeatConfig.
    /// Each thread has its own isolated event listener.
    async fn event_listener_with_heartbeat(
        event_bus: crate::core::thread_event_bus::ThreadEventBusRef,
        thread_name: String,
        outbound: Arc<dyn OutboundAdapter>,
        current_message_rx: tokio::sync::watch::Receiver<Option<crate::channels::types::InboundMessage>>,
        heartbeat_config: HeartbeatConfig,
        heartbeat_template: String,
        thread_cancel: CancellationToken,
    ) {
    use std::time::{Instant, Duration};
    
    let heartbeat_interval = Duration::from_secs(heartbeat_config.interval_secs);
    let min_heartbeat_elapsed = Duration::from_secs(heartbeat_config.min_elapsed_secs);

    tracing::debug!(
        thread = %thread_name,
        interval_secs = heartbeat_config.interval_secs,
        min_elapsed_secs = heartbeat_config.min_elapsed_secs,
        "Heartbeat config loaded"
    );
    
    // Subscribe to this thread's event bus
    let mut receiver = match event_bus.subscribe().await {
        Ok(receiver) => receiver,
        Err(e) => {
            tracing::warn!(thread = %thread_name, error = %e, "Failed to subscribe to event bus");
            return;
        }
    };
    
    // State for this thread's heartbeat control
    let mut last_heartbeat_sent: Option<Instant> = None;
    let mut last_processing_state: Option<(u64, String, String)> = None; // (elapsed_secs, activity, progress)
    
    // Heartbeat timer for this thread
    let mut heartbeat_timer = tokio::time::interval(heartbeat_interval);
    heartbeat_timer.tick().await; // Skip immediate tick
    
    loop {
        tokio::select! {
            // Receive events from this thread's event bus
            event = receiver.recv() => {
                if let Some(event) = event {
                    // Clone event for logging (since we might partially move it)
                    let event_clone = event.clone();
                    
                    // Update processing state based on ProcessingProgress events
                    if let crate::core::thread_event::ThreadEvent::ProcessingProgress {
                        thread_name: event_thread_name,
                        elapsed_secs,
                        activity,
                        progress,
                        parts_count: _,
                        output_length: _,
                        timestamp: _,
                    } = &event {
                        // Verify this event is for our thread (should always be true due to isolation)
                        if event_thread_name == &thread_name {
                            let progress_str = progress.clone().unwrap_or_else(|| "Processing...".to_string());
                            last_processing_state = Some((*elapsed_secs, activity.clone(), progress_str));
                            tracing::trace!(
                                thread = %thread_name,
                                elapsed_secs = elapsed_secs,
                                activity = %activity,
                                "Updated processing state"
                            );
                        }
                    }
                    
                    // Log other events for debugging
                    tracing::trace!(
                        thread = %thread_name,
                        event = ?event_clone,
                        "Received thread event"
                    );
                } else {
                    tracing::trace!(thread = %thread_name, "Event bus channel closed");
                    break;
                }
            }
            
            // Heartbeat timer tick - check if we should send a heartbeat
            _ = heartbeat_timer.tick() => {
                // Get current message for this thread
                let current_message = current_message_rx.borrow().clone();
                
                if let Some(message) = current_message {
                    tracing::trace!(thread = %thread_name, "Current message available for heartbeat check");
                    // Check if we have processing state and should send heartbeat
                    if let Some((elapsed_secs, activity, progress)) = &last_processing_state {
                        tracing::debug!(
                            thread = %thread_name,
                            elapsed_secs = elapsed_secs,
                            activity = %activity,
                            progress = %progress,
                            "Processing state available"
                        );
                        // Check minimum elapsed time
                        let processing_elapsed = Duration::from_secs(*elapsed_secs);
                        if processing_elapsed < min_heartbeat_elapsed {
                        tracing::trace!(
                            thread = %thread_name,
                            elapsed_secs = elapsed_secs,
                            "Processing just started, skipping heartbeat (elapsed < min_elapsed_secs)"
                        );
                            continue;
                        }
                        
                        // Check heartbeat interval
                        let should_send = match last_heartbeat_sent {
                            Some(last_sent) => last_sent.elapsed() >= heartbeat_interval,
                            None => true, // First heartbeat
                        };
                        
                        if should_send {
                            // Format the heartbeat message from per-channel template
                            let minutes = elapsed_secs / 60;
                            let seconds = elapsed_secs % 60;
                            let elapsed_str = format!("{}m {}s", minutes, seconds);
                            let heartbeat_msg = heartbeat_template.replace("{elapsed}", &elapsed_str);

                            tracing::info!(
                                thread = %thread_name,
                                elapsed_secs = elapsed_secs,
                                "Sending heartbeat"
                            );
                            
                            match outbound.send_heartbeat(&message, &heartbeat_msg).await {
                                Ok(result) => {
                                    tracing::info!(
                                        thread = %thread_name,
                                        message_id = %result.message_id,
                                        "Heartbeat sent successfully"
                                    );
                                    last_heartbeat_sent = Some(Instant::now());
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        thread = %thread_name,
                                        error = %e,
                                        "Failed to send heartbeat"
                                    );
                                }
                            }
                        } else {
                            tracing::debug!(
                                thread = %thread_name,
                                "Heartbeat interval not yet reached"
                            );
                        }
                    } else {
                        tracing::trace!(
                            thread = %thread_name,
                            "No processing state yet, skipping heartbeat (need ProcessingProgress event)"
                        );
                    }
                } else {
                    tracing::trace!(
                        thread = %thread_name,
                        "No current message, skipping heartbeat"
                    );
                }
            }
            // Thread closed — stop the event listener
            _ = thread_cancel.cancelled() => {
                tracing::debug!(thread = %thread_name, "Event listener cancelled (thread closed)");
                break;
            }
        }
    }
    
    tracing::debug!(thread = %thread_name, "Event listener with heartbeat finished");
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
            tokio::fs::remove_dir_all(&thread_path)
                .await
                .context(format!("Failed to remove thread directory: {:?}", thread_path))?;
            tracing::info!(thread = %thread_name, "Thread directory deleted");
        }

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
}

/// Process a single message within a worker.
///
/// Flow:
/// 1. STORE → chat log
/// 2. COMMAND PROCESS → parse, execute, strip
/// 3. REPLY COMMAND RESULTS → direct reply (if commands found)
/// 4. CHECK BODY → if empty after commands + quoted history stripping → stop
/// 5. DISPATCH TO AGENT → agent.process() handles everything
async fn process_message(
    item: &QueueItem,
    thread_name: &str,
    storage: &MessageStorage,
    outbound: Arc<dyn OutboundAdapter>,
    agent: Arc<dyn AgentService>,
    pending_rx: &mut mpsc::Receiver<QueueItem>,
    template_dir: &PathBuf,
    config: &Arc<crate::config::types::AppConfig>,
    thread_manager: Arc<ThreadManager>,
) -> Result<()> {
    let message = &item.message;

    // ── 1. STORE ──────────────────────────────────────────────────────
    let is_matched = !item.pattern_match.pattern_name.is_empty();
    let store_result: StoreResult = storage
        .store_with_match(message, thread_name, is_matched, item.attachment_config.as_ref())
        .await?;

    tracing::info!(
        sender = %message.sender_address,
        topic = %message.topic,
        "Message stored"
    );

    // ── 1.5. SAVE ATTACHMENTS ─────────────────────────────────────────
    // Save attachments AFTER thread name resolution (not before).
    // This ensures attachments go to the correct thread directory when
    // thread_name override is configured on the pattern.
    if !message.attachments.is_empty() {
        if let Err(e) = crate::core::attachment_storage::save_attachments_to_dir(
            &mut message.clone(),
            &store_result.thread_path,
            item.attachment_config.as_ref(),
        ).await {
            tracing::warn!(error = %e, "Failed to save attachments");
        }
    }

    // ── 2. COMMAND PROCESS ────────────────────────────────────────────
    let raw_body = message
        .content
        .text
        .as_deref()
        .or(message.content.markdown.as_deref())
        .unwrap_or("");

    let mut command_registry = CommandRegistry::new();
    command_registry.register(Box::new(ModelCommandHandler));
    command_registry.register(Box::new(PlanCommandHandler));
    command_registry.register(Box::new(BuildCommandHandler));
    command_registry.register(Box::new(ResetCommandHandler));
    command_registry.register(Box::new(TemplateCommandHandler));
    command_registry.register(Box::new(CloseCommandHandler::new(thread_manager.clone())));

    let cmd_context = CommandContext {
        args: vec![],
        thread_path: store_result.thread_path.clone(),
        config: config.clone(),
        channel: message.channel.clone(),
        agent: Some(agent.clone()),
        template_dir: template_dir.clone(),
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

    tracing::debug!(
        body_empty = effective_body_empty,
        cleaned_len = cleaned_body.trim().len(),
        "Body check after command + quote stripping"
    );

    if effective_body_empty {
        tracing::info!("No message body, stopping (no AI)");
        return Ok(());
    }

    // ── 4.5. CHECK IF THREAD IS WAITING FOR QUESTION ANSWER ──────────
    // If the AI previously asked a question via the ask_user MCP tool,
    // the next user message is the answer — route it to the answer file
    // instead of creating a new AI prompt.
    let question_flag = store_result.thread_path.join(".jyc").join("question-sent.flag");
    if question_flag.exists() {
        tracing::info!("Thread is waiting for question answer, routing response");
        let answer_file = store_result.thread_path.join(".jyc").join("question-answer.json");
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
        ).await;
    });

    let result = if item.live_injection {
        // Live injection enabled: pass real queue receiver so new messages
        // arriving during AI processing get injected into the active session.
        agent
            .process(&message, thread_name, &store_result.thread_path, &store_result.message_dir, pending_rx)
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
            .process(&message, thread_name, &store_result.thread_path, &store_result.message_dir, &mut dummy_rx)
            .await?
    };

    // Stop the delivery watcher
    delivery_cancel.cancel();
    let _ = delivery_handle.await;

    // ── 6. HANDLE AGENT RESULT ────────────────────────────────────────
    // The MCP reply tool stores the reply in the chat log and writes a signal file.
    // The monitor process (this code) handles actual delivery using its
    // pre-warmed outbound adapter with cached connections/tokens.
    if result.reply_sent_by_tool {
        // Reply text comes from the SSE tool input (extracted by service layer).
        // If not available (e.g., question tool), try reading from reply.md.
        let reply_text = result.reply_text.as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(|t| t.to_string());

        let reply_text = match reply_text {
            Some(t) => Some(t),
            None => {
                // Fallback: read from .jyc/reply.md (written by question tool or other MCP tools)
                let reply_md = store_result.thread_path.join(".jyc").join("reply.md");
                if reply_md.exists() {
                    tokio::fs::read_to_string(&reply_md).await.ok()
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
            let signal_path = store_result.thread_path.join(".jyc").join("reply-sent.flag");
            let attachments = read_signal_attachments(&signal_path, &store_result.thread_path).await;

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
            thread_manager.metrics.reply_by_tool(thread_name);
        } else {
            tracing::warn!("MCP tool signaled reply but no reply text available");
        }
    } else if let Some(ref text) = result.reply_text {
        tracing::info!(text_len = text.len(), "Fallback: sending AI text via outbound");
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
    }

    Ok(())
}

/// Read attachment filenames from the reply-sent.flag signal file.
/// Returns OutboundAttachment list, or None if no attachments.
async fn read_signal_attachments(
    signal_path: &std::path::Path,
    thread_path: &std::path::Path,
) -> Option<Vec<crate::channels::types::OutboundAttachment>> {
    let content = tokio::fs::read_to_string(signal_path).await.ok()?;
    let signal: serde_json::Value = serde_json::from_str(&content).ok()?;

    let filenames = signal.get("attachments")?.as_array()?;
    if filenames.is_empty() {
        return None;
    }

    let attachments: Vec<crate::channels::types::OutboundAttachment> = filenames
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
                "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "txt" | "md" => "text/plain",
                _ => "application/octet-stream",
            };
            crate::channels::types::OutboundAttachment {
                filename: filename.to_string(),
                path,
                content_type: content_type.to_string(),
            }
        })
        .collect();

    Some(attachments)
}

async fn initialize_thread_from_template(
    thread_path: &Path,
    template_name: &str,
    template_dir: &Path,
) -> Result<()> {
    if thread_path.exists() {
        return Ok(());
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
    
    tracing::info!(template = %template_name, "Thread initialized from template");
    
    Ok(())
}
