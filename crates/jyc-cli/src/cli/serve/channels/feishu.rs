//! `feishu` channel adapter wiring (extracted from serve/channels.rs).

use anyhow::Result;
use jyc_channels::feishu::client::FeishuClient;
use jyc_channels::feishu::inbound::{FeishuInboundAdapter, FeishuMatcher};
use jyc_core::duration::{DurationStyle, format_duration_secs};
use jyc_core::topic_manager::TopicManager;
use jyc_types::{ChannelConfig, InboundAdapter, InboundAttachmentConfig};
use serde_json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

/// Spawn a pipe-only feishu adapter: the inbound adapter plus one reply
/// forwarder per distinct pipe target channel.
///
/// Unlike full channels, a feishu adapter has no outbound adapter, agent
/// service, TopicManager, StateManager, or orchestrator registration — all
/// topics live in the pipe target (hub) channel. See
/// `docs/architecture/overview.md`.
#[allow(clippy::too_many_arguments)]
use super::*;

pub(crate) fn spawn_feishu_adapter(
    channel_config: &ChannelConfig,
    channel_name: String,
    workdir: &Path,
    inbound_attachment_config: Option<InboundAttachmentConfig>,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
    config_for_spawn: Arc<arc_swap::ArcSwap<jyc_types::AppConfig>>,
    ws_broadcasts: std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
    routers: HubRegistry,
) -> Result<()> {
    let feishu_config = channel_config
        .feishu
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing feishu config"))?
        .clone();
    // Feishu is pipe-only: every enabled pattern must name a pipe target (a
    // websocket hub channel). Collect the distinct target channels for reply
    // relaying; patterns without one are a configuration error — warn at
    // startup, drop matching messages at runtime.
    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("feishu", &channel_name, channel_config);

    let channel_span = tracing::info_span!("in", ch = %channel_name);
    // Owned copy for the task: `workdir` borrows from the caller.
    let workdir_for_task = workdir.to_path_buf();

    let task = tokio::spawn(
        async move {
            let adapter = FeishuInboundAdapter::new(&feishu_config, channel_name.clone());

            // Shared feishu client + topic->chat_id map for pipe relaying.
            let feishu_client = std::sync::Arc::new(FeishuClient::new(feishu_config.clone()));
            let topic_chat: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>> =
                std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
            // Per-topic start times, used for the reply footer
            // ("⏱ 耗时 <elapsed>") and the live status card. Entries live from
            // inbound until the topic is closed (chat disband) or
            // overwritten by the next message. In-memory only; lost on
            // restart (status cards stay frozen until cleared manually).
            let topic_starts: std::sync::Arc<
                std::sync::Mutex<HashMap<String, std::time::Instant>>,
            > = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

            // Live status message per topic (dedup registry): the first
            // progress watcher armed by a run posts the message and records
            // its id here; other watchers armed by the same run reuse the
            // id instead of posting a duplicate. The watcher removes the
            // entry when the run finalizes. In-memory only.
            let progress_cards: std::sync::Arc<
                tokio::sync::Mutex<HashMap<String, String>>,
            > = std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new()));

            // One reply forwarder per distinct pipe target channel:
            // subscribe to the target channel's broadcast and relay
            // replies back to feishu.
            //
            // Attachment relay needs the inspect server (reply broadcasts
            // carry download paths served by its files endpoint). Built
            // once here; `None` when inspect is disabled — text relaying
            // is unaffected, attachments are dropped with a warning.
            let inspect_client = {
                let cfg = config_for_spawn.load();
                cfg.inspect.as_ref().filter(|i| i.enabled).map(|i| {
                    let token = jyc_utils::auth_token::read_token(
                        &jyc_utils::auth_token::token_path(&workdir_for_task),
                    )
                    .ok();
                    jyc_inspect::client::InspectClient::with_token(
                        &loopback_addr(&i.bind),
                        token.as_deref(),
                    )
                })
            };
            for channel in &pipe_channels {
                let ws_broadcasts = ws_broadcasts.clone();
                let topic_chat = topic_chat.clone();
                let topic_starts = topic_starts.clone();
                let feishu_client = feishu_client.clone();
                let channel = channel.clone();
                let inspect_client = inspect_client.clone();
                let config_for_relay = config_for_spawn.clone();
                tokio::spawn(async move {
                    let Some(broadcast_tx) = wait_for_broadcast(&ws_broadcasts, &channel).await
                    else {
                        tracing::error!(
                            channel = %channel,
                            "feishu pipe: target channel broadcast never appeared (is it a websocket channel?), reply forwarder not started"
                        );
                        return;
                    };
                    let mut rx = broadcast_tx.subscribe();
                    tracing::info!(channel = %channel, "feishu pipe reply forwarder subscribed");
                    while let Ok(payload) = rx.recv().await {
                        let v: serde_json::Value = match serde_json::from_str(&payload) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if v.get("type").and_then(|t| t.as_str()) != Some("reply") {
                            continue;
                        }
                        let (Some(topic), Some(text)) = (
                            v.get("topic").and_then(|t| t.as_str()),
                            v.get("text").and_then(|t| t.as_str()),
                        ) else {
                            continue;
                        };
                        let Some(chat_id) = topic_chat.lock().unwrap().get(topic).cloned() else {
                            tracing::debug!(topic = %topic, "feishu pipe: no chat mapping for reply, skipping");
                            continue;
                        };
                        let send_result = {
                            // Completion footer: every relayed reply carries
                            // the elapsed time since the indicator started —
                            // useful for both the final reply and mid-run
                            // progress replies.
                            let elapsed = topic_starts
                                .lock()
                                .unwrap()
                                .get(topic)
                                .map(|s| s.elapsed().as_secs());
                            let text = match elapsed {
                                Some(s) => format!(
                                    "{text}\n\n⏱ 耗时 {}",
                                    format_duration_secs(s, DurationStyle::Precise)
                                ),
                                None => text.to_string(),
                            };
                            feishu_client.send_text_message(&chat_id, &text).await
                        };
                        if let Err(e) = &send_result {
                            tracing::error!(error = %e, "failed to relay reply to feishu");
                        }
                        // Relay reply attachments: download from the inspect
                        // server's files endpoint, re-upload to feishu.
                        for att in parse_reply_attachments(&v) {
                            let Some(inspect) = &inspect_client else {
                                tracing::warn!(
                                    filename = %att.filename,
                                    "feishu pipe: attachment dropped (inspect server disabled)"
                                );
                                continue;
                            };
                            if let Err(e) =
                                relay_attachment(inspect, &feishu_client, &chat_id, &att, &config_for_relay)
                                    .await
                            {
                                // Full chain (`{:#}`): the outer context
                                // alone hides the HTTP status (e.g. 401).
                                tracing::warn!(
                                    filename = %att.filename,
                                    error = format!("{e:#}"),
                                    "feishu pipe: failed to relay attachment"
                                );
                            }
                        }
                    }
                });
            }

            let topic_chat_for_close = topic_chat.clone();
            let topic_starts_for_close = topic_starts.clone();
            let routers_for_close = routers.clone();

            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let config_for_task = config_for_spawn.clone();
                    let topic_chat = topic_chat.clone();
                    let topic_starts = topic_starts.clone();
                    let progress_cards = progress_cards.clone();
                    let feishu_client = feishu_client.clone();
                    let channel_name_self = channel_name.clone();
                    let routers = routers.clone();
                    tokio::spawn(async move {
                        let cfg = config_for_task.load();
                        let patterns = cfg
                            .channels
                            .get(&channel_name_self)
                            .and_then(|c| c.patterns.clone())
                            .unwrap_or_default();
                        let Some((_pm, pattern)) =
                            match_pipe("feishu", &FeishuMatcher, &message, &patterns)
                        else {
                            return;
                        };
                        let pipe = pattern
                            .pipe
                            .as_ref()
                            .expect("match_pipe guarantees a pipe target");

                        // Re-target into the target channel/topic —
                        //    resolves the effective topic (`topic ?? pattern`)
                        //    and `${msg.chat_name}` placeholders against
                        //    message metadata.
                        let Some(message) = retarget_or_drop("feishu", message, pipe) else {
                            return;
                        };

                        // Record resolved topic -> chat_id for reply relay.
                        let chat_id = message
                            .metadata
                            .get("chat_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        if let Some(chat_id) = &chat_id {
                            topic_chat
                                .lock()
                                .unwrap()
                                .insert(message.topic.clone(), chat_id.clone());
                        }

                        // Progress indicator (best-effort): the watcher
                        // sends a live status card in the chat once
                        // processing actually starts, then updates it from
                        // the topic's event bus. The footer timing is
                        // recorded unconditionally.
                        //
                        // Skipped for commands that reply instantly and
                        // never reach the agent: those would never emit
                        // `ProcessingStarted`, so the watcher would sleep
                        // until MAX_LIFETIME and then double-post
                        // alongside the next real message's watcher.
                        //
                        // Spawned for commands that continue into an
                        // agent run (e.g. `/backlog pop` injects the
                        // popped text via `append_body`, custom prompt
                        // commands inject `user_prompt`), and for unknown
                        // slash names that fall through to the agent as a
                        // plain prompt. Shell custom commands also reply
                        // instantly and so are skipped — their flag is
                        // set by `all_commands_with` based on
                        // `CustomCommand::shell`.
                        let first_token = message
                            .content
                            .text
                            .as_deref()
                            .unwrap_or("")
                            .split_whitespace()
                            .next()
                            .unwrap_or("");
                        let custom = cfg.commands.clone();
                        let continues_to_agent = jyc_core::command::all_commands_with(&custom, &[])
                            .iter()
                            .find(|c| c.name == first_token)
                            .map(|c| c.continues_to_agent)
                            // Unknown slash name: assume the agent will run.
                            // This covers typos (`/foo`) and unknown
                            // commands that fall through to a normal
                            // message — both reach the agent.
                            .unwrap_or(true);
                        if continues_to_agent && let Some(cid) = &chat_id {
                            let start = std::time::Instant::now();
                            // Event freshness cutoff for the watcher — taken
                            // here, before routing, so no event of this run
                            // can predate it.
                            let seen_after = chrono::Utc::now();
                            let hub_tm = {
                                let reg = routers.lock().unwrap();
                                reg.get(&message.channel).map(|(_, tm)| tm.clone())
                            };
                            if let Some(tm) = hub_tm {
                                jyc_channels::feishu::progress::spawn_progress_watcher(
                                    feishu_client.clone(),
                                    tm,
                                    message.topic.clone(),
                                    cid.clone(),
                                    start,
                                    seen_after,
                                    progress_cards.clone(),
                                );
                            }
                            topic_starts
                                .lock()
                                .unwrap()
                                .insert(message.topic.clone(), start);
                        }

                        // Route through the target's own MessageRouter — the
                        // exact same path as a chat-pane message, so
                        // topic_path/template/skills apply identically.
                        route_into_pipe_target("feishu", &routers, pipe, message).await;
                    });
                    Ok(())
                }),
                on_topic_close: Some(Box::new(move |chat_id: String| {
                    let topic_chat = topic_chat_for_close.clone();
                    let topic_starts = topic_starts_for_close.clone();
                    let routers = routers_for_close.clone();
                    tokio::spawn(async move {
                        // Reverse-lookup: the disband event carries the
                        // upstream chat_id; the map records topic → chat_id.
                        let topics_to_close: Vec<String> = {
                            let map = topic_chat.lock().unwrap();
                            map.iter()
                                .filter(|(_, v)| v.as_str() == chat_id)
                                .map(|(t, _)| t.clone())
                                .collect()
                        };
                        if topics_to_close.is_empty() {
                            return;
                        }
                        // Collect out of both std mutexes before awaiting:
                        // their guards are not Send.
                        let hubs: Vec<(String, Arc<TopicManager>)> = {
                            let reg = routers.lock().unwrap();
                            reg.iter()
                                .map(|(name, (_, tm))| (name.clone(), tm.clone()))
                                .collect()
                        };
                        for topic in &topics_to_close {
                            for (hub_name, tm) in &hubs {
                                if let Err(e) = tm.auto_close_topic(topic).await {
                                    tracing::debug!(
                                        hub = %hub_name,
                                        topic = %topic,
                                        chat_id = %chat_id,
                                        error = %e,
                                        "feishu pipe: auto_close_topic ignored (no such topic in this hub)"
                                    );
                                }
                            }
                        }
                        topic_chat.lock().unwrap().retain(|_, v| v != &chat_id);
                        for topic in &topics_to_close {
                            topic_starts.lock().unwrap().remove(topic);
                        }
                    });
                    Ok(())
                })),
                on_close_event: None,
                on_error: Box::new(|error| {
                    tracing::error!(error = %error, "Feishu inbound error");
                }),
                attachment_config: inbound_attachment_config.clone(),
            };

            if let Err(e) = adapter.start(options, cancel).await {
                tracing::error!(
                    error = %e,
                    "Feishu inbound adapter error"
                );
            }
        }
        .instrument(channel_span),
    );
    tasks.push(task);
    Ok(())
}
