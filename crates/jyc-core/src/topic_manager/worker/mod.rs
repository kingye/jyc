//! Per-message processing pipeline (`process_message`) and its helpers.
//!
//! Extracted from the monolithic `topic_manager.rs`; the worker is the
//! free-function half of the module, while `TopicManager` (struct + impl)
//! lives in `mod.rs`.

use anyhow::Result;
use arc_swap::ArcSwap;
use jyc_types::state_dir::resolve_state_dir;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentService;
use crate::command::builtin::builtin_registry;
use crate::command::custom_handler::CustomCommandHandler;
use crate::command::handler::CommandContext;
use crate::command::per_agent_commands;
use crate::command::registry::CommandRegistry;
use crate::command::skill_commands;
use crate::message_storage::{MessageStorage, StoreResult};
use crate::pending_delivery::{
    json_metadata, read_signal_attachments, reply_blocked_by_hook, watch_pending_deliveries,
};
use jyc_types::config::HookEvent;
use jyc_types::{OutboundAdapter, QueueItem};
use jyc_utils::hooks::{HookCtx, HookOutcome, HookSet};

use super::TopicManager;

/// How many messages may wait to be folded into a running turn. The
/// channel is drained in full at every drain point, so this only has to
/// cover the messages that arrive between two drain points.
const INJECTION_CHANNEL_CAPACITY: usize = 32;

/// Whether a queued message can join the running turn as it is.
///
/// Only plain text: an attachment, a template or a scheduled job needs
/// the full turn pipeline (its own storage, hooks and reply target).
///
/// A topic path is not a reason on its own: the worker registers the
/// topic's directory the first time it stores a message, so every
/// message after that carries one. What matters is where the message
/// would land — a message headed for a directory other than the running
/// turn's keeps a turn of its own.
fn is_injectable(item: &QueueItem, topic_path: &Path, workspace: &Path) -> bool {
    let effective_path = item
        .topic_path_override
        .clone()
        .unwrap_or_else(|| workspace.join(&item.topic_name));
    effective_path == topic_path
        && item.template.is_none()
        && item.message.attachments.is_empty()
        && !item.message.is_scheduled_job()
        && item
            .message
            .content
            .text
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
}

/// Ends the turn's injection count when the turn ends — the messages the
/// model had not read are gone with the turn, and a count that outlived
/// them would report a backlog the topic does not have.
///
/// The count belongs to the turn that took the messages in, and this guard
/// holds that turn's id: a turn that ends late (its topic was closed and
/// reopened while it was still running) finds the next turn's id in the
/// store and leaves it alone. Clearing the next turn's count would hide
/// its unread messages for the rest of that turn.
struct InTurnGuard {
    manager: Arc<TopicManager>,
    topic: String,
    turn_id: u64,
}

impl Drop for InTurnGuard {
    fn drop(&mut self) {
        self.manager.clear_topic_in_turn(&self.topic, self.turn_id);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_message(
    item: &mut QueueItem,
    topic_name: &str,
    storage: &MessageStorage,
    outbound: Arc<dyn OutboundAdapter>,
    agent: Arc<dyn AgentService>,
    pending_rx: &mut mpsc::Receiver<QueueItem>,
    template_dirs: &crate::template_dirs::TemplateDirs,
    config: &Arc<ArcSwap<jyc_types::AppConfig>>,
    tx_for_reenqueue: &mpsc::Sender<QueueItem>,
    topic_manager: Arc<TopicManager>,
    topic_cancel: CancellationToken,
) -> Result<()> {
    // ── 1. STORE ──────────────────────────────────────────────────────
    let is_matched = !item.pattern_match.pattern_name.is_empty();
    let store_result: StoreResult = match &item.topic_path_override {
        Some(path) => {
            storage
                .store_at_path(&item.message, topic_name, path, is_matched)
                .await?
        }
        None => {
            storage
                .store_with_match(
                    &item.message,
                    topic_name,
                    is_matched,
                    item.attachment_config.as_ref(),
                )
                .await?
        }
    };

    tracing::info!(
        sender = %item.message.sender_address,
        topic = %item.message.topic,
        "Message stored"
    );

    // Marks for the per-message dispatch path: everything from here to the
    // first poll of the agent future is otherwise invisible in the log, so a
    // slow turn cannot be attributed. One summary line below carries the
    // numbers; drop these marks once the breakdown is known.
    let t_prelude = std::time::Instant::now();

    // ── 1.2. WRITE THREAD ROUTING METADATA ────────────────────────────
    // Persist routing metadata on the first message for a topic so that
    // the dashboard's `TopicProxyHandler` (via /ws/<channel>/<topic>)
    // can restore it when constructing a synthetic InboundMessage.
    // Without this, the proxy's InboundMessage has empty metadata and
    // channel-specific reply routing fails (e.g., github_number missing → 404).
    let topic_meta_path =
        resolve_state_dir(topic_name, &store_result.topic_path).join("topic-meta.json");
    let topic_is_new = !topic_meta_path.exists();
    if topic_is_new && item.message.channel_uid != "dashboard" {
        let meta = serde_json::json!({
            "channel_uid": item.message.channel_uid,
            "external_id": item.message.external_id,
            "references": item.message.references,
            "metadata": item.message.metadata,
            // Which channel routed here. The only record of it that outlives
            // the process, and what lets a topic idle since the last restart be
            // addressed again (`send_handler::recorded_channel`).
            "channel": item.message.channel,
        });
        if let Ok(json) = serde_json::to_string_pretty(&meta) {
            let _ =
                tokio::fs::create_dir_all(resolve_state_dir(topic_name, &store_result.topic_path))
                    .await;
            if let Err(e) = tokio::fs::write(&topic_meta_path, json).await {
                tracing::warn!(error = %e, "Failed to write topic-meta.json");
            } else {
                tracing::debug!(topic = %topic_name, "Wrote topic-meta.json");
            }
        }
    }

    // Backfill `channel` into a record written before the key existed. Without
    // this, such a topic is addressable again only while it is running its
    // queue — the next message makes it live for that process, but the key it
    // would need after the following restart is still absent, forever. Only
    // added when missing: the first routing owns the name, so a later message
    // on a topic two channels route never rewrites it.
    //
    // Dashboard traffic is included, unlike the first write above: its
    // `channel_uid` is a synthetic marker and is skipped there, but the channel
    // in the message is the topic's real one (the proxy is per
    // `/ws/<channel>/<topic>`), and it is only `channel` that is added here.
    if !topic_is_new {
        match tokio::fs::read_to_string(&topic_meta_path).await {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(mut meta) if meta.is_object() && meta.get("channel").is_none() => {
                    meta["channel"] = serde_json::json!(item.message.channel);
                    if let Ok(json) = serde_json::to_string_pretty(&meta)
                        && let Err(e) = tokio::fs::write(&topic_meta_path, json).await
                    {
                        tracing::warn!(error = %e, "Failed to backfill topic-meta.json");
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::debug!(
                    error = %e,
                    topic = %topic_name,
                    "topic-meta.json is not readable JSON; not backfilling `channel`"
                ),
            },
            Err(e) => tracing::debug!(
                error = %e,
                topic = %topic_name,
                "Could not read topic-meta.json; not backfilling `channel`"
            ),
        }
    }

    let t_meta = std::time::Instant::now();

    // ── 1.5. SAVE ATTACHMENTS ─────────────────────────────────────────
    // Save attachments AFTER topic name resolution (not before).
    // This ensures attachments go to the correct topic directory when
    // topic_name override is configured on the pattern.
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
            &store_result.topic_path,
            item.attachment_config.as_ref(),
        )
        .await
    {
        tracing::warn!(error = %e, "Failed to save attachments");
    }

    let t_attach = std::time::Instant::now();

    // From here on we only need a shared borrow of the message.
    let message = &item.message;

    // ── 2. COMMAND PROCESS ────────────────────────────────────────────
    let raw_body = message
        .content
        .text
        .as_deref()
        .or(message.content.markdown.as_deref())
        .unwrap_or("");

    // Every built-in handler, from the one table the `/` popup and `/?`
    // read too — see [`crate::command::builtin`].
    let mut command_registry = builtin_registry(topic_manager.clone());

    // User-defined commands: global `[[commands]]` first, then
    // `[[agents.<name>.commands]]` for the agent the topic is routed
    // to. Registration order matters — `CommandRegistry::register`
    // overwrites on collision with a `tracing::warn`, so a per-agent
    // command with the same name as a global one wins for that
    // agent's topics. Non-agent topics see only the global set.
    // `register_custom_commands` returns the per-agent slice it
    // registered, so the cmd_context below doesn't have to do the same
    // lookup a second time. Clone it so the inline-slash-command path
    // (further down in this function) can reuse the same value.
    let per_agent_commands = register_custom_commands(
        &config.load(),
        &item.pattern_match.pattern_name,
        &mut command_registry,
    );

    // `/skill:<name>`: one command per skill this topic's agent can resolve,
    // registered as the synthesized `[[commands]]` entry it is (see
    // [`skill_commands`]). A name already taken by config is skipped, so an
    // explicit `[[commands]] name = "skill:foo"` wins without the
    // warn-on-overwrite firing on every message.
    for cmd in skill_commands(&agent.available_skills(topic_name, &store_result.topic_path)) {
        let key = CustomCommandHandler::command_key(&cmd);
        if command_registry.get(&key).is_none() {
            command_registry.register(&key, Arc::new(CustomCommandHandler::new(cmd)));
        }
    }
    let t_after_prelude = std::time::Instant::now();

    // Hook set for this topic: global [[hooks]] + the routed agent's.
    // Built per message (same lifetime as the command registry), so a
    // config reload applies immediately. Empty for hook-free configs.
    let hooks = build_hook_set(&config.load(), &item.pattern_match.pattern_name);

    let cmd_context = CommandContext {
        topic_name: topic_name.to_string(),
        args: vec![],
        topic_path: store_result.topic_path.clone(),
        config: config.load_full(),
        channel: message.channel.clone(),
        channel_type: topic_manager.channel_type.clone(),
        agent: Some(agent.clone()),
        template_dirs: template_dirs.clone(),
        config_path: topic_manager.config_path.clone(),
        per_agent_commands: per_agent_commands.clone(),
        hooks: hooks.clone(),
    };

    let cmd_output = command_registry
        .process_commands(raw_body, &cmd_context)
        .await?;
    let t_after_commands = std::time::Instant::now();

    // ── 3. REPLY COMMAND RESULTS (always, if commands found) ──────────
    if !cmd_output.results.is_empty() {
        let summary = cmd_output.results_summary();
        tracing::info!(
            commands = cmd_output.results.len(),
            "Sending command results"
        );

        // Outbound adapter handles formatting + sending + storing
        let delivery = outbound
            .send_reply(
                message,
                &summary,
                &store_result.topic_path,
                &store_result.message_dir,
                None,
            )
            .await?;
        // Publish a ReplySent event so the inspect server's ActivityTracker
        // fans it out as a chat_message to dashboard WS clients. Without
        // this, command results are persisted to disk (visible on re-enter)
        // but never appear live in the chat pane. The adapter that logged
        // the row hands back the chat sequence number it used.
        topic_manager
            .publish_reply_sent(topic_name, &summary, delivery.chat_seq)
            .await;
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

    // ── 5. DISPATCH TO AGENT ──────────────────────────────────────────
    // Build message with cleaned body for agent processing
    let message = {
        let mut m = message.clone();
        m.content.text = Some(cleaned_body);
        m
    };

    // ── 5.0 EXTERNAL HOOKS: session_start + message_received ──────────
    // Notification `session_start(startup)` fires once for a brand-new
    // topic; blocking `message_received` gates the AI dispatch itself —
    // the message is already in the chat log, it just never reaches the
    // model. Command-only and question-answer messages returned earlier.
    // `session_start` is gated on the SAME condition as the topic-meta.json
    // write above (not the dashboard): dashboard-proxy messages never create
    // that file, so keying only on `topic_is_new` would re-fire startup on
    // every dashboard message. A startup is "the first real-channel message".
    let t_before_hooks = std::time::Instant::now();
    // The hook context of a message: what a hook sees about it. The
    // queued message that joins the running turn gets one too — the
    // hook gates what reaches the model, and an injected message
    // reaches the model just as much.
    let hook_ctx = |msg: &jyc_types::InboundMessage| HookCtx {
        topic: topic_name.to_string(),
        cwd: store_result.topic_path.display().to_string(),
        channel: Some(msg.channel.clone()),
        message_content: msg.content.text.clone(),
        sender: Some(msg.sender.clone()),
        sender_address: Some(msg.sender_address.clone()),
        metadata: json_metadata(&msg.metadata),
        ..Default::default()
    };
    if !hooks.is_empty() {
        if topic_is_new && message.channel_uid != "dashboard" {
            let mut sc = hook_ctx(&message);
            sc.source = Some("startup".into());
            hooks
                .run(HookEvent::SessionStart, Some("startup"), &sc)
                .await;
        }
        if let HookOutcome::Block(reason) = hooks
            .run(
                HookEvent::MessageReceived,
                Some(topic_name),
                &hook_ctx(&message),
            )
            .await
        {
            tracing::info!(
                topic = %topic_name,
                reason = %reason,
                "message_received hook blocked AI dispatch"
            );
            return Ok(());
        }
    }

    let t_after_hooks = std::time::Instant::now();

    // Spawn a background task to watch for pending reply deliveries.
    // When the agent loop cannot deliver directly (no live reply target, or
    // the direct send failed), it writes reply.md + reply-sent.flag. This
    // watcher detects them and delivers immediately via the outbound
    // adapter, without waiting for the SSE stream to complete.
    let delivery_cancel = tokio_util::sync::CancellationToken::new();
    let delivery_cancel_child = delivery_cancel.clone();
    let delivery_topic_path = store_result.topic_path.clone();
    let delivery_message_dir = store_result.message_dir.clone();
    let delivery_message = message.clone();
    let delivery_outbound = outbound.clone();
    let delivery_topic_manager = topic_manager.clone();
    let delivery_topic_name = topic_name.to_string();
    let delivery_hooks = hooks.clone();
    let delivery_handle = tokio::spawn(async move {
        let event_bus = delivery_topic_manager
            .get_event_bus(&delivery_topic_name)
            .await;
        watch_pending_deliveries(
            &delivery_topic_path,
            &delivery_message_dir,
            &delivery_message,
            &*delivery_outbound,
            delivery_hooks,
            delivery_cancel_child,
            event_bus,
            &delivery_topic_name,
        )
        .await;
    });

    // We monitor the queue ourselves during AI processing. Slash commands
    // (e.g. /cancel, /model) are executed immediately via the command
    // registry so the user expects instant feedback even during a 429
    // retry. A plain message joins the running turn when the pattern's
    // `live_injection` is on: it is handed to the agent through the
    // injection channel below, and the agent folds it into the
    // conversation at its next drain point. Otherwise it is re-enqueued
    // and processed after the current AI call completes.
    let (inject_tx, mut inject_rx) = mpsc::channel::<QueueItem>(INJECTION_CHANNEL_CAPACITY);

    // The store counts what the turn takes in, so the topic's backlog says
    // how much of the user's input the model has not seen yet. The count
    // ends with the turn, and only with that turn: the guard holds the
    // turn's id, so a turn that ends late — its topic was closed and
    // reopened while it was still running — cannot clear the next turn's
    // channel.
    let turn_id = topic_manager.track_topic_in_turn(topic_name, inject_tx.clone());
    let _in_turn_guard = InTurnGuard {
        manager: topic_manager.clone(),
        topic: topic_name.to_string(),
        turn_id,
    };

    let t_dispatch = std::time::Instant::now();
    tracing::info!(
        topic = %topic_name,
        // Spans: meta_ms = topic-meta backfill, attach_ms = attachment save,
        // registry_ms = built-in registry + custom + skill commands,
        // reply_ms = the command result send (`outbound.send_reply`, a real
        // channel call), hooks_ms = session_start + message_received only,
        // dispatch_ms = the whole post-store pre-agent path.
        meta_ms = t_meta.duration_since(t_prelude).as_millis() as u64,
        attach_ms = t_attach.duration_since(t_meta).as_millis() as u64,
        registry_ms = t_after_prelude.duration_since(t_attach).as_millis() as u64,
        commands_ms = t_after_commands.duration_since(t_after_prelude).as_millis() as u64,
        reply_ms = t_before_hooks.duration_since(t_after_commands).as_millis() as u64,
        hooks_ms = t_after_hooks.duration_since(t_before_hooks).as_millis() as u64,
        hooks = !hooks.is_empty(),
        dispatch_ms = t_dispatch.duration_since(t_prelude).as_millis() as u64,
        "Message dispatch path"
    );

    // Buffer messages that arrive during AI processing and do not join the
    // running turn. They are re-enqueued after the agent finishes,
    // preserving FIFO order.
    let mut buffered: Vec<QueueItem> = Vec::new();

    let result = {
        let agent_fut = agent.process(
            &message,
            topic_name,
            &store_result.topic_path,
            &store_result.message_dir,
            &mut inject_rx,
            topic_cancel.clone(),
        );
        tokio::pin!(agent_fut);

        loop {
            tokio::select! {
                    r = &mut agent_fut => {
                        // Do NOT use `break r?` here — an early `?` return would
                        // skip the background-task cleanup below and leak the
                        // progress updater (see the `let result = result?;`
                        // after the cleanup section).
                        break r;
                    }
                    msg = pending_rx.recv() => match msg {
                    Some(qi) => {
                        let text = qi.message.content.text.as_deref().unwrap_or("");
                        let trimmed = text.trim();
                        if trimmed.starts_with('/') {
                            // Execute slash command immediately via the command
                            // registry instead of buffering — the user expects
                            // instant feedback even during AI processing.
                            tracing::info!(
                                topic = %topic_name,
                                command = %trimmed,
                                "Executing slash command during AI processing"
                            );
                            let cmd_ctx = CommandContext {
                                args: vec![],
                                topic_name: topic_name.to_string(),
                                topic_path: store_result.topic_path.clone(),
                                config: config.load_full(),
                                channel: qi.message.channel.clone(),
                                channel_type: topic_manager.channel_type.clone(),
                                agent: Some(agent.clone()),
                                template_dirs: template_dirs.clone(),
                                config_path: topic_manager.config_path.clone(),
                                per_agent_commands: per_agent_commands.clone(),

                                hooks: Default::default(),};
                            match command_registry.process_commands(trimmed, &cmd_ctx).await {
                                Ok(output) => {
                                    if !output.results.is_empty() {
                                        let summary = output.results_summary();
                                        let delivery = outbound
                                            .send_reply(
                                                &qi.message,
                                                &summary,
                                                &store_result.topic_path,
                                                &store_result.message_dir,
                                                None,
                                            )
                                            .await
                                            .map_err(|e| {
                                                tracing::warn!(
                                                    error = %e,
                                                    "Failed to send command result during AI processing"
                                                );
                                            })
                                            .ok();
                                        // Publish ReplySent so the dashboard
                                        // sees the command result live (same as
                                        // the main command-result path above).
                                        topic_manager
                                            .publish_reply_sent(
                                                topic_name,
                                                &summary,
                                                delivery.map(|d| d.chat_seq).unwrap_or(0),
                                            )
                                            .await;
                                    }

                                    // A command may inject prompt text for the
                                    // agent (user-defined `[[commands]]` append
                                    // their `user_prompt`). The current agent call
                                    // is already running, so re-enqueue the
                                    // injected body to be processed after it
                                    // finishes — dropping it here would silently
                                    // lose the instruction while still reporting
                                    // success.
                                    //
                                    // The command line itself was stripped, so the
                                    // re-enqueued body no longer starts with `/`
                                    // and will not re-enter this branch.
                                    if !output.cleaned_body.trim().is_empty() {
                                        let mut requeued = qi;
                                        requeued.message.content.text =
                                            Some(output.cleaned_body.clone());
                                        requeued.message.content.markdown = None;
                                        tracing::info!(
                                            topic = %topic_name,
                                            "Re-enqueueing command-injected body for post-AI processing"
                                        );
                                        buffered.push(requeued);
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        command = %trimmed,
                                        "Command execution failed during AI processing"
                                    );
                                }
                            }
                        } else if item.live_injection
                            && is_injectable(&qi, &store_result.topic_path, storage.workspace())
                        {
                            // Live injection: the message joins the running turn
                            // instead of waiting for it to end. The agent folds
                            // it into the conversation at its next drain point,
                            // where the model sees it as a user message.
                            //
                            // The `message_received` hook gates what reaches the
                            // model, and an injected message reaches it just as
                            // much as a message that starts a turn: the hook
                            // runs here too, on the same body the model will
                            // read. A hook that blocks the message keeps it
                            // out of the running turn — it waits for a turn of
                            // its own, where the same hook blocks it again and
                            // the chat log records it.
                            let mut injected_ctx = hook_ctx(&qi.message);
                            injected_ctx.message_content =
                                Some(outbound.clean_body(qi.message.content.text.as_deref().unwrap_or("")));
                            let blocked = matches!(
                                hooks.run(
                                    HookEvent::MessageReceived,
                                    Some(topic_name),
                                    &injected_ctx
                                ).await,
                                HookOutcome::Block(_)
                            );
                            if blocked {
                                tracing::info!(
                                    topic = %topic_name,
                                    "message_received hook blocked an injected message; \
                                     it waits for a turn of its own"
                                );
                                buffered.push(qi);
                            } else {
                                match inject_tx.try_send(qi) {
                                    Ok(()) => {
                                        tracing::info!(
                                            topic = %topic_name,
                                            "Injecting a message into the running turn"
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            topic = %topic_name,
                                            error = %e,
                                            "Live injection failed; re-queueing the message"
                                        );
                                        buffered.push(e.into_inner());
                                    }
                                }
                            }
                        } else {
                            // Buffer non-command message for re-enqueue after AI finishes
                            buffered.push(qi);
                        }
                    }
                    None => {
                        // Channel closed; just wait for agent to finish.
                        // Same as above: no `?` here, errors are propagated
                        // after the background-task cleanup.
                        break agent_fut.await;
                    }
                },
            }

            // The turn's own backlog, for the surfaces that report it: the
            // messages this turn holds for a turn of their own. Written
            // once per message the turn takes in, so the count follows
            // the buffer while the turn runs.
            topic_manager.set_topic_queued(topic_name, buffered.len());
        }
    };

    // The agent is done. Messages still queued in the injection channel
    // were never folded into the conversation — the turn ended before its
    // next drain point. Re-enqueue them so they are processed as turns of
    // their own instead of being lost.
    while let Ok(qi) = inject_rx.try_recv() {
        buffered.push(qi);
    }
    topic_manager.set_topic_queued(topic_name, buffered.len());

    // Re-enqueue buffered messages so they are processed after the current AI call.
    // Uses direct try_send to the original mpsc channel rather than going
    // through enqueue(), which would create a new worker with an orphaned
    // event bus on the TopicManager clone.
    for qi in buffered {
        if let Err(e) = tx_for_reenqueue.try_send(qi) {
            tracing::warn!(topic = %topic_name, error = %e, "Failed to re-enqueue buffered message");
        }
    }
    // The turn's own backlog is empty again: the messages it held went
    // back to the queue, and the queue counts them from here on. A
    // message the queue could not take is dropped — the warning above is
    // where that shows; the count does not carry it.
    topic_manager.set_topic_queued(topic_name, 0);

    // Stop the delivery watcher
    delivery_cancel.cancel();
    let _ = delivery_handle.await;

    // Propagate agent errors only AFTER the background tasks above are
    // stopped. Returning early via `?` inside the select loop would leak
    // the delivery watcher.
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            let err_display = format!("{:#}", e);
            let notice = format!(
                "⚠️ Processing failed: {}\n\nThe agent was unable to complete this round. \
                 Please try again, use /reset to clear the context, or use a model with a larger context window.",
                err_display.chars().take(200).collect::<String>()
            );
            if store_result.topic_path.exists() {
                match outbound
                    .send_reply(
                        &message,
                        &notice,
                        &store_result.topic_path,
                        &store_result.message_dir,
                        None,
                    )
                    .await
                {
                    Err(send_err) => {
                        tracing::warn!(
                            error = %send_err,
                            "Failed to send processing-error notice"
                        );
                    }
                    Ok(delivery) => {
                        topic_manager
                            .publish_reply_sent(topic_name, &notice, delivery.chat_seq)
                            .await;
                    }
                }
            }
            return Err(e);
        }
    };

    // ── 5.5. GUARD: skip reply if topic directory no longer exists ──
    // If the topic was closed while AI was processing, the directory gets
    // deleted. Even with SSE cancellation, there's a small race window.
    // This guard prevents posting comments to closed issues/PRs.
    if !store_result.topic_path.exists() {
        tracing::warn!(
            topic_path = %store_result.topic_path.display(),
            "Topic directory no longer exists — skipping reply delivery"
        );
        return Ok(());
    }

    // ── 6. HANDLE AGENT RESULT ────────────────────────────────────────
    // The agent loop delivers the reply itself when it can (direct adapter
    // delivery, signal files never written). Otherwise it queued the reply
    // via the `.jyc/reply.md` + `.jyc/reply-sent.flag` file relay, which
    // this code delivers using the pre-warmed outbound adapter.
    if result.reply_delivered {
        // The background delivery watcher deletes reply-sent.flag after
        // successful delivery.
        let signal_path =
            resolve_state_dir(topic_name, &store_result.topic_path).join("reply-sent.flag");
        if !signal_path.exists() {
            tracing::info!(
                "Reply already delivered directly by the agent loop (or by the \
                 background watcher), skipping post-loop delivery"
            );
        } else {
            // Prefer the text carried in the result; fall back to reply.md.
            let reply_text = result
                .reply_text
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .map(|t| t.to_string());

            let reply_text = match reply_text {
                Some(t) => Some(t),
                None => {
                    let reply_md =
                        resolve_state_dir(topic_name, &store_result.topic_path).join("reply.md");
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
                if let Some(reason) = reply_blocked_by_hook(
                    &hooks,
                    topic_name,
                    &store_result.topic_path,
                    &message,
                    reply_text,
                )
                .await
                {
                    tracing::warn!(
                        topic = %topic_name,
                        reason = %reason,
                        "reply_send hook suppressed delivery"
                    );
                    // Same bookkeeping as a delivered reply — clean up the
                    // signal files so the suppression never re-fires.
                    tokio::fs::remove_file(&signal_path).await.ok();
                    let reply_md_gone =
                        resolve_state_dir(topic_name, &store_result.topic_path).join("reply.md");
                    tokio::fs::remove_file(&reply_md_gone).await.ok();
                } else {
                    tracing::info!(
                        text_len = reply_text.len(),
                        "Delivering queued reply via outbound adapter"
                    );

                    // Read signal file for attachment info
                    let attachments =
                        read_signal_attachments(&signal_path, &store_result.topic_path).await;

                    let delivery = outbound
                        .send_reply(
                            &message,
                            reply_text,
                            &store_result.topic_path,
                            &store_result.message_dir,
                            attachments.as_deref(),
                        )
                        .await?;
                    tracing::info!("Reply delivered via outbound adapter");
                    topic_manager
                        .publish_reply_sent(topic_name, reply_text, delivery.chat_seq)
                        .await;
                    // Clean up signal files after successful delivery to prevent re-delivery on restart
                    tokio::fs::remove_file(&signal_path).await.ok();
                    let reply_md_path =
                        resolve_state_dir(topic_name, &store_result.topic_path).join("reply.md");
                    tokio::fs::remove_file(&reply_md_path).await.ok();
                    topic_manager.metrics.reply_by_tool(topic_name);
                }
            } else {
                // Signal file exists but no reply text — tell the user what
                // happened instead of going silent.
                tracing::warn!("Reply signal present but no reply text available");
                let warning = "⚠️ [The agent finished, but its reply content was \
                    lost before delivery. Please ask again if you are waiting \
                    for an answer.]";
                if let Err(e) = outbound
                    .send_reply(
                        &message,
                        warning,
                        &store_result.topic_path,
                        &store_result.message_dir,
                        None,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Failed to deliver lost-reply warning");
                }
            }
        }
    } else if let Some(text) = result
        .reply_text
        .as_deref()
        .filter(|t| !t.trim().is_empty())
    {
        // The agent returned text without delivering it (e.g. `mode =
        // "static"` auto-replies, which never touch the delivery paths in
        // the agent loop). Deliver here, gated by reply_send hooks.
        tracing::info!(text_len = text.len(), "Delivering AI text via outbound");
        if let Some(reason) =
            reply_blocked_by_hook(&hooks, topic_name, &store_result.topic_path, &message, text)
                .await
        {
            tracing::warn!(
                topic = %topic_name,
                reason = %reason,
                "reply_send hook suppressed reply"
            );
        } else {
            let delivery = outbound
                .send_reply(
                    &message,
                    text,
                    &store_result.topic_path,
                    &store_result.message_dir,
                    None,
                )
                .await?;
            tracing::info!("Reply sent");
            topic_manager
                .publish_reply_sent(topic_name, text, delivery.chat_seq)
                .await;
            topic_manager.metrics.reply_by_fallback(topic_name);
        }
    } else {
        tracing::warn!("No reply delivered from AI");
    }

    Ok(())
}

/// Compile the hook set for a topic: global `[[hooks]]` first, then the
/// routed agent's `[[agents.<pattern>.hooks]]` (the pattern name is the
/// agent key — same merge rule as `[[commands]]`). Non-agent topics
/// (`pattern_name` empty) get the global set only.
pub(crate) fn build_hook_set(cfg: &jyc_types::AppConfig, pattern_name: &str) -> Arc<HookSet> {
    Arc::new(HookSet::merged_for_agent(
        &cfg.hooks,
        &cfg.agents,
        pattern_name,
    ))
}

/// Read skills from topic's .jyc/skills.json file.
pub(crate) async fn read_skills(topic_name: &str, topic_path: &Path) -> Vec<String> {
    let skills_path = resolve_state_dir(topic_name, topic_path).join("skills.json");
    match tokio::fs::read_to_string(&skills_path).await {
        Ok(content) => serde_json::from_str::<Vec<String>>(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Register user-defined slash commands into the topic's `CommandRegistry`,
/// and return the per-agent slice that was registered.
///
/// Order: globals first, then per-agent commands for the agent this topic
/// is routed to. `CommandRegistry::register` overwrites on collision with
/// a `tracing::warn`, so a per-agent command with the same name as a
/// global one wins for that agent's topics (and emits a visible warn at
/// every topic start — the only signal that an overwrite happened).
/// `pattern_name` is empty for non-agent topics; in that case only
/// globals are registered and `Vec::new()` is returned.
pub(crate) fn register_custom_commands(
    cfg: &jyc_types::AppConfig,
    pattern_name: &str,
    command_registry: &mut CommandRegistry,
) -> Vec<jyc_types::CustomCommand> {
    for custom in &cfg.commands {
        command_registry.register(
            &CustomCommandHandler::command_key(custom),
            Arc::new(CustomCommandHandler::new(custom.clone())),
        );
    }
    let per_agent = per_agent_commands(cfg, pattern_name);
    for custom in &per_agent {
        command_registry.register(
            &CustomCommandHandler::command_key(custom),
            Arc::new(CustomCommandHandler::new(custom.clone())),
        );
    }
    per_agent
}
#[cfg(test)]
mod has_active_queue;

/// Live injection at the worker: a message that arrives while a turn is
/// running joins that turn when `live_injection` is on.
#[cfg(test)]
mod live_injection_tests;
