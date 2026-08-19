//! Per-message processing pipeline (`process_message`) and its helpers.
//!
//! Extracted from the monolithic `topic_manager.rs`; the worker is the
//! free-function half of the module, while `TopicManager` (struct + impl)
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
use crate::command::context_handler::ContextCommandHandler;
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
use crate::pending_delivery::{read_signal_attachments, watch_pending_deliveries};
use crate::topic_json::TopicJson;
use jyc_types::{InboundMessage, OutboundAdapter, QueueItem};

use super::TopicManager;

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
                .store_at_path(&item.message, path, is_matched)
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

    // ── 1.1. WRITE THREAD.JSON (if channel provides metadata) ─────────
    // Channels like wecomkf embed customer info in message metadata.
    // Persist it once per topic so subsequent messages can read cached data.
    if item.message.channel == "wecomkf" {
        let topic_json_path = store_result.topic_path.join(".jyc").join("topic.json");
        if !topic_json_path.exists() {
            write_wecomkf_topic_json(&item.message, &store_result.topic_path, topic_name).await;
        }
    }

    // ── 1.2. WRITE THREAD ROUTING METADATA ────────────────────────────
    // Persist routing metadata on the first message for a topic so that
    // the dashboard's `TopicProxyHandler` (via /ws/<channel>/<topic>)
    // can restore it when constructing a synthetic InboundMessage.
    // Without this, the proxy's InboundMessage has empty metadata and
    // channel-specific reply routing fails (e.g., github_number missing → 404).
    let topic_meta_path = store_result.topic_path.join(".jyc").join("topic-meta.json");
    if !topic_meta_path.exists() && item.message.channel_uid != "dashboard" {
        let meta = serde_json::json!({
            "channel_uid": item.message.channel_uid,
            "external_id": item.message.external_id,
            "references": item.message.references,
            "metadata": item.message.metadata,
        });
        if let Ok(json) = serde_json::to_string_pretty(&meta) {
            let _ = tokio::fs::create_dir_all(store_result.topic_path.join(".jyc")).await;
            if let Err(e) = tokio::fs::write(&topic_meta_path, json).await {
                tracing::warn!(error = %e, "Failed to write topic-meta.json");
            } else {
                tracing::debug!(topic = %topic_name, "Wrote topic-meta.json");
            }
        }
    }

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
    command_registry.register(Box::new(CloseCommandHandler::new(topic_manager.clone())));
    command_registry.register(Box::new(CancelCommandHandler::new(topic_manager.clone())));
    command_registry.register(Box::new(PinCommandHandler::new(topic_manager.clone())));
    command_registry.register(Box::new(UnpinCommandHandler::new(topic_manager.clone())));
    command_registry.register(Box::new(ThinkingCommandHandler));
    command_registry.register(Box::new(ExchangeCommandHandler::new(topic_manager.clone())));
    command_registry.register(Box::new(ContextCommandHandler));

    // User-defined commands from config.toml `[[commands]]`. Registered last,
    // but `register()` warns on collisions and config validation rejects
    // names that shadow a built-in.
    for custom in &config.load().commands {
        command_registry.register(Box::new(CustomCommandHandler::new(custom.clone())));
    }

    let cmd_context = CommandContext {
        args: vec![],
        topic_path: store_result.topic_path.clone(),
        config: config.load_full(),
        channel: message.channel.clone(),
        channel_type: topic_manager.channel_type.clone(),
        agent: Some(agent.clone()),
        template_dirs: template_dirs.clone(),
        config_path: topic_manager.config_path.clone(),
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
                &store_result.topic_path,
                &store_result.message_dir,
                None,
            )
            .await?;
        // Publish a ReplySent event so the inspect server's ActivityTracker
        // fans it out as a chat_message to dashboard WS clients. Without
        // this, command results are persisted to disk (visible on re-enter)
        // but never appear live in the chat pane.
        topic_manager.publish_reply_sent(topic_name, &summary).await;
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
        .topic_path
        .join(".jyc")
        .join("question-sent.flag");
    if question_flag.exists() {
        tracing::info!("Topic is waiting for question answer, routing response");
        let answer_file = store_result
            .topic_path
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
    let delivery_topic_path = store_result.topic_path.clone();
    let delivery_message_dir = store_result.message_dir.clone();
    let delivery_message = message.clone();
    let delivery_outbound = outbound.clone();
    let delivery_topic_manager = topic_manager.clone();
    let delivery_topic_name = topic_name.to_string();
    let delivery_handle = tokio::spawn(async move {
        let event_bus = delivery_topic_manager
            .get_event_bus(&delivery_topic_name)
            .await;
        watch_pending_deliveries(
            &delivery_topic_path,
            &delivery_message_dir,
            &delivery_message,
            &*delivery_outbound,
            delivery_cancel_child,
            event_bus,
            &delivery_topic_name,
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
        topic_name,
        &store_result.topic_path,
        &store_result.message_dir,
        &mut dummy_rx,
        topic_cancel.clone(),
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
                            topic = %topic_name,
                            command = %trimmed,
                            "Executing slash command during AI processing"
                        );
                        let cmd_ctx = CommandContext {
                            args: vec![],
                            topic_path: store_result.topic_path.clone(),
                            config: config.load_full(),
                            channel: qi.message.channel.clone(),
                            channel_type: topic_manager.channel_type.clone(),
                            agent: Some(agent.clone()),
                            template_dirs: template_dirs.clone(),
                            config_path: topic_manager.config_path.clone(),
                        };
                        match command_registry.process_commands(trimmed, &cmd_ctx).await {
                            Ok(output) => {
                                if !output.results.is_empty() {
                                    let summary = output.results_summary();
                                    if let Err(e) = outbound
                                        .send_reply(
                                            &qi.message,
                                            &summary,
                                            &store_result.topic_path,
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
                                    topic_manager
                                        .publish_reply_sent(topic_name, &summary)
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
    // event bus on the TopicManager clone.
    for qi in buffered {
        if let Err(e) = tx_for_reenqueue.try_send(qi) {
            tracing::warn!(topic = %topic_name, error = %e, "Failed to re-enqueue buffered message");
        }
    }

    // Stop the delivery watcher
    delivery_cancel.cancel();
    let _ = delivery_handle.await;

    // Propagate agent errors only AFTER the background tasks above are
    // stopped. Returning early via `?` inside the select loop would leak
    // the delivery watcher.
    let result = result?;

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
    // The MCP reply tool stores the reply in the chat log and writes a signal file.
    // The monitor process (this code) handles actual delivery using its
    // pre-warmed outbound adapter with cached connections/tokens.
    if result.reply_sent_by_tool {
        // Check if the background delivery watcher already delivered the reply.
        // The watcher deletes reply-sent.flag after successful delivery.
        let signal_path = store_result.topic_path.join(".jyc").join("reply-sent.flag");
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
                    let reply_md = store_result.topic_path.join(".jyc").join("reply.md");
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
                    read_signal_attachments(&signal_path, &store_result.topic_path).await;

                outbound
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
                    .publish_reply_sent(topic_name, reply_text)
                    .await;
                // Clean up signal files after successful delivery to prevent re-delivery on restart
                tokio::fs::remove_file(&signal_path).await.ok();
                let reply_md_path = store_result.topic_path.join(".jyc").join("reply.md");
                tokio::fs::remove_file(&reply_md_path).await.ok();
                topic_manager.metrics.reply_by_tool(topic_name);
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
                &store_result.topic_path,
                &store_result.message_dir,
                None,
            )
            .await?;
        tracing::info!("Fallback reply sent");
        topic_manager.publish_reply_sent(topic_name, text).await;
        topic_manager.metrics.reply_by_fallback(topic_name);
    } else {
        tracing::warn!("No reply text from AI");
    }

    Ok(())
}

/// Read skills from topic's .jyc/skills.json file.
pub(crate) async fn read_skills(topic_path: &Path) -> Vec<String> {
    let skills_path = topic_path.join(".jyc").join("skills.json");
    match tokio::fs::read_to_string(&skills_path).await {
        Ok(content) => serde_json::from_str::<Vec<String>>(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Error returned when an existing topic directory was created from a
/// different template than the one the current message is requesting. The
/// topic manager surfaces this and drops the message rather than risk
/// overwriting AGENTS.md / template files in place.
#[cfg(test)]
mod topic_json_tests {
    use super::*;
    use jyc_types::{InboundMessage, MessageContent};
    use std::collections::HashMap;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_write_wecomkf_topic_json_creates_file() {
        let tmp = tempdir().unwrap();
        let topic_path = tmp.path().join("test-topic");

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
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata,
            matched_pattern: None,
        };

        write_wecomkf_topic_json(&message, &topic_path, "test-topic").await;

        let topic_json = TopicJson::read(&topic_path).await.unwrap().unwrap();
        assert_eq!(topic_json.channel_type, "wecomkf");
        assert_eq!(topic_json.version, 1);

        let data = topic_json.data_as::<serde_json::Value>().unwrap().unwrap();
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
    async fn test_write_wecomkf_topic_json_skips_without_external_userid() {
        let tmp = tempdir().unwrap();
        let topic_path = tmp.path().join("test-topic");

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
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        };

        write_wecomkf_topic_json(&message, &topic_path, "test-topic").await;

        // No external_userid in metadata → file should not be created
        assert!(!topic_path.join(".jyc/topic.json").exists());
    }

    #[tokio::test]
    async fn test_write_wecomkf_topic_json_fallback_user_name() {
        let tmp = tempdir().unwrap();
        let topic_path = tmp.path().join("test-topic");

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
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata,
            matched_pattern: None,
        };

        write_wecomkf_topic_json(&message, &topic_path, "test-topic").await;

        let topic_json = TopicJson::read(&topic_path).await.unwrap().unwrap();
        let data = topic_json.data_as::<serde_json::Value>().unwrap().unwrap();
        assert_eq!(
            data.get("user_name").and_then(|v| v.as_str()),
            Some("wm456")
        );
    }
}

/// Write `topic.json` for a WeCom KF topic from message metadata.
///
/// Extracts `external_userid`, `user_name`, and `open_kfid` from the
/// message metadata and persists them in `.jyc/topic.json`.
async fn write_wecomkf_topic_json(message: &InboundMessage, topic_path: &Path, topic_name: &str) {
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
        let topic_json = TopicJson {
            channel_type: "wecomkf".to_string(),
            version: 1,
            data: Some(serde_json::json!({
                "external_userid": external_userid,
                "user_name": user_name,
                "open_kfid": open_kfid,
                "first_message_at": chrono::Utc::now().to_rfc3339(),
            })),
        };
        if let Err(e) = topic_json.write(topic_path).await {
            tracing::warn!(
                error = %e,
                topic = %topic_name,
                "Failed to write topic.json"
            );
        } else {
            tracing::info!(topic = %topic_name, "Wrote topic.json");
        }
    }
}

#[cfg(test)]
mod has_active_queue;
