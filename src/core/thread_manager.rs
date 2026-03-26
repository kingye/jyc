use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::channels::email::outbound::EmailOutboundAdapter;
use crate::channels::types::{AttachmentConfig, InboundMessage, PatternMatch};
use crate::config::types::AgentConfig;
use crate::core::email_parser;
use crate::core::message_storage::{MessageStorage, StoreResult};

/// An item in a thread's message queue.
struct QueueItem {
    message: InboundMessage,
    pattern_match: PatternMatch,
    attachment_config: Option<AttachmentConfig>,
}

/// Per-thread queue stats.
#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    pub active_workers: usize,
    pub total_threads: usize,
    pub pending_messages: usize,
}

/// Manages per-thread message queues with bounded concurrency.
///
/// Each thread gets its own mpsc channel (FIFO within a conversation).
/// A tokio Semaphore limits the number of concurrent worker tasks.
pub struct ThreadManager {
    /// Per-thread bounded mpsc senders
    thread_queues: Mutex<HashMap<String, mpsc::Sender<QueueItem>>>,

    /// Bounds concurrent thread workers
    semaphore: Arc<Semaphore>,

    /// Configuration
    max_queue_size: usize,

    /// Shared dependencies
    storage: Arc<MessageStorage>,
    outbound: Arc<EmailOutboundAdapter>,
    agent_config: Arc<AgentConfig>,

    /// Graceful shutdown
    cancel: CancellationToken,

    /// Worker join handles for cleanup
    worker_handles: Mutex<Vec<JoinHandle<()>>>,
}

impl ThreadManager {
    pub fn new(
        max_concurrent: usize,
        max_queue_size: usize,
        storage: Arc<MessageStorage>,
        outbound: Arc<EmailOutboundAdapter>,
        agent_config: Arc<AgentConfig>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            thread_queues: Mutex::new(HashMap::new()),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_queue_size,
            storage,
            outbound,
            agent_config,
            cancel,
            worker_handles: Mutex::new(Vec::new()),
        }
    }

    /// Enqueue a message for processing in the given thread.
    ///
    /// If the thread's queue doesn't exist, creates it and spawns a worker.
    /// If the queue is full, the message is dropped with a warning.
    pub async fn enqueue(
        &self,
        message: InboundMessage,
        thread_name: String,
        pattern_match: PatternMatch,
        attachment_config: Option<AttachmentConfig>,
    ) {
        let mut queues = self.thread_queues.lock().await;

        let item = QueueItem {
            message,
            pattern_match,
            attachment_config,
        };

        // Try to send to existing thread queue
        if let Some(sender) = queues.get(&thread_name) {
            match sender.try_send(item) {
                Ok(()) => {
                    tracing::debug!(thread = %thread_name, "Message enqueued");
                    return;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(
                        thread = %thread_name,
                        "Queue full, dropping message"
                    );
                    return;
                }
                Err(mpsc::error::TrySendError::Closed(item)) => {
                    // Worker finished, remove stale queue and recreate below
                    queues.remove(&thread_name);
                    // Re-send below after creating new queue
                    self.create_and_enqueue(&mut queues, thread_name, item)
                        .await;
                    return;
                }
            }
        }

        // No existing queue — create one
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

        let handle = self.spawn_worker(thread_name, rx);
        self.worker_handles.lock().await.push(handle);
    }

    fn spawn_worker(
        &self,
        thread_name: String,
        mut rx: mpsc::Receiver<QueueItem>,
    ) -> JoinHandle<()> {
        let semaphore = self.semaphore.clone();
        let cancel = self.cancel.clone();
        let storage = self.storage.clone();
        let outbound = self.outbound.clone();
        let agent_config = self.agent_config.clone();

        tokio::spawn(async move {
            // Acquire semaphore permit (blocks if all workers busy)
            let _permit = tokio::select! {
                permit = semaphore.acquire_owned() => match permit {
                    Ok(p) => p,
                    Err(_) => return, // Semaphore closed
                },
                _ = cancel.cancelled() => return,
            };

            tracing::info!(thread = %thread_name, "Worker started");

            loop {
                let item = tokio::select! {
                    item = rx.recv() => match item {
                        Some(item) => item,
                        None => break, // Channel closed, queue drained
                    },
                    _ = cancel.cancelled() => {
                        tracing::info!(thread = %thread_name, "Worker cancelled");
                        break;
                    }
                };

                if let Err(e) = process_message(
                    &item,
                    &thread_name,
                    &storage,
                    &outbound,
                    &agent_config,
                )
                .await
                {
                    tracing::error!(
                        thread = %thread_name,
                        error = %e,
                        "Failed to process message"
                    );
                }
            }

            tracing::info!(thread = %thread_name, "Worker finished");
            // _permit dropped here → semaphore slot freed
        })
    }

    /// Get current queue statistics.
    pub async fn get_stats(&self) -> QueueStats {
        let queues = self.thread_queues.lock().await;
        let total_threads = queues.len();
        let active_workers = self.semaphore.available_permits();
        let max = total_threads; // rough estimate

        QueueStats {
            active_workers: max.saturating_sub(active_workers),
            total_threads,
            pending_messages: 0, // Can't peek into mpsc channels
        }
    }

    /// Wait for all workers to finish (for graceful shutdown).
    pub async fn shutdown(&self) {
        self.cancel.cancel();

        // Close all sender channels to signal workers
        {
            let mut queues = self.thread_queues.lock().await;
            queues.clear();
        }

        // Wait for all worker tasks
        let mut handles = self.worker_handles.lock().await;
        for handle in handles.drain(..) {
            let _ = handle.await;
        }

        tracing::info!("All workers shut down");
    }
}

/// Process a single message within a worker.
///
/// Current flow (Phase 3 — no AI yet):
/// 1. Store the inbound message
/// 2. If agent enabled with static mode → send static reply
/// 3. If agent enabled with opencode mode → log placeholder (Phase 4)
/// 4. If agent disabled → just store
async fn process_message(
    item: &QueueItem,
    thread_name: &str,
    storage: &MessageStorage,
    outbound: &EmailOutboundAdapter,
    agent_config: &AgentConfig,
) -> Result<()> {
    let message = &item.message;

    // 1. Store the inbound message
    let store_result: StoreResult = storage
        .store(message, thread_name, item.attachment_config.as_ref())
        .await?;

    tracing::info!(
        thread = %thread_name,
        message_dir = %store_result.message_dir,
        sender = %message.sender_address,
        topic = %message.topic,
        "Message stored, processing..."
    );

    // 2. Check if agent is enabled
    if !agent_config.enabled {
        tracing::info!(thread = %thread_name, "Agent disabled, skipping reply");
        return Ok(());
    }

    // 3. Handle reply mode
    match agent_config.mode.as_str() {
        "static" => {
            let reply_text = agent_config
                .text
                .as_deref()
                .unwrap_or("Thank you for your message. We will get back to you soon.");

            outbound.send_reply(message, reply_text, None).await?;

            storage
                .store_reply(
                    &store_result.thread_path,
                    reply_text,
                    &store_result.message_dir,
                )
                .await?;

            tracing::info!(
                thread = %thread_name,
                "Static reply sent"
            );
        }
        "opencode" => {
            // Phase 4: AI integration
            tracing::info!(
                thread = %thread_name,
                "OpenCode mode — AI reply not yet implemented (Phase 4)"
            );

            // For now, send a placeholder reply so the pipeline is testable
            let reply_text = format!(
                "**[JYC]** Message received and stored.\n\n\
                 > **From:** {}\n\
                 > **Subject:** {}\n\
                 > **Thread:** {}\n\n\
                 AI processing will be available in Phase 4.",
                message.sender_address, message.topic, thread_name
            );

            outbound.send_reply(message, &reply_text, None).await?;

            storage
                .store_reply(
                    &store_result.thread_path,
                    &reply_text,
                    &store_result.message_dir,
                )
                .await?;
        }
        other => {
            tracing::warn!(
                thread = %thread_name,
                mode = %other,
                "Unknown agent mode"
            );
        }
    }

    Ok(())
}
