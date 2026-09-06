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
        //
        // Event buses are deliberately NOT removed here. Subscribers (TUI WS
        // relay, Feishu forwarder) hold receivers that outlive any single
        // worker — a worker respawned later in this call must find the same
        // bus, or those subscribers are orphaned on a dead channel and the
        // topic's event stream goes silent. Bus removal happens in
        // cleanup_topic_state (/close), where teardown is intentional.
        queues.retain(|_, sender| !sender.is_closed());

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
                    // Worker exited (e.g. after /cancel) and dropped its
                    // receiver. Respawn below — but keep the existing event
                    // bus: long-lived subscribers (TUI WS relay, Feishu
                    // forwarder) hold receivers of it, so recreating the bus
                    // here would orphan them and silence the topic's event
                    // stream. Bus cleanup belongs to cleanup_topic_state.
                    queues.remove(&topic_name);
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
            topic_paths: self.topic_paths.clone(),
            topic_patterns: self.topic_patterns.clone(),
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
        let tm = topic_manager.clone();
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
                    // Update the in-memory pattern cache so the
                    // inspect server's list_topics sees the same
                    // value without re-reading disk. Single source
                    // of truth for "what pattern does this topic
                    // belong to" — worker writes, inspect server
                    // reads.
                    topic_manager
                        .set_topic_pattern(&topic_path, &item.pattern_match.pattern_name)
                        .await;
                }
                // Persist the logical topic name so custom topic_path
                // directories can be rediscovered after restart.
                let topic_name_file = jyc_dir.join("topic-name");
                if let Err(e) = tokio::fs::write(&topic_name_file, &topic_name).await {
                    tracing::warn!(error = %e, "Failed to write topic-name file");
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
