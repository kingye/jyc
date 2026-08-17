//! `TopicManager` impl block: queue.rs methods.
//!
//! Extracted from the monolithic `topic_manager.rs`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::topic_event::TopicEvent;
use crate::topic_event_bus::TopicEventBusRef;

use jyc_types::InboundAttachmentConfig;
use jyc_types::{InboundMessage, PatternMatch, QueueItem};

use super::TopicManager;
/// Per-topic queue stats.
use super::template::{TemplateMismatch, initialize_topic_from_template};
use super::worker::process_message;

impl TopicManager {
    pub async fn enqueue(
        &self,
        message: InboundMessage,
        topic_name: String,
        pattern_match: PatternMatch,
        attachment_config: Option<InboundAttachmentConfig>,
        live_injection: bool,
        topic_path_override: Option<PathBuf>,
    ) {
        let mut queues = self.topic_queues.lock().await;

        // Periodic cleanup: remove closed senders to prevent unbounded HashMap growth.
        // This is cheap (O(n) scan) and only retains senders that are still open.
        let mut closed_topics = Vec::new();
        queues.retain(|name, sender| {
            let is_open = !sender.is_closed();
            if !is_open {
                closed_topics.push(name.clone());
            }
            is_open
        });

        // Clean up event buses for closed topics
        if !closed_topics.is_empty() && self.enable_events {
            let mut event_buses = self.event_buses.lock().await;
            for topic_name in closed_topics {
                event_buses.remove(&topic_name);
                tracing::debug!(topic = %topic_name, "Cleaned up event bus for closed topic");
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
            topic = %topic_name,
            ?template,
            "TopicManager::enqueue: extracted template from message metadata"
        );

        // Resolve topic_path_override: prefer the TopicManager's registered
        // custom path (set by a prior message for this topic, e.g. an ad-hoc
        // create_topic via jyc dashboard open) over the router-provided value
        // (which may be a pattern fallback that doesn't apply to this specific
        // topic instance).
        let topic_path_override = self
            .topic_paths
            .lock()
            .await
            .get(&topic_name)
            .cloned()
            .or(topic_path_override);

        let item = QueueItem {
            topic_name: topic_name.clone(),
            message,
            pattern_match,
            attachment_config,
            template,
            live_injection,
            topic_path_override,
        };

        self.metrics.message_received(&topic_name);

        if let Some(sender) = queues.get(&topic_name) {
            match sender.try_send(item) {
                Ok(()) => {
                    tracing::debug!(topic = %topic_name, "Message enqueued");
                    self.publish_incoming_message(&topic_name, &event_sender, &event_text)
                        .await;
                    return;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(topic = %topic_name, "Queue full, dropping message");
                    self.metrics.queue_dropped(&topic_name);
                    return;
                }
                Err(mpsc::error::TrySendError::Closed(item)) => {
                    queues.remove(&topic_name);
                    // Clean up event bus for this topic
                    if self.enable_events {
                        let mut event_buses = self.event_buses.lock().await;
                        event_buses.remove(&topic_name);
                        tracing::debug!(topic = %topic_name, "Cleaned up event bus for closed queue");
                    }
                    self.create_and_enqueue(&mut queues, topic_name.clone(), item)
                        .await;
                    self.publish_incoming_message(&topic_name, &event_sender, &event_text)
                        .await;
                    return;
                }
            }
        }

        self.create_and_enqueue(&mut queues, topic_name.clone(), item)
            .await;
        self.publish_incoming_message(&topic_name, &event_sender, &event_text)
            .await;
    }

    /// Build the per-worker TopicManager clone used inside a topic worker.
    ///
    /// Shares `topic_cancels` (and other Arc-backed fields) with the parent
    /// so /cancel and /close invoked through the command registry reach the
    /// running worker's real cancellation token. A fresh empty map here made
    /// /cancel report success without cancelling anything.
    pub(crate) fn worker_clone(&self) -> TopicManager {
        TopicManager {
            topic_queues: Mutex::new(HashMap::new()),
            semaphore: self.semaphore.clone(),
            max_queue_size: self.max_queue_size,
            storage: self.storage.clone(),
            outbound: self.outbound.clone(),
            outbounds: self.outbounds.clone(),
            agent: self.agent.clone(),
            event_buses: Mutex::new(HashMap::new()),
            enable_events: self.enable_events,
            topic_cancels: self.topic_cancels.clone(),
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
            topic_paths: self.topic_paths.clone(),
        }
    }

    async fn create_and_enqueue(
        &self,
        queues: &mut HashMap<String, mpsc::Sender<QueueItem>>,
        topic_name: String,
        item: QueueItem,
    ) {
        let (tx, rx) = mpsc::channel(self.max_queue_size);
        let _ = tx.try_send(item);
        // Clone the sender for re-enqueuing buffered messages within
        // process_message(). Direct try_send bypasses the clone's enqueue()
        // path entirely, avoiding creation of orphaned event buses or stale
        // sender entries in the clone's topic_queues.
        let tx_for_reenqueue = tx.clone();
        queues.insert(topic_name.clone(), tx);

        // Create event bus for this topic if events are enabled
        let event_bus = if self.enable_events {
            self.get_or_create_event_bus(&topic_name).await
        } else {
            None
        };

        // Create per-topic cancellation token so close_topic can stop this worker
        let topic_cancel = CancellationToken::new();
        {
            let mut cancels = self.topic_cancels.lock().await;
            cancels.insert(topic_name.clone(), topic_cancel.clone());
        }

        let tm = Arc::new(self.worker_clone());

        // Share the event bus with the clone so publish_reply_sent() can find it.
        // The event bus was created in `self` (the original TopicManager), but
        // publish_reply_sent() looks up the bus via `self.get_event_bus()` on the
        // worker's clone, which has an empty event_buses HashMap. Without this,
        // ReplySent events are silently dropped and the dashboard never sees them.
        if let Some(ref bus) = event_bus {
            tm.event_buses
                .lock()
                .await
                .insert(topic_name.clone(), bus.clone());
        }

        let handle = TopicManager::spawn_worker(
            tm,
            topic_name,
            rx,
            tx_for_reenqueue,
            event_bus,
            topic_cancel,
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
        topic_manager: Arc<TopicManager>,
        topic_name: String,
        mut rx: mpsc::Receiver<QueueItem>,
        tx_for_reenqueue: mpsc::Sender<QueueItem>,
        event_bus: Option<TopicEventBusRef>,
        topic_cancel: CancellationToken,
    ) -> JoinHandle<()> {
        let semaphore = topic_manager.semaphore.clone();
        let cancel = topic_manager.cancel.clone();
        let storage = topic_manager.storage.clone();
        let outbound = topic_manager.outbound.clone();
        let agent = topic_manager.agent.clone();
        let template_dirs = topic_manager.template_dirs.clone();
        let config = topic_manager.config.clone();
        let tm = topic_manager;
        let tm_span = tracing::info_span!("tm", t = %topic_name);

        tokio::spawn(async move {
            let mut _permit = tokio::select! {
                permit = semaphore.clone().acquire_owned() => match permit {
                    Ok(p) => p,
                    Err(_) => return,
                },
                _ = cancel.cancelled() => return,
                _ = topic_cancel.cancelled() => return,
            };

            tracing::info!("Worker started");

            // Set topic event bus for agent service
            let _ = agent.set_topic_event_bus(&topic_name, event_bus.clone()).await;

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
                        _ = topic_cancel.cancelled() => {
                            tracing::info!("Worker cancelled (topic closed)");
                            break;
                        }
                    },
                };

                // Compute the effective topic path (custom override or default)
                let workspace = storage.workspace();
                let topic_path = item
                    .topic_path_override
                    .clone()
                    .unwrap_or_else(|| workspace.join(&topic_name));

                // Store the resolved topic path so it can be returned by
                // list_topics() and used by the ActivityTracker.
                {
                    let mut paths = tm.topic_paths.lock().await;
                    paths.insert(topic_name.clone(), topic_path.clone());
                }

                // Initialize topic from template if needed
                if let Some(ref template_name) = item.template {
                    let topic_path = topic_path.clone();

                    match initialize_topic_from_template(
                        &topic_path,
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
                                    topic = %topic_name,
                                    template = %template_name,
                                    "Template mismatch on existing topic; dropping message. \
                                     Two patterns likely share a topic_prefix but use different templates."
                                );
                                tm.metrics.processing_error(&topic_name, "template_mismatch");
                                continue;
                            }
                            tracing::warn!(
                                error = %e,
                                template = %template_name,
                                "Failed to initialize topic from template"
                            );
                        }
                    }

                    if let Some(repo_group_key) = item.message.metadata.get("repo_group_key").and_then(|v| v.as_str()) {
                        let shared_repo_dir = crate::topic_path::resolve_shared_repo_dir(workspace, repo_group_key);
                        let symlink_path = topic_path.join("repo");

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
                                    topic = %topic_name,
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
                // custom topic_path directories to be rediscovered after
                // restart (via .jyc/topic-name).
                let jyc_dir = topic_path.join(".jyc");
                if let Err(e) = tokio::fs::create_dir_all(&jyc_dir).await {
                    tracing::warn!(error = %e, "Failed to create .jyc directory");
                }
                // Injected messages (jyc_send_to_topic, dashboard topic
                // proxy) carry an empty pattern_name when no pattern could be
                // resolved; don't let them erase the topic's real pattern.
                if !item.pattern_match.pattern_name.is_empty() {
                    let pattern_file = jyc_dir.join("pattern");
                    if let Err(e) = tokio::fs::write(&pattern_file, &item.pattern_match.pattern_name).await {
                        tracing::warn!(error = %e, "Failed to write pattern file");
                    }
                }
                // Persist the logical topic name so custom topic_path
                // directories can be rediscovered after restart.
                let topic_name_file = jyc_dir.join("topic-name");
                if let Err(e) = tokio::fs::write(&topic_name_file, &topic_name).await {
                    tracing::warn!(error = %e, "Failed to write topic-name file");
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
                    let shared_repo_dir = crate::topic_path::resolve_shared_repo_dir(workspace, repo_group_key);

                    if let Ok(guard) = lock.clone().try_lock_owned() {
                        let is_empty = match tokio::fs::read_dir(&shared_repo_dir).await {
                            Ok(mut entries) => entries.next_entry().await.unwrap_or(None).is_none(),
                            Err(_) => true,
                        };

                        if is_empty {
                            tracing::info!(
                                topic = %topic_name,
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
                                topic = %topic_name,
                                group_key = %repo_group_key,
                                "Shared repo dir already initialized, proceeding immediately"
                            );
                            drop(guard);
                        }
                    } else {
                        tracing::info!(
                            topic = %topic_name,
                            group_key = %repo_group_key,
                            "Repo group lock held by another worker, waiting..."
                        );
                        let _guard = lock.lock().await;
                        tracing::info!(
                            topic = %topic_name,
                            group_key = %repo_group_key,
                            "Repo group lock acquired, proceeding"
                        );
                    }
                }

                let processing_started = std::time::Instant::now();
                if let Err(e) = process_message(
                    &mut item,
                    &topic_name,
                    &storage,
                    outbound.clone(),
                    agent.clone(),
                    &mut rx,
                    &template_dirs,
                    &config,
                    &tx_for_reenqueue,
                    tm.clone(),
                    topic_cancel.clone(),
                ).await {
                    let err_display = format!("{:#}", e);
                    tracing::error!(
                        error = %err_display,
                        "Failed to process message"
                    );
                    tm.metrics.processing_error(&topic_name, &err_display);

                    if let Some(event_bus) = event_bus_for_error.clone() {
                        let truncated: String = err_display.chars().take(200).collect();
                        let topic_name_clone = topic_name.clone();
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
                                TopicEvent::SessionStatus {
                                    topic_name: topic_name_clone.clone(),
                                    status_type: "error".to_string(),
                                    attempt: None,
                                    message: Some(truncated),
                                    timestamp: chrono::Utc::now(),
                                },
                                TopicEvent::ProcessingCompleted {
                                    topic_name: topic_name_clone,
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
                    let topic_path = item
                        .topic_path_override
                        .clone()
                        .unwrap_or_else(|| storage.workspace().join(&topic_name));
                    let symlink_path = topic_path.join("repo");
                    match tokio::fs::symlink_metadata(&symlink_path).await {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            // Symlink intact — good
                        }
                        Ok(_) => {
                            tracing::warn!(
                                topic = %topic_name,
                                group_key = %repo_group_key,
                                path = %symlink_path.display(),
                                "Shared repo symlink was replaced by a regular directory (agent likely ran rm -rf repo && mkdir repo)"
                            );
                        }
                        Err(_) => {
                            tracing::warn!(
                                topic = %topic_name,
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
                    _ = topic_cancel.cancelled() => {
                        tracing::info!("Worker cancelled (topic closed)");
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
                    _ = topic_cancel.cancelled() => {
                        tracing::trace!("Worker cancelled (topic closed) while waiting for permit after receiving message");
                        break;
                    }
                };

                pending = Some(next);
            }

            tracing::info!("Worker finished");
        }.instrument(tm_span))
    }
}
