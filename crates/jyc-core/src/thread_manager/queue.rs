//! `ThreadManager` impl block: queue.rs methods.
//!
//! Extracted from the monolithic `thread_manager.rs`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::thread_event::ThreadEvent;
use crate::thread_event_bus::ThreadEventBusRef;

use jyc_types::InboundAttachmentConfig;
use jyc_types::{InboundMessage, PatternMatch, QueueItem};

use super::ThreadManager;
/// Per-thread queue stats.
use super::template::{TemplateMismatch, initialize_thread_from_template};
use super::worker::process_message;

impl ThreadManager {
    pub async fn enqueue(
        &self,
        message: InboundMessage,
        thread_name: String,
        pattern_match: PatternMatch,
        attachment_config: Option<InboundAttachmentConfig>,
        live_injection: bool,
        thread_path_override: Option<PathBuf>,
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

        // Capture data for IncomingMessage event before `message` is moved.
        // Use full text — no truncation — so dashboard dedup between history
        // and recent_messages works correctly.
        let event_sender = message.sender.clone();
        let event_text = message.content.text.clone().unwrap_or_default();

        let template = message
            .metadata
            .get("template")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        tracing::debug!(
            thread = %thread_name,
            ?template,
            "ThreadManager::enqueue: extracted template from message metadata"
        );

        // Resolve thread_path_override: prefer the ThreadManager's registered
        // custom path (set by a prior message for this thread, e.g. an ad-hoc
        // create_thread via jyc dashboard open) over the router-provided value
        // (which may be a pattern fallback that doesn't apply to this specific
        // thread instance).
        let thread_path_override = self
            .thread_paths
            .lock()
            .await
            .get(&thread_name)
            .cloned()
            .or(thread_path_override);

        let item = QueueItem {
            thread_name: thread_name.clone(),
            message,
            pattern_match,
            attachment_config,
            template,
            live_injection,
            thread_path_override,
        };

        self.metrics.message_received(&thread_name);

        if let Some(sender) = queues.get(&thread_name) {
            match sender.try_send(item) {
                Ok(()) => {
                    tracing::debug!(thread = %thread_name, "Message enqueued");
                    self.publish_incoming_message(&thread_name, &event_sender, &event_text)
                        .await;
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
                    self.create_and_enqueue(&mut queues, thread_name.clone(), item)
                        .await;
                    self.publish_incoming_message(&thread_name, &event_sender, &event_text)
                        .await;
                    return;
                }
            }
        }

        self.create_and_enqueue(&mut queues, thread_name.clone(), item)
            .await;
        self.publish_incoming_message(&thread_name, &event_sender, &event_text)
            .await;
    }

    /// Build the per-worker ThreadManager clone used inside a thread worker.
    ///
    /// Shares `thread_cancels` (and other Arc-backed fields) with the parent
    /// so /cancel and /close invoked through the command registry reach the
    /// running worker's real cancellation token. A fresh empty map here made
    /// /cancel report success without cancelling anything.
    pub(crate) fn worker_clone(&self) -> ThreadManager {
        ThreadManager {
            thread_queues: Mutex::new(HashMap::new()),
            semaphore: self.semaphore.clone(),
            max_queue_size: self.max_queue_size,
            storage: self.storage.clone(),
            outbound: self.outbound.clone(),
            agent: self.agent.clone(),
            event_buses: Mutex::new(HashMap::new()),
            enable_events: self.enable_events,
            thread_cancels: self.thread_cancels.clone(),
            template_dirs: self.template_dirs.clone(),
            channel_name: self.channel_name.clone(),
            channel_type: self.channel_type.clone(),
            workdir: self.workdir.clone(),
            workspace_dir: self.workspace_dir.clone(),
            config: self.config.clone(),
            config_path: self.config_path.clone(),
            metrics: self.metrics.clone(),
            cancel: self.cancel.clone(),
            worker_handles: Mutex::new(vec![]),
            repo_group_locks: self.repo_group_locks.clone(),
            thread_paths: self.thread_paths.clone(),
        }
    }

    async fn create_and_enqueue(
        &self,
        queues: &mut HashMap<String, mpsc::Sender<QueueItem>>,
        thread_name: String,
        item: QueueItem,
    ) {
        let (tx, rx) = mpsc::channel(self.max_queue_size);
        let _ = tx.try_send(item);
        // Clone the sender for re-enqueuing buffered messages within
        // process_message(). Direct try_send bypasses the clone's enqueue()
        // path entirely, avoiding creation of orphaned event buses or stale
        // sender entries in the clone's thread_queues.
        let tx_for_reenqueue = tx.clone();
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

        let tm = Arc::new(self.worker_clone());

        // Share the event bus with the clone so publish_reply_sent() can find it.
        // The event bus was created in `self` (the original ThreadManager), but
        // publish_reply_sent() looks up the bus via `self.get_event_bus()` on the
        // worker's clone, which has an empty event_buses HashMap. Without this,
        // ReplySent events are silently dropped and the dashboard never sees them.
        if let Some(ref bus) = event_bus {
            tm.event_buses
                .lock()
                .await
                .insert(thread_name.clone(), bus.clone());
        }

        let handle = ThreadManager::spawn_worker(
            tm,
            thread_name,
            rx,
            tx_for_reenqueue,
            event_bus,
            thread_cancel,
        );

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
        tx_for_reenqueue: mpsc::Sender<QueueItem>,
        event_bus: Option<ThreadEventBusRef>,
        thread_cancel: CancellationToken,
    ) -> JoinHandle<()> {
        let semaphore = thread_manager.semaphore.clone();
        let cancel = thread_manager.cancel.clone();
        let storage = thread_manager.storage.clone();
        let outbound = thread_manager.outbound.clone();
        let agent = thread_manager.agent.clone();
        let template_dirs = thread_manager.template_dirs.clone();
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

                // Compute the effective thread path (custom override or default)
                let workspace = storage.workspace();
                let thread_path = item
                    .thread_path_override
                    .clone()
                    .unwrap_or_else(|| workspace.join(&thread_name));

                // Store the resolved thread path so it can be returned by
                // list_threads() and used by the ActivityTracker.
                {
                    let mut paths = tm.thread_paths.lock().await;
                    paths.insert(thread_name.clone(), thread_path.clone());
                }

                // Initialize thread from template if needed
                if let Some(ref template_name) = item.template {
                    let thread_path = thread_path.clone();

                    match initialize_thread_from_template(
                        &thread_path,
                        template_name,
                        &template_dirs,
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
                }

                // Always ensure .jyc/ directory and metadata files exist,
                // even when no template is configured. This is critical for
                // custom thread_path directories to be rediscovered after
                // restart (via .jyc/thread-name).
                let jyc_dir = thread_path.join(".jyc");
                if let Err(e) = tokio::fs::create_dir_all(&jyc_dir).await {
                    tracing::warn!(error = %e, "Failed to create .jyc directory");
                }
                // Injected messages (jyc_send_to_thread, dashboard thread
                // proxy) carry an empty pattern_name when no pattern could be
                // resolved; don't let them erase the thread's real pattern.
                if !item.pattern_match.pattern_name.is_empty() {
                    let pattern_file = jyc_dir.join("pattern");
                    if let Err(e) = tokio::fs::write(&pattern_file, &item.pattern_match.pattern_name).await {
                        tracing::warn!(error = %e, "Failed to write pattern file");
                    }
                }
                // Persist the logical thread name so custom thread_path
                // directories can be rediscovered after restart.
                let thread_name_file = jyc_dir.join("thread-name");
                if let Err(e) = tokio::fs::write(&thread_name_file, &thread_name).await {
                    tracing::warn!(error = %e, "Failed to write thread-name file");
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

                let processing_started = std::time::Instant::now();
                if let Err(e) = process_message(
                    &mut item,
                    &thread_name,
                    &storage,
                    outbound.clone(),
                    agent.clone(),
                    &mut rx,
                    &template_dirs,
                    &config,
                    &tx_for_reenqueue,
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
                        let duration_secs = processing_started.elapsed().as_secs();
                        tokio::spawn(async move {
                            // The error report is always followed by
                            // ProcessingCompleted: consumers (inspect server,
                            // dashboard) clear their "processing" state only on
                            // that event, and an error exit from the agent loop
                            // never publishes one. Without it the dashboard stays
                            // stuck at "AI thinking..." forever. Duplicate
                            // completions are idempotent.
                            let events = [
                                ThreadEvent::SessionStatus {
                                    thread_name: thread_name_clone.clone(),
                                    status_type: "error".to_string(),
                                    attempt: None,
                                    message: Some(truncated),
                                    timestamp: chrono::Utc::now(),
                                },
                                ThreadEvent::ProcessingCompleted {
                                    thread_name: thread_name_clone,
                                    message_id: "processing-error".to_string(),
                                    success: false,
                                    duration_secs,
                                    timestamp: chrono::Utc::now(),
                                },
                            ];
                            for event in events {
                                if let Err(publish_err) = event_bus.publish(event).await {
                                    tracing::trace!("Failed to publish event: {}", publish_err);
                                }
                            }
                        });
                    }
                }

                // Check symlink integrity after AI processing completes
                if let Some(repo_group_key) = item.message.metadata.get("repo_group_key").and_then(|v| v.as_str()) {
                    let thread_path = item
                        .thread_path_override
                        .clone()
                        .unwrap_or_else(|| storage.workspace().join(&thread_name));
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
}
