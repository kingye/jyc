use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::channels::email::outbound::EmailOutboundAdapter;
use crate::channels::types::{AttachmentConfig, InboundMessage, PatternMatch};
use crate::config::types::AgentConfig;
use crate::core::command::handler::CommandContext;
use crate::core::command::model_handler::ModelCommandHandler;
use crate::core::command::mode_handler::{BuildCommandHandler, PlanCommandHandler};
use crate::core::command::registry::CommandRegistry;
use crate::core::email_parser;
use crate::core::message_storage::{MessageStorage, StoreResult};
use crate::services::agent::AgentService;

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
/// Responsible for:
/// - Queue management, concurrency control (semaphore + mpsc)
/// - Storing received messages (received.md)
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
    outbound: Arc<EmailOutboundAdapter>,
    agent: Arc<dyn AgentService>,

    cancel: CancellationToken,
    worker_handles: Mutex<Vec<JoinHandle<()>>>,
}

impl ThreadManager {
    pub fn new(
        max_concurrent: usize,
        max_queue_size: usize,
        storage: Arc<MessageStorage>,
        outbound: Arc<EmailOutboundAdapter>,
        agent: Arc<dyn AgentService>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            thread_queues: Mutex::new(HashMap::new()),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_queue_size,
            storage,
            outbound,
            agent,
            cancel,
            worker_handles: Mutex::new(Vec::new()),
        }
    }

    /// Enqueue a message for processing in the given thread.
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

        if let Some(sender) = queues.get(&thread_name) {
            match sender.try_send(item) {
                Ok(()) => {
                    tracing::debug!(thread = %thread_name, "Message enqueued");
                    return;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(thread = %thread_name, "Queue full, dropping message");
                    return;
                }
                Err(mpsc::error::TrySendError::Closed(item)) => {
                    queues.remove(&thread_name);
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
        let agent = self.agent.clone();

        tokio::spawn(async move {
            let _permit = tokio::select! {
                permit = semaphore.acquire_owned() => match permit {
                    Ok(p) => p,
                    Err(_) => return,
                },
                _ = cancel.cancelled() => return,
            };

            let mut channel_name: Option<String> = None;

            tracing::info!(thread = %thread_name, "Worker started");

            loop {
                let item = tokio::select! {
                    item = rx.recv() => match item {
                        Some(item) => item,
                        None => break,
                    },
                    _ = cancel.cancelled() => {
                        let ch = channel_name.as_deref().unwrap_or("-");
                        tracing::info!(channel = %ch, thread = %thread_name, "Worker cancelled");
                        break;
                    }
                };

                // Capture channel name from first message
                if channel_name.is_none() {
                    channel_name = Some(item.message.channel.clone());
                }
                let ch = channel_name.as_deref().unwrap_or("-");

                if let Err(e) = process_message(
                    &item,
                    &thread_name,
                    &storage,
                    &outbound,
                    agent.as_ref(),
                ).await {
                    tracing::error!(
                        channel = %ch,
                        thread = %thread_name,
                        error = %e,
                        "Failed to process message"
                    );
                }
            }

            let ch = channel_name.as_deref().unwrap_or("-");
            tracing::info!(channel = %ch, thread = %thread_name, "Worker finished");
        })
    }

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

    pub async fn shutdown(&self) {
        self.cancel.cancel();
        {
            let mut queues = self.thread_queues.lock().await;
            queues.clear();
        }
        let mut handles = self.worker_handles.lock().await;
        for handle in handles.drain(..) {
            let _ = handle.await;
        }
        tracing::info!("All workers shut down");
    }
}

/// Process a single message within a worker.
///
/// Flow:
/// 1. STORE → received.md
/// 2. COMMAND PROCESS → parse, execute, strip
/// 3. REPLY COMMAND RESULTS → direct reply (if commands found)
/// 4. CHECK BODY → if empty after commands + quoted history stripping → stop
/// 5. DISPATCH TO AGENT → agent.process() handles everything
async fn process_message(
    item: &QueueItem,
    thread_name: &str,
    storage: &MessageStorage,
    outbound: &EmailOutboundAdapter,
    agent: &dyn AgentService,
) -> Result<()> {
    let message = &item.message;
    let ch = &message.channel;

    // ── 1. STORE ──────────────────────────────────────────────────────
    let store_result: StoreResult = storage
        .store(message, thread_name, item.attachment_config.as_ref())
        .await?;

    tracing::info!(
        channel = %ch,
        thread = %thread_name,
        sender = %message.sender_address,
        topic = %message.topic,
        "Message stored"
    );

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

    let cmd_context = CommandContext {
        args: vec![],
        thread_path: store_result.thread_path.clone(),
        config: Arc::new(crate::config::load_config_from_str(
            "[general]\n[agent]\nenabled = true\nmode = \"opencode\""
        ).unwrap()),
        channel: message.channel.clone(),
    };

    let cmd_output = command_registry
        .process_commands(raw_body, &cmd_context)
        .await?;

    // ── 3. REPLY COMMAND RESULTS (always, if commands found) ──────────
    if !cmd_output.results.is_empty() {
        let summary = cmd_output.results_summary();
        tracing::info!(
            channel = %ch,
            thread = %thread_name,
            commands = cmd_output.results.len(),
            "Sending command results"
        );

        let full_reply = email_parser::build_full_reply_text(
            &summary,
            &store_result.thread_path,
            &message.sender,
            &message.timestamp.to_rfc3339(),
            &message.topic,
            raw_body,
            &store_result.message_dir,
        )
        .await;

        outbound.send_reply(message, &full_reply, None).await?;
        storage
            .store_reply(&store_result.thread_path, &full_reply, &store_result.message_dir)
            .await?;
    }

    // ── 4. CHECK BODY ─────────────────────────────────────────────────
    let cleaned_body = email_parser::strip_quoted_history(&cmd_output.cleaned_body);
    let effective_body_empty = cleaned_body.trim().is_empty();

    tracing::debug!(
        thread = %thread_name,
        body_empty = effective_body_empty,
        cleaned_len = cleaned_body.trim().len(),
        "Body check after command + quote stripping"
    );

    if effective_body_empty {
        tracing::info!(
            channel = %ch,
            thread = %thread_name,
            "No message body, stopping (no AI)"
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

    let result = agent
        .process(&message, thread_name, &store_result.thread_path, &store_result.message_dir)
        .await?;

    tracing::info!(
        channel = %ch,
        thread = %thread_name,
        reply_sent = result.reply_sent,
        summary = %result.summary,
        "Agent complete"
    );

    Ok(())
}
