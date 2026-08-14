//! Per-message processing pipeline (`process_message`) and its helpers.
//!
//! Extracted from the monolithic `thread_manager.rs`; the worker is the
//! free-function half of the module, while `ThreadManager` (struct + impl)
//! lives in `mod.rs`.

use anyhow::Result;
use arc_swap::ArcSwap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentService;
use crate::command::cancel_handler::CancelCommandHandler;
use crate::command::close_handler::CloseCommandHandler;
use crate::command::custom_handler::CustomCommandHandler;
use crate::command::exchange_handler::ExchangeCommandHandler;
use crate::command::handler::CommandContext;
use crate::command::help_handler::HelpCommandHandler;
use crate::command::mode_handler::{BuildCommandHandler, PlanCommandHandler};
use crate::command::model_handler::ModelCommandHandler;
use crate::command::new_handler::NewCommandHandler;
use crate::command::pin_handler::PinCommandHandler;
use crate::command::registry::CommandRegistry;
use crate::command::reset_handler::ResetCommandHandler;
use crate::command::template_handler::TemplateCommandHandler;
use crate::command::thinking_handler::ThinkingCommandHandler;
use crate::command::unpin_handler::UnpinCommandHandler;
use crate::message_storage::{MessageStorage, StoreResult};
use crate::pending_delivery::watch_pending_deliveries;
use crate::thread_json::ThreadJson;
use jyc_types::{InboundMessage, OutboundAdapter, QueueItem};

use super::ThreadManager;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_message(
    item: &mut QueueItem,
    thread_name: &str,
    storage: &MessageStorage,
    outbound: Arc<dyn OutboundAdapter>,
    agent: Arc<dyn AgentService>,
    pending_rx: &mut mpsc::Receiver<QueueItem>,
    template_dirs: &crate::template_dirs::TemplateDirs,
    config: &Arc<ArcSwap<jyc_types::AppConfig>>,
    tx_for_reenqueue: &mpsc::Sender<QueueItem>,
    thread_manager: Arc<ThreadManager>,
    thread_cancel: CancellationToken,
) -> Result<()> {
    // ── 1. STORE ──────────────────────────────────────────────────────
    let is_matched = !item.pattern_match.pattern_name.is_empty();
    let store_result: StoreResult = match &item.thread_path_override {
        Some(path) => {
            storage
                .store_at_path(&item.message, path, is_matched)
                .await?
        }
        None => {
            storage
                .store_with_match(
                    &item.message,
                    thread_name,
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

    // ── 1.1. WRITE THREAD.JSON (if channel provides metadata) ─────────
    // Channels like wecomkf embed customer info in message metadata.
    // Persist it once per thread so subsequent messages can read cached data.
    if item.message.channel == "wecomkf" {
        let thread_json_path = store_result.thread_path.join(".jyc").join("thread.json");
        if !thread_json_path.exists() {
            write_wecomkf_thread_json(&item.message, &store_result.thread_path, thread_name).await;
        }
    }

    // ── 1.2. WRITE THREAD ROUTING METADATA ────────────────────────────
    // Persist routing metadata on the first message for a thread so that
    // the dashboard's `ThreadProxyHandler` (via /ws/<channel>/<thread>)
    // can restore it when constructing a synthetic InboundMessage.
    // Without this, the proxy's InboundMessage has empty metadata and
    // channel-specific reply routing fails (e.g., github_number missing → 404).
    let thread_meta_path = store_result
        .thread_path
        .join(".jyc")
        .join("thread-meta.json");
    if !thread_meta_path.exists() && item.message.channel_uid != "dashboard" {
        let meta = serde_json::json!({
            "channel_uid": item.message.channel_uid,
            "external_id": item.message.external_id,
            "thread_refs": item.message.thread_refs,
            "metadata": item.message.metadata,
        });
        if let Ok(json) = serde_json::to_string_pretty(&meta) {
            let _ = tokio::fs::create_dir_all(store_result.thread_path.join(".jyc")).await;
            if let Err(e) = tokio::fs::write(&thread_meta_path, json).await {
                tracing::warn!(error = %e, "Failed to write thread-meta.json");
            } else {
                tracing::debug!(thread = %thread_name, "Wrote thread-meta.json");
            }
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
    command_registry.register(Box::new(HelpCommandHandler));
    command_registry.register(Box::new(ModelCommandHandler));
    command_registry.register(Box::new(PlanCommandHandler));
    command_registry.register(Box::new(BuildCommandHandler));
    command_registry.register(Box::new(ResetCommandHandler));
    command_registry.register(Box::new(NewCommandHandler));
    command_registry.register(Box::new(TemplateCommandHandler));
    command_registry.register(Box::new(CloseCommandHandler::new(thread_manager.clone())));
    command_registry.register(Box::new(CancelCommandHandler::new(thread_manager.clone())));
    command_registry.register(Box::new(PinCommandHandler::new(thread_manager.clone())));
    command_registry.register(Box::new(UnpinCommandHandler::new(thread_manager.clone())));
    command_registry.register(Box::new(ThinkingCommandHandler));
    command_registry.register(Box::new(ExchangeCommandHandler::new(
        thread_manager.clone(),
    )));

    // User-defined commands from config.toml `[[commands]]`. Registered last,
    // but `register()` warns on collisions and config validation rejects
    // names that shadow a built-in.
    for custom in &config.load().commands {
        command_registry.register(Box::new(CustomCommandHandler::new(custom.clone())));
    }

    let cmd_context = CommandContext {
        args: vec![],
        thread_path: store_result.thread_path.clone(),
        config: config.load_full(),
        channel: message.channel.clone(),
        channel_type: thread_manager.channel_type.clone(),
        agent: Some(agent.clone()),
        template_dirs: template_dirs.clone(),
        config_path: thread_manager.config_path.clone(),
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
        // Publish a ReplySent event so the inspect server's ActivityTracker
        // fans it out as a chat_message to dashboard WS clients. Without
        // this, command results are persisted to disk (visible on re-enter)
        // but never appear live in the chat pane.
        thread_manager
            .publish_reply_sent(thread_name, &summary)
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

    // ── 4.8. START PROGRESS UPDATER ───────────────────────────────────
    // For wecom_bot, spawn a background task that updates the processing
    // indicator with a rotating spinner every 3 seconds.
    let progress_cancel = tokio_util::sync::CancellationToken::new();
    let progress_handle = if thread_manager.channel_type() == "wecom_bot" {
        if let Some(ref stream_id) = indicator_handle {
            let progress_outbound = outbound.clone();
            let progress_message = message.clone();
            let progress_stream_id = stream_id.clone();
            let progress_cancel_child = progress_cancel.clone();
            Some(tokio::spawn(async move {
                update_progress_indicator(
                    progress_outbound,
                    progress_message,
                    progress_stream_id,
                    progress_cancel_child,
                )
                .await;
            }))
        } else {
            None
        }
    } else {
        None
    };

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
    let delivery_thread_manager = thread_manager.clone();
    let delivery_thread_name = thread_name.to_string();
    let delivery_handle = tokio::spawn(async move {
        let event_bus = delivery_thread_manager
            .get_event_bus(&delivery_thread_name)
            .await;
        watch_pending_deliveries(
            &delivery_thread_path,
            &delivery_message_dir,
            &delivery_message,
            &*delivery_outbound,
            delivery_cancel_child,
            event_bus,
            &delivery_thread_name,
        )
        .await;
    });

    // The in-process agent does not consume `pending_rx`. We monitor it
    // ourselves for incoming messages during AI processing. Slash commands
    // (e.g. /cancel, /model) are executed immediately via the command
    // registry so the user gets instant feedback even during a 429 retry.
    // Non-command messages are re-enqueued so they are processed after the
    // current AI call completes (same as before).
    let (_dummy_tx, mut dummy_rx) = mpsc::channel::<QueueItem>(1);
    drop(_dummy_tx);

    let agent_fut = agent.process(
        &message,
        thread_name,
        &store_result.thread_path,
        &store_result.message_dir,
        &mut dummy_rx,
        thread_cancel.clone(),
    );
    tokio::pin!(agent_fut);

    // Buffer non-command messages that arrive during AI processing.
    // They are re-enqueued after the agent finishes, preserving FIFO order.
    let mut buffered: Vec<QueueItem> = Vec::new();

    let result = loop {
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
                            thread = %thread_name,
                            command = %trimmed,
                            "Executing slash command during AI processing"
                        );
                        let cmd_ctx = CommandContext {
                            args: vec![],
                            thread_path: store_result.thread_path.clone(),
                            config: config.load_full(),
                            channel: qi.message.channel.clone(),
                            channel_type: thread_manager.channel_type.clone(),
                            agent: Some(agent.clone()),
                            template_dirs: template_dirs.clone(),
                            config_path: thread_manager.config_path.clone(),
                        };
                        match command_registry.process_commands(trimmed, &cmd_ctx).await {
                            Ok(output) => {
                                if !output.results.is_empty() {
                                    let summary = output.results_summary();
                                    if let Err(e) = outbound
                                        .send_reply(
                                            &qi.message,
                                            &summary,
                                            &store_result.thread_path,
                                            &store_result.message_dir,
                                            None,
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            error = %e,
                                            "Failed to send command result during AI processing"
                                        );
                                    }
                                    // Publish ReplySent so the dashboard
                                    // sees the command result live (same as
                                    // the main command-result path above).
                                    thread_manager
                                        .publish_reply_sent(thread_name, &summary)
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
                                        thread = %thread_name,
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
    };

    // Re-enqueue buffered messages so they are processed after the current AI call.
    // Uses direct try_send to the original mpsc channel rather than going
    // through enqueue(), which would create a new worker with an orphaned
    // event bus on the ThreadManager clone.
    for qi in buffered {
        if let Err(e) = tx_for_reenqueue.try_send(qi) {
            tracing::warn!(thread = %thread_name, error = %e, "Failed to re-enqueue buffered message");
        }
    }

    // Stop the delivery watcher
    delivery_cancel.cancel();
    let _ = delivery_handle.await;

    // Stop the progress updater
    progress_cancel.cancel();
    if let Some(handle) = progress_handle {
        let _ = handle.await;
    }

    // Propagate agent errors only AFTER the background tasks above are
    // stopped. Returning early via `?` inside the select loop would leak
    // the progress updater: it has no self-termination and would keep
    // sending stream updates for a long-expired req_id forever (visible
    // as a never-ending WeCom "reply ack error errcode=846604" WARN storm
    // even when no task is running).
    let result = result?;

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
                thread_manager
                    .publish_reply_sent(thread_name, reply_text)
                    .await;
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
        thread_manager.publish_reply_sent(thread_name, text).await;
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

/// Braille spinner frames for dynamic progress indicator.
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Background task that updates the WeCom Bot processing indicator
/// with a rotating spinner and elapsed time every 3 seconds.
///
/// The indicator content cycles through spinner frames while maintaining
/// a consistent activity message, giving the user a sense of progress.
async fn update_progress_indicator(
    outbound: Arc<dyn OutboundAdapter>,
    message: InboundMessage,
    stream_id: String,
    cancel: CancellationToken,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
    let mut frame_idx = 0usize;
    let mut elapsed_secs = 0u64;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let frame = SPINNER[frame_idx % SPINNER.len()];
                let content = format!(
                    "{} 正在处理中... (已用 {}s)",
                    frame, elapsed_secs
                );

                if let Err(e) = outbound
                    .update_processing_indicator(&message, &stream_id, &content)
                    .await
                {
                    tracing::debug!(
                        error = %format!("{:#}", e),
                        "Failed to update progress indicator"
                    );
                }

                frame_idx += 1;
                elapsed_secs += 3;
            }
            _ = cancel.cancelled() => break,
        }
    }
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
pub(crate) async fn read_skills(thread_path: &Path) -> Vec<String> {
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

#[cfg(test)]
mod has_active_queue_tests {
    use super::*;
    use crate::message_storage::MessageStorage;
    use crate::metrics::MetricsCollector;
    use crate::static_agent::StaticAgentService;
    use jyc_types::{ChangeKind, ChangedFileEntry, PatternMatch};
    use std::collections::HashMap;
    use tempfile::tempdir;

    /// Minimal outbound adapter that does nothing.
    struct NoopOutbound;

    #[async_trait::async_trait]
    impl jyc_types::OutboundAdapter for NoopOutbound {
        fn channel_type(&self) -> &str {
            "test"
        }
        async fn connect(&self) -> Result<()> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<()> {
            Ok(())
        }
        fn clean_body(&self, raw_body: &str) -> String {
            raw_body.to_string()
        }
        async fn send_reply(
            &self,
            _original: &InboundMessage,
            _reply_text: &str,
            _thread_path: &Path,
            _message_dir: &str,
            _attachments: Option<&[jyc_types::OutboundAttachment]>,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "test".to_string(),
            })
        }
        async fn send_message(
            &self,
            _recipient: &str,
            _subject: &str,
            _body: &str,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "test".to_string(),
            })
        }
    }

    fn make_test_tm(workspace: &std::path::Path) -> Arc<ThreadManager> {
        make_test_tm_with_config(
            workspace,
            r#"
[general]
[channels.test]
type = "email"
[channels.test.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.test.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
"#,
        )
    }

    fn make_test_tm_with_config(
        workspace: &std::path::Path,
        config_str: &str,
    ) -> Arc<ThreadManager> {
        let storage = Arc::new(MessageStorage::new(workspace));
        let cancel = CancellationToken::new();
        let metrics_cancel = CancellationToken::new();
        let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();
        let config = Arc::new(ArcSwap::from_pointee(
            jyc_types::load_config_from_str(config_str).unwrap(),
        ));

        Arc::new(ThreadManager::new_with_options(
            1,
            10,
            storage,
            Arc::new(NoopOutbound),
            Arc::new(StaticAgentService::new("ok")),
            cancel,
            true,
            workspace.join("templates"),
            config,
            "test-channel".to_string(),
            "websocket".to_string(),
            workspace.parent().unwrap_or(workspace).to_path_buf(),
            workspace.to_path_buf(),
            metrics,
            None,
        ))
    }

    #[tokio::test]
    async fn test_has_active_queue_false_for_unknown_thread() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);
        assert!(!tm.has_active_queue("nonexistent").await);
    }

    #[tokio::test]
    async fn test_has_active_queue_true_after_enqueue() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        // Create a thread directory so list_threads finds it
        let thread_path = workspace.join("test-thread");
        tokio::fs::create_dir_all(thread_path.join(".jyc"))
            .await
            .unwrap();

        // Enqueue a dummy message — this creates an mpsc queue
        let msg = InboundMessage {
            id: "test".to_string(),
            channel: "test-channel".to_string(),
            channel_uid: "test".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "test".to_string(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
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
        let pattern_match = PatternMatch {
            pattern_name: "test".to_string(),
            channel: "websocket".to_string(),
            matches: HashMap::new(),
        };
        tm.enqueue(
            msg,
            "test-thread".to_string(),
            pattern_match,
            None,
            false,
            None,
        )
        .await;

        // Give the worker a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            tm.has_active_queue("test-thread").await,
            "Thread should have an active queue after enqueue"
        );

        // Clean up
        tm.shutdown().await;
    }

    /// Regression test (#542): injected messages (jyc_send_to_thread,
    /// dashboard thread proxy) carry an empty pattern_name. The worker must
    /// not overwrite `.jyc/pattern` with it, or the dashboard loses the
    /// thread's pattern identity until a router-matched message rewrites it.
    #[tokio::test]
    async fn test_empty_pattern_name_does_not_clobber_pattern_file() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        let make_msg = || InboundMessage {
            id: uuid::Uuid::new_v4().to_string(),
            channel: "test-channel".to_string(),
            channel_uid: "test".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "test".to_string(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
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
        let make_pm = |name: &str| PatternMatch {
            pattern_name: name.to_string(),
            channel: "websocket".to_string(),
            matches: HashMap::new(),
        };

        // Router-matched message writes the real pattern name.
        tm.enqueue(
            make_msg(),
            "test-thread".to_string(),
            make_pm("jyc"),
            None,
            false,
            None,
        )
        .await;
        let thread_path = workspace.join("test-thread");
        assert!(
            wait_for_history_lines(&thread_path, 1).await,
            "worker did not process the first message in time"
        );
        let pattern_file = thread_path.join(".jyc").join("pattern");
        assert_eq!(
            tokio::fs::read_to_string(&pattern_file).await.unwrap(),
            "jyc"
        );

        // Injected message with empty pattern_name must leave the file
        // alone. Wait until the worker provably processed it (second chat
        // history line) before asserting — otherwise a slow worker would
        // let the test pass even without the guard.
        tm.enqueue(
            make_msg(),
            "test-thread".to_string(),
            make_pm(""),
            None,
            false,
            None,
        )
        .await;
        assert!(
            wait_for_history_lines(&thread_path, 2).await,
            "worker did not process the injected message in time"
        );
        assert_eq!(
            tokio::fs::read_to_string(&pattern_file).await.unwrap(),
            "jyc"
        );

        tm.shutdown().await;
    }

    /// Poll until the thread's chat history holds at least `n` lines
    /// (i.e. the worker processed `n` messages). ~2s timeout.
    async fn wait_for_history_lines(thread_path: &std::path::Path, n: usize) -> bool {
        for _ in 0..40 {
            let (files, _) = crate::chat_log_store::list_chat_history_files(thread_path);
            let mut count = 0;
            for f in files {
                if let Ok(content) = tokio::fs::read_to_string(&f).await {
                    count += content.lines().filter(|l| !l.trim().is_empty()).count();
                }
            }
            if count >= n {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }

    /// `pattern_for_thread` (#542): resolves the enabled pattern named after
    /// the thread, including its template/role/custom `thread_path`; returns
    /// None for unknown/disabled names so injection falls back to an empty
    /// pattern.
    #[tokio::test]
    async fn test_pattern_for_thread() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let custom_path = tmp.path().join("custom-jyc");

        let config_str = format!(
            r#"
[general]
[channels.test-channel]
type = "websocket"
[[channels.test-channel.patterns]]
name = "jyc"
enabled = true
thread_path = "{}"
template = "dev"
role = "Developer"
[channels.test-channel.patterns.rules]
[[channels.test-channel.patterns]]
name = "disabled"
enabled = false
thread_path = "/nowhere"
[channels.test-channel.patterns.rules]
[agent]
enabled = true
mode = "agent"
"#,
            custom_path.display()
        );
        let tm = make_test_tm_with_config(&workspace, &config_str);

        let p = tm
            .pattern_for_thread("jyc")
            .expect("pattern should resolve");
        assert_eq!(p.name, "jyc");
        assert_eq!(
            p.thread_path.as_deref(),
            Some(custom_path.to_str().unwrap())
        );
        assert_eq!(p.template.as_deref(), Some("dev"));
        assert_eq!(p.role.as_deref(), Some("Developer"));
        assert!(p.live_injection);
        assert!(tm.pattern_for_thread("disabled").is_none());
        assert!(tm.pattern_for_thread("unknown").is_none());
    }

    /// Regression test: the per-worker clone must share the parent's
    /// `thread_cancels` map. Previously the clone got a fresh empty map, so
    /// `cancel_thread` (invoked via /cancel through the command registry,
    /// which holds the clone) never found the running worker's token — the
    /// user got a success reply but the agent kept running.
    #[tokio::test]
    async fn test_cancel_thread_via_worker_clone_really_cancels() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        // Simulate an active worker token registered in the shared map
        let token = CancellationToken::new();
        {
            let mut cancels = tm.thread_cancels.lock().await;
            cancels.insert("test-thread".to_string(), token.clone());
        }

        // Cancel through the worker clone — the path /cancel actually takes
        let clone = tm.worker_clone();
        assert!(
            clone.cancel_thread("test-thread").await,
            "cancel_thread via worker clone must find the shared token"
        );
        assert!(token.is_cancelled());

        // Unknown thread must report "nothing cancelled"
        assert!(!clone.cancel_thread("no-such-thread").await);
    }

    #[tokio::test]
    async fn test_publish_incoming_message_on_event_bus() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        // Create event bus manually so we can subscribe
        let bus = tm.get_or_create_event_bus("test-thread").await.unwrap();
        let mut rx = bus.subscribe().await.unwrap();

        // Publish incoming message event
        tm.publish_incoming_message("test-thread", "user", "hello world")
            .await;

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive event within timeout")
            .expect("should have an event");

        match event {
            crate::thread_event::ThreadEvent::IncomingMessage {
                thread_name,
                sender,
                text,
                ..
            } => {
                assert_eq!(thread_name, "test-thread");
                assert_eq!(sender, "user");
                assert_eq!(text, "hello world");
            }
            other => panic!("expected IncomingMessage, got {:?}", other),
        }

        tm.shutdown().await;
    }

    #[tokio::test]
    async fn test_publish_reply_sent_on_event_bus() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        // Create event bus manually so we can subscribe
        let bus = tm.get_or_create_event_bus("test-thread").await.unwrap();
        let mut rx = bus.subscribe().await.unwrap();

        // Publish reply sent event
        tm.publish_reply_sent("test-thread", "AI reply here").await;

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive event within timeout")
            .expect("should have an event");

        match event {
            crate::thread_event::ThreadEvent::ReplySent {
                thread_name, text, ..
            } => {
                assert_eq!(thread_name, "test-thread");
                assert_eq!(text, "AI reply here");
            }
            other => panic!("expected ReplySent, got {:?}", other),
        }

        tm.shutdown().await;
    }

    #[tokio::test]
    async fn test_publish_incoming_message_noop_without_event_bus() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        // No event bus created — publish should silently succeed (no panic)
        tm.publish_incoming_message("test-thread", "user", "hello")
            .await;

        tm.shutdown().await;
    }

    #[tokio::test]
    async fn test_thread_meta_written_on_first_message() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        let thread_path = workspace.join("test-thread");
        tokio::fs::create_dir_all(thread_path.join(".jyc"))
            .await
            .unwrap();

        let mut metadata = HashMap::new();
        metadata.insert("github_number".to_string(), serde_json::json!(42));

        let msg = InboundMessage {
            id: "test".to_string(),
            channel: "test-channel".to_string(),
            channel_uid: "test-uid".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "test".to_string(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: Some(vec!["ref-1".to_string()]),
            reply_to_id: None,
            external_id: Some("ext-123".to_string()),
            attachments: vec![],
            metadata,
            matched_pattern: None,
        };
        let pattern_match = PatternMatch {
            pattern_name: "test".to_string(),
            channel: "websocket".to_string(),
            matches: HashMap::new(),
        };
        tm.enqueue(
            msg,
            "test-thread".to_string(),
            pattern_match,
            None,
            false,
            None,
        )
        .await;

        // Give the worker a moment to process
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Check thread-meta.json was written
        let meta_path = thread_path.join(".jyc").join("thread-meta.json");
        assert!(meta_path.exists(), "thread-meta.json should be written");

        let content = std::fs::read_to_string(&meta_path).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(meta["channel_uid"], "test-uid");
        assert_eq!(meta["external_id"], "ext-123");
        assert_eq!(meta["thread_refs"], serde_json::json!(["ref-1"]));
        assert_eq!(meta["metadata"]["github_number"], 42);

        tm.shutdown().await;
    }

    #[tokio::test]
    async fn test_thread_meta_not_overwritten_on_second_message() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        let thread_path = workspace.join("test-thread");
        tokio::fs::create_dir_all(thread_path.join(".jyc"))
            .await
            .unwrap();

        // Pre-write a thread-meta.json with a known value
        let meta_path = thread_path.join(".jyc").join("thread-meta.json");
        std::fs::write(
            &meta_path,
            r#"{"channel_uid":"original-uid","metadata":{"github_number":99}}"#,
        )
        .unwrap();

        let msg = InboundMessage {
            id: "test".to_string(),
            channel: "test-channel".to_string(),
            channel_uid: "new-uid".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "test".to_string(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
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
        let pattern_match = PatternMatch {
            pattern_name: "test".to_string(),
            channel: "websocket".to_string(),
            matches: HashMap::new(),
        };
        tm.enqueue(
            msg,
            "test-thread".to_string(),
            pattern_match,
            None,
            false,
            None,
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Should NOT be overwritten — still has original values
        let content = std::fs::read_to_string(&meta_path).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(meta["channel_uid"], "original-uid");
        assert_eq!(meta["metadata"]["github_number"], 99);

        tm.shutdown().await;
    }

    #[tokio::test]
    async fn test_thread_meta_not_written_for_dashboard_channel_uid() {
        // Dashboard-injected messages have channel_uid == "dashboard" and empty
        // metadata. Writing thread-meta.json for these would poison subsequent
        // injections — the empty metadata would be re-used and real routing data
        // (e.g. github_number) would be lost, causing 404 errors on replies.
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        let thread_path = workspace.join("test-thread");
        let mut metadata = HashMap::new();
        metadata.insert("github_number".to_string(), serde_json::json!(42));

        let msg = InboundMessage {
            id: "test".to_string(),
            channel: "test-channel".to_string(),
            channel_uid: "dashboard".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "test".to_string(),
            content: jyc_types::MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: Some(vec!["ref-1".to_string()]),
            reply_to_id: None,
            external_id: Some("ext-123".to_string()),
            attachments: vec![],
            metadata,
            matched_pattern: None,
        };
        let pattern_match = PatternMatch {
            pattern_name: "test".to_string(),
            channel: "websocket".to_string(),
            matches: HashMap::new(),
        };
        tm.enqueue(
            msg,
            "test-thread".to_string(),
            pattern_match,
            None,
            false,
            None,
        )
        .await;

        // Give the worker a moment to process
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // thread-meta.json must NOT be written for dashboard messages
        let meta_path = thread_path.join(".jyc").join("thread-meta.json");
        assert!(
            !meta_path.exists(),
            "thread-meta.json should NOT be written for dashboard channel_uid"
        );

        tm.shutdown().await;
    }

    #[tokio::test]
    async fn test_thread_path_returns_custom_override() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        let custom_path = tmp.path().join("custom-threads").join("my-thread");
        tm.thread_paths
            .lock()
            .await
            .insert("my-thread".to_string(), custom_path.clone());

        let resolved = tm.thread_path("my-thread").await;
        assert_eq!(resolved, Some(custom_path));

        tm.shutdown().await;
    }

    #[tokio::test]
    async fn test_thread_path_falls_back_to_default() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        // Create thread dir at default location
        let default_path = workspace.join("default-thread");
        tokio::fs::create_dir_all(&default_path).await.unwrap();

        // No custom path stored — should fall back to workspace/thread_name
        let resolved = tm.thread_path("default-thread").await;
        assert_eq!(resolved, Some(default_path));

        tm.shutdown().await;
    }

    #[tokio::test]
    async fn test_thread_path_returns_none_for_nonexistent() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        let resolved = tm.thread_path("nonexistent").await;
        assert_eq!(resolved, None);

        tm.shutdown().await;
    }

    #[tokio::test]
    async fn test_custom_thread_paths_empty_initially() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        let paths = tm.custom_thread_paths().await;
        assert!(paths.is_empty());

        tm.shutdown().await;
    }

    /// Regression test for the `jyc open` ad-hoc thread timeout:
    ///
    /// `set_thread_path` must create `.jyc/thread-name` so that the
    /// `path.join(".jyc").is_dir()` filter in `list_threads` keeps the
    /// entry. Without it, a freshly-registered ad-hoc thread is dropped
    /// from the overview and `wait_for_thread` in `run_open` times out
    /// with "Timeout waiting for thread ... to be created".
    #[tokio::test]
    async fn test_set_thread_path_creates_jyc_dir_and_appears_in_list() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        let custom_path = tmp.path().join("adhoc-projects");
        tm.set_thread_path("projects", custom_path.clone())
            .await
            .unwrap();

        // `.jyc/` and `.jyc/thread-name` are written by set_thread_path so
        // list_threads doesn't filter the entry out.
        assert!(
            custom_path.join(".jyc").is_dir(),
            "set_thread_path must create .jyc/"
        );
        assert_eq!(
            tokio::fs::read_to_string(custom_path.join(".jyc").join("thread-name"))
                .await
                .unwrap()
                .trim(),
            "projects",
            "set_thread_path must write .jyc/thread-name"
        );

        // The new ad-hoc thread appears in list_threads so the dashboard
        // overview reports it within the 5s wait_for_thread window.
        let threads = tm.list_threads().await;
        assert!(
            threads.iter().any(|t| t.name == "projects"),
            "ad-hoc thread should appear in list_threads"
        );

        tm.shutdown().await;
    }

    #[tokio::test]
    async fn test_restore_custom_thread_paths_from_disk() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        // Custom thread path outside workspace
        let custom_path = tmp.path().join("external-project");
        tokio::fs::create_dir_all(custom_path.join(".jyc"))
            .await
            .unwrap();
        // Simulate a previously initialized thread
        tokio::fs::write(
            custom_path.join(".jyc").join("thread-name"),
            "my-custom-thread",
        )
        .await
        .unwrap();

        // Config with thread_path override — channel name must match TM's channel_name
        let config_str = format!(
            r#"
[general]
[channels.test-channel]
type = "email"
[channels.test-channel.inbound]
host = "h"
port = 998
username = "u"
password = "p"
[channels.test-channel.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[[channels.test-channel.patterns]]
name = "test-pattern"
thread_path = "{}"
[channels.test-channel.patterns.rules]
[agent]
enabled = true
mode = "agent"
"#,
            custom_path.display()
        );

        let config = Arc::new(ArcSwap::from_pointee(
            jyc_types::load_config_from_str(&config_str).unwrap(),
        ));

        let storage = Arc::new(MessageStorage::new(&workspace));
        let cancel = CancellationToken::new();
        let metrics_cancel = CancellationToken::new();
        let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();

        let tm = Arc::new(ThreadManager::new_with_options(
            1,
            10,
            storage,
            Arc::new(NoopOutbound),
            Arc::new(StaticAgentService::new("ok")),
            cancel,
            true,
            workspace.join("templates"),
            config,
            "test-channel".to_string(),
            "websocket".to_string(),
            workspace.parent().unwrap_or(&workspace).to_path_buf(),
            workspace.to_path_buf(),
            metrics,
            None,
        ));

        // Before restore: empty
        let paths = tm.custom_thread_paths().await;
        assert!(paths.is_empty());

        // Restore from disk
        tm.restore_custom_thread_paths().await;

        // After restore: mapping exists
        let paths = tm.custom_thread_paths().await;
        assert_eq!(
            paths.get("my-custom-thread"),
            Some(&custom_path),
            "restore_custom_thread_paths should rediscover the thread"
        );

        // list_threads should now include the restored thread
        let threads = tm.list_threads().await;
        let names: Vec<&str> = threads.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"my-custom-thread"),
            "list_threads should include restored custom-path thread"
        );

        // Event bus should be pre-created so ActivityTracker can subscribe
        // before the first message arrives (avoids lost first-message events).
        let bus = tm.get_event_bus("my-custom-thread").await;
        assert!(
            bus.is_some(),
            "restore_custom_thread_paths should pre-create event bus"
        );

        tm.shutdown().await;
    }

    #[tokio::test]
    async fn test_restore_skips_missing_thread_name_file() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        // Custom path exists but has no .jyc/thread-name file
        let custom_path = tmp.path().join("uninitialized");
        tokio::fs::create_dir_all(&custom_path).await.unwrap();

        let config_str = format!(
            r#"
[general]
[channels.test-channel]
type = "email"
[channels.test-channel.inbound]
host = "h"
port = 998
username = "u"
password = "p"
[channels.test-channel.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[[channels.test-channel.patterns]]
name = "test-pattern"
thread_path = "{}"
[channels.test-channel.patterns.rules]
[agent]
enabled = true
mode = "agent"
"#,
            custom_path.display()
        );

        let config = Arc::new(ArcSwap::from_pointee(
            jyc_types::load_config_from_str(&config_str).unwrap(),
        ));

        let storage = Arc::new(MessageStorage::new(&workspace));
        let cancel = CancellationToken::new();
        let metrics_cancel = CancellationToken::new();
        let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();

        let tm = Arc::new(ThreadManager::new_with_options(
            1,
            10,
            storage,
            Arc::new(NoopOutbound),
            Arc::new(StaticAgentService::new("ok")),
            cancel,
            true,
            workspace.join("templates"),
            config,
            "test-channel".to_string(),
            "websocket".to_string(),
            workspace.parent().unwrap_or(&workspace).to_path_buf(),
            workspace.to_path_buf(),
            metrics,
            None,
        ));

        tm.restore_custom_thread_paths().await;

        // Should be empty — no thread-name file
        let paths = tm.custom_thread_paths().await;
        assert!(
            paths.is_empty(),
            "Should skip paths without thread-name file"
        );

        tm.shutdown().await;
    }

    #[tokio::test]
    async fn test_list_threads_cleans_stale_custom_path() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_test_tm(&workspace);

        // Insert a custom path that doesn't exist on disk
        let ghost_path = tmp.path().join("deleted-thread");
        tokio::fs::create_dir_all(ghost_path.join(".jyc"))
            .await
            .unwrap();
        tm.thread_paths
            .lock()
            .await
            .insert("ghost".to_string(), ghost_path.clone());

        // list_threads should include it while dir exists
        let threads = tm.list_threads().await;
        assert!(
            threads.iter().any(|t| t.name == "ghost"),
            "Should list thread while directory exists"
        );

        // Delete the directory
        tokio::fs::remove_dir_all(&ghost_path).await.unwrap();

        // list_threads should now clean it up
        let threads = tm.list_threads().await;
        assert!(
            !threads.iter().any(|t| t.name == "ghost"),
            "Should not list thread after directory deleted"
        );

        // thread_paths map should no longer contain the entry
        let paths = tm.custom_thread_paths().await;
        assert!(
            !paths.contains_key("ghost"),
            "Stale entry should be removed from thread_paths"
        );

        tm.shutdown().await;
    }
    /// Build a TM whose config prices `cnprov/m1` in CNY, so
    /// `list_threads` has a real pricing entry to resolve a currency from.
    fn make_priced_tm(workspace: &std::path::Path) -> Arc<ThreadManager> {
        let storage = Arc::new(MessageStorage::new(workspace));
        let cancel = CancellationToken::new();
        let metrics_cancel = CancellationToken::new();
        let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();
        let config = Arc::new(ArcSwap::from_pointee(
            jyc_types::load_config_from_str(
                r#"
[general]
[channels.test]
type = "email"
[channels.test.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.test.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
model = "cnprov/m1"
[agent.providers.cnprov]
type = "openai-compatible"
[agent.providers.cnprov.models.m1]
pricing = { input_per_million = 3.0, output_per_million = 4.0, cache_hit_per_million = 0.5, currency = "CNY" }
"#,
            )
            .unwrap(),
        ));

        Arc::new(ThreadManager::new_with_options(
            1,
            10,
            storage,
            Arc::new(NoopOutbound),
            Arc::new(StaticAgentService::new("ok")),
            cancel,
            true,
            workspace.join("templates"),
            config,
            "test-channel".to_string(),
            "websocket".to_string(),
            workspace.parent().unwrap_or(workspace).to_path_buf(),
            workspace.to_path_buf(),
            metrics,
            None,
        ))
    }

    /// Regression: a session carrying spend across UTC midnight has a
    /// non-zero `session_cost` but an empty ledger for the new day. The
    /// currency must still come from the model's configured pricing —
    /// previously it fell back to DEFAULT_CURRENCY, labelling a CNY
    /// amount with the wrong unit.
    #[tokio::test]
    async fn list_threads_currency_from_config_when_ledger_empty() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let thread = workspace.join("t1");
        std::fs::create_dir_all(thread.join(".jyc")).unwrap();
        // Session has spend; no bill-<today>.jsonl exists at all.
        std::fs::write(
            thread.join(".jyc/agent-session.json"),
            r#"{"session_cost":0.05,"context_input_tokens":10}"#,
        )
        .unwrap();

        let tm = make_priced_tm(&workspace);
        let threads = tm.list_threads().await;
        let t = threads
            .iter()
            .find(|t| t.name == "t1")
            .expect("thread listed");
        let cost = t.cost.as_ref().expect("cost present when session_cost > 0");

        assert_eq!(
            cost.currency, "CNY",
            "currency must come from pricing config"
        );
        assert!((cost.session - 0.05).abs() < 1e-9);
        assert_eq!(cost.today, 0.0, "no ledger entries today");
        tm.shutdown().await;
    }

    /// A multi-currency day is the one case where the ledger's own label
    /// wins: config can only name one currency, but the ledger knows the
    /// thread actually spent in two.
    #[tokio::test]
    async fn list_threads_preserves_mixed_currency_from_ledger() {
        use crate::billing_log_store::{BillingEntry, BillingLogStore, MIXED_CURRENCY};

        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let thread = workspace.join("t1");
        std::fs::create_dir_all(thread.join(".jyc")).unwrap();

        for (cost, currency) in [(1.0, "CNY"), (2.0, "USD")] {
            BillingLogStore::append(
                &thread,
                &BillingEntry {
                    ts: chrono::Utc::now().to_rfc3339(),
                    model: "cnprov/m1".to_string(),
                    input_tokens: 100,
                    output_tokens: 10,
                    cache_hit_tokens: 0,
                    cache_creation_tokens: 0,
                    cost,
                    currency: currency.to_string(),
                    kind: crate::billing_log_store::KIND_CALL.to_string(),
                },
            )
            .unwrap();
        }

        let tm = make_priced_tm(&workspace);
        let threads = tm.list_threads().await;
        let t = threads
            .iter()
            .find(|t| t.name == "t1")
            .expect("thread listed");
        let cost = t.cost.as_ref().expect("cost present");

        assert_eq!(
            cost.currency, MIXED_CURRENCY,
            "ledger's mixed marker must survive, not be replaced by config"
        );
        assert!((cost.today - 3.0).abs() < 1e-9);
        tm.shutdown().await;
    }

    /// Regression for #512: `ThreadManager::list_threads` must populate
    /// `ThreadInfo::branch` by reading `.git/HEAD` under each thread's
    /// path. Without this test, a future refactor that drops the call to
    /// `branch_for_thread_path` at the `threads.push(...)` site would
    /// silently leave `branch == None` on every payload.
    #[tokio::test]
    async fn list_threads_populates_branch_from_dot_git_head() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");

        // Thread "main-test" with a symbolic-ref HEAD pointing at main.
        let t1 = workspace.join("main-test");
        std::fs::create_dir_all(t1.join(".jyc")).unwrap();
        std::fs::create_dir_all(t1.join(".git")).unwrap();
        std::fs::write(t1.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        // Thread "detached-test" with a raw 40-char SHA — should appear
        // as "(detached)" rather than as `None`.
        let t2 = workspace.join("detached-test");
        std::fs::create_dir_all(t2.join(".jyc")).unwrap();
        std::fs::create_dir_all(t2.join(".git")).unwrap();
        std::fs::write(
            t2.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567",
        )
        .unwrap();

        // Thread "no-git" — `.jyc` exists but no `.git/HEAD`. Branch
        // should be `None` (renderer skips the row).
        let t3 = workspace.join("no-git");
        std::fs::create_dir_all(t3.join(".jyc")).unwrap();

        let tm = make_test_tm(&workspace);
        let threads = tm.list_threads().await;

        let by_name = |n: &str| {
            threads
                .iter()
                .find(|t| t.name == n)
                .unwrap_or_else(|| panic!("thread {n} missing from list_threads"))
        };

        assert_eq!(
            by_name("main-test").branch.as_deref(),
            Some("main"),
            "symbolic-ref branch must be resolved"
        );
        assert_eq!(
            by_name("detached-test").branch.as_deref(),
            Some("(detached)"),
            "raw SHA must surface as (detached)"
        );
        assert!(
            by_name("no-git").branch.is_none(),
            "non-git thread must have branch=None"
        );

        tm.shutdown().await;
    }

    /// Regression for #220: `ThreadManager::list_threads` must populate
    /// `ThreadInfo::changed_files` by running `git diff --name-only
    /// main...HEAD` under each thread's path. Without this test, a
    /// future refactor that drops the call to
    /// `changed_files_for_thread_path` at the `threads.push(...)` site
    /// would silently leave `changed_files == None` on every payload.
    #[tokio::test]
    async fn list_threads_populates_changed_files_from_git_diff() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("workspace");

        // Thread "clean": real git repo on `main` with no commits ahead.
        // Expect `Some(vec![])`.
        let clean = workspace.join("clean");
        std::fs::create_dir_all(clean.join(".jyc")).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&clean)
                .output()
                .expect("git failed")
        };
        run(&["init", "-q", "-b", "main"]);
        run(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);

        // Thread "ahead": feature branch with one commit adding "x.rs".
        let ahead = workspace.join("ahead");
        std::fs::create_dir_all(ahead.join(".jyc")).unwrap();
        let run_ahead = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&ahead)
                .output()
                .expect("git failed")
        };
        run_ahead(&["init", "-q", "-b", "main"]);
        run_ahead(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        run_ahead(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(ahead.join("x.rs"), "fn x() {}").unwrap();
        run_ahead(&["add", "x.rs"]);
        run_ahead(&[
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "x",
        ]);

        // Thread "no-git": no `.git` at all → `changed_files == None`.
        let no_git = workspace.join("no-git");
        std::fs::create_dir_all(no_git.join(".jyc")).unwrap();

        let tm = make_test_tm(&workspace);
        let threads = tm.list_threads().await;

        let by_name = |n: &str| {
            threads
                .iter()
                .find(|t| t.name == n)
                .unwrap_or_else(|| panic!("thread {n} missing from list_threads"))
        };

        assert_eq!(
            by_name("clean").changed_files.as_deref(),
            Some(&[][..]),
            "branch == main must surface as Some(vec![])"
        );
        assert_eq!(
            by_name("ahead").changed_files.as_deref(),
            Some(
                &[ChangedFileEntry {
                    path: "x.rs".into(),
                    uncommitted: false,
                    change: ChangeKind::Added,
                }][..]
            ),
            "feature branch with one new file must list it"
        );
        assert!(
            by_name("no-git").changed_files.is_none(),
            "non-git thread must have changed_files=None"
        );

        tm.shutdown().await;
    }
}
