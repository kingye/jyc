//! Feishu pipe wiring: the peripheral feishu adapter running inside
//! `jyc-pipe`, mirroring the in-process `spawn_feishu_adapter`
//! (`jyc-cli/src/cli/serve/channels/feishu.rs`) over the hub websocket.
//!
//! - Inbound messages: `FeishuInboundAdapter` → pattern match + retarget →
//!   `message` frame on the target channel's [`HubPipe`], with the images and
//!   files the adapter downloaded uploaded to the hub's inbound endpoint first.
//! - Replies: the pipe's reply stream → feishu relay (completion footer +
//!   attachment download from the hub's files endpoint).
//! - Live status cards: the pipe's `topic_event` stream feeds the shared
//!   progress watcher (no `TopicManager` in the pipe process — the card
//!   omits mode/model/context segments).
//! - Chat disband: `close_topic` frame to every connected hub pipe.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use jyc_core::duration::{DurationStyle, format_duration_secs};
use jyc_types::{ChannelConfig, InboundAdapter, InboundMessage};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::feishu::client::FeishuClient;
use crate::feishu::inbound::{FeishuInboundAdapter, FeishuMatcher};
use crate::pipe::hub::HubPipe;
use crate::pipe::{
    ReplyAttachmentRef, collect_pipe_target_channels, match_pipe, parse_reply_attachments,
    retarget_or_drop, warn_on_bad_pipe_patterns,
};

/// Shared per-process state for one feishu pipe channel.
struct PipeState {
    /// Resolved topic → feishu chat_id, for reply relay and disband
    /// reverse-lookup. In-memory only.
    topic_chat: std::sync::Mutex<HashMap<String, String>>,
    /// Per-topic start times for the reply footer ("⏱ 耗时 <elapsed>") and
    /// the live status card. In-memory only.
    topic_starts: std::sync::Mutex<HashMap<String, std::time::Instant>>,
    /// Live status message id per topic (dedup registry). In-memory only.
    /// `Arc`-wrapped because the map is handed to the progress watcher
    /// task by value.
    progress_cards: Arc<tokio::sync::Mutex<HashMap<String, String>>>,
}

/// Spawn the feishu pipe channel: inbound adapter + reply forwarders +
/// disband handling, all over the given hub pipes.
///
/// `hubs` must contain one [`HubPipe`] per pipe-target channel this
/// channel's patterns reference (built by `pipe::run`). `files_base` is
/// the hub's HTTP origin for attachment download (`None` when the inspect
/// server is disabled — attachments are then dropped with a warning).
#[allow(clippy::too_many_arguments)]
pub fn spawn_feishu_pipe(
    channel_config: &ChannelConfig,
    channel_name: String,
    config: Arc<jyc_types::AppConfig>,
    hubs: Arc<HashMap<String, Arc<HubPipe>>>,
    files_base: Option<String>,
    token: Option<String>,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Result<()> {
    let feishu_config = channel_config
        .feishu
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing feishu config"))?
        .clone();
    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("feishu", &channel_name, channel_config);
    for target in &pipe_channels {
        if !hubs.contains_key(target) {
            tracing::warn!(
                channel = %channel_name,
                target = %target,
                "feishu pipe: no hub pipe for target channel (config mismatch?)"
            );
        }
    }

    let state = Arc::new(PipeState {
        topic_chat: std::sync::Mutex::new(HashMap::new()),
        topic_starts: std::sync::Mutex::new(HashMap::new()),
        progress_cards: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    });
    let feishu_client = Arc::new(FeishuClient::new(feishu_config.clone()));

    // One reply forwarder per distinct pipe target channel.
    for target in &pipe_channels {
        let Some(hub) = hubs.get(target).cloned() else {
            continue;
        };
        let state = state.clone();
        let client = feishu_client.clone();
        let config = config.clone();
        let files_base = files_base.clone();
        let token = token.clone();
        let target = target.clone();
        let span = tracing::info_span!("out", ch = %channel_name, target = %target);
        tasks.push(tokio::spawn(
            async move {
                let mut rx = hub.subscribe_replies();
                tracing::info!(channel = %target, "feishu pipe reply forwarder subscribed");
                loop {
                    // Lagged: a burst overflowed the broadcast buffer —
                    // skip the missed frames and keep relaying. Closed:
                    // the pipe is gone for good.
                    let payload = match rx.recv().await {
                        Ok(payload) => payload,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    };
                    let v: serde_json::Value = match serde_json::from_str(&payload) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    // Questions are never relayed here: `ask_user` only
                    // pushes to a channel that can answer one, and a feishu
                    // turn asks in its reply text instead.
                    if v.get("type").and_then(|t| t.as_str()) != Some("reply") {
                        continue;
                    }
                    let (Some(topic), Some(text)) = (
                        v.get("topic").and_then(|t| t.as_str()),
                        v.get("text").and_then(|t| t.as_str()),
                    ) else {
                        continue;
                    };
                    let Some(chat_id) = state.topic_chat.lock().unwrap().get(topic).cloned()
                    else {
                        tracing::debug!(topic = %topic, "feishu pipe: no chat mapping for reply, skipping");
                        continue;
                    };
                    // Completion footer: elapsed time since the indicator
                    // started — useful for both the final reply and
                    // mid-run progress replies.
                    let elapsed = state
                        .topic_starts
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
                    if let Err(e) = client.send_text_message(&chat_id, &text).await {
                        tracing::error!(error = %e, "failed to relay reply to feishu");
                    }
                    for att in parse_reply_attachments(&v) {
                        let Some(base) = &files_base else {
                            tracing::warn!(
                                filename = %att.filename,
                                "feishu pipe: attachment dropped (inspect server disabled)"
                            );
                            continue;
                        };
                        if let Err(e) = relay_attachment(
                            base,
                            token.as_deref(),
                            &client,
                            &chat_id,
                            &att,
                            &config,
                        )
                        .await
                        {
                            // Full chain (`{:#}`): the outer context alone
                            // hides the HTTP status (e.g. 401).
                            tracing::warn!(
                                filename = %att.filename,
                                error = format!("{e:#}"),
                                "feishu pipe: failed to relay attachment"
                            );
                        }
                    }
                }
            }
            .instrument(span),
        ));
    }

    // Inbound adapter task.
    let channel_span = tracing::info_span!("in", ch = %channel_name);
    let state_for_adapter = state.clone();
    let channel_name_for_adapter = channel_name.clone();
    tasks.push(tokio::spawn(
        async move {
            let adapter =
                FeishuInboundAdapter::new(&feishu_config, channel_name_for_adapter.clone());
            let state = state_for_adapter;
            let hubs_message = hubs.clone();
            let hubs_close = hubs.clone();
            let state_close = state.clone();
            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let config = config.clone();
                    let hubs = hubs_message.clone();
                    let state = state.clone();
                    let client = feishu_client.clone();
                    let channel_name = channel_name_for_adapter.clone();
                    let files_base = files_base.clone();
                    let token = token.clone();
                    tokio::spawn(async move {
                        handle_inbound(
                            config,
                            hubs,
                            state,
                            client,
                            channel_name,
                            files_base,
                            token,
                            message,
                        )
                        .await;
                    });
                    Ok(())
                }),
                on_topic_close: Some(Box::new(move |chat_id: String| {
                    let state = state_close.clone();
                    let hubs = hubs_close.clone();
                    tokio::spawn(async move {
                        handle_disband(state, hubs, chat_id).await;
                    });
                    Ok(())
                })),
                on_close_event: None,
                on_error: Box::new(|error| {
                    tracing::error!(error = %error, "Feishu pipe inbound error");
                }),
                attachment_config: None,
            };
            if let Err(e) = adapter.start(options, cancel).await {
                tracing::error!(error = %e, "Feishu pipe inbound adapter error");
            }
        }
        .instrument(channel_span),
    ));
    Ok(())
}

/// Route one inbound feishu message into the hub: pattern match, retarget,
/// record the chat mapping, arm the progress watcher, send the frame.
async fn handle_inbound(
    config: Arc<jyc_types::AppConfig>,
    hubs: Arc<HashMap<String, Arc<HubPipe>>>,
    state: Arc<PipeState>,
    feishu_client: Arc<FeishuClient>,
    channel_name: String,
    files_base: Option<String>,
    token: Option<String>,
    message: InboundMessage,
) {
    let patterns = config
        .channels
        .get(&channel_name)
        .and_then(|c| c.patterns.clone())
        .unwrap_or_default();
    let Some((_pm, pattern)) = match_pipe("feishu", &FeishuMatcher, &message, &patterns) else {
        return;
    };
    let pipe = pattern
        .pipe
        .as_ref()
        .expect("match_pipe guarantees a pipe target");
    let Some(message) = retarget_or_drop("feishu", message, pipe) else {
        return;
    };

    // Record resolved topic → chat_id for reply relay.
    let chat_id = message
        .metadata
        .get("chat_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if let Some(chat_id) = &chat_id {
        state
            .topic_chat
            .lock()
            .unwrap()
            .insert(message.topic.clone(), chat_id.clone());
    }

    // Arm the live status card for turns that continue into an agent run
    // (same command classification as the in-process wiring).
    let first_token = message
        .content
        .text
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .next()
        .unwrap_or("");
    let continues_to_agent = jyc_core::command::all_commands_with(&config.commands, &[], &[])
        .iter()
        .find(|c| c.name == first_token)
        .map(|c| c.continues_to_agent)
        .unwrap_or(true);
    if continues_to_agent && let Some(cid) = &chat_id {
        let start = std::time::Instant::now();
        // Freshness cutoff — taken before sending, so no event of this run
        // can predate it.
        let seen_after = chrono::Utc::now();
        if let Some(hub) = hubs.get(&message.channel) {
            let topic = message.topic.clone();
            let mut events = hub.subscribe_events();
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let forward_topic = topic.clone();
            tokio::spawn(async move {
                loop {
                    let pe = tokio::select! {
                        // Watcher ended (card completed / lifetime
                        // bound): stop filtering, don't leak the task.
                        _ = tx.closed() => return,
                        received = events.recv() => match received {
                            Ok(pe) => pe,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                continue
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                        },
                    };
                    if pe.topic != forward_topic {
                        continue;
                    }
                    if tx.try_send(pe.event).is_err() {
                        tracing::debug!(
                            topic = %forward_topic,
                            "feishu pipe: progress event queue full, event dropped"
                        );
                    }
                }
            });
            crate::feishu::progress::spawn_progress_watcher_pipe(
                feishu_client,
                topic,
                cid.clone(),
                start,
                seen_after,
                state.progress_cards.clone(),
                rx,
            );
        }
        state
            .topic_starts
            .lock()
            .unwrap()
            .insert(message.topic.clone(), start);
    }

    // Send the frame to the hub pipe for the retargeted channel.
    let Some(hub) = hubs.get(&message.channel) else {
        tracing::warn!(
            channel = %message.channel,
            "feishu pipe: no hub pipe for target channel, dropping"
        );
        return;
    };
    // Upload the images/files the adapter downloaded to the hub first (it has
    // no access to the hub's filesystem), then announce them in the frame.
    let attachments = crate::pipe::upload_inbound_attachments(
        files_base.as_deref(),
        token.as_deref(),
        &message.channel,
        &message.attachments,
        &config,
    )
    .await;

    hub.send_message_with_attachments(
        &message.topic,
        message.content.text.as_deref().unwrap_or(""),
        &message.sender,
        &message.sender_address,
        message.metadata,
        &attachments,
    );
}

/// Chat disband: close every topic mapped to the chat on every hub pipe,
/// then drop the local mappings.
async fn handle_disband(
    state: Arc<PipeState>,
    hubs: Arc<HashMap<String, Arc<HubPipe>>>,
    chat_id: String,
) {
    let topics_to_close: Vec<String> = {
        let map = state.topic_chat.lock().unwrap();
        map.iter()
            .filter(|(_, v)| v.as_str() == chat_id)
            .map(|(t, _)| t.clone())
            .collect()
    };
    if topics_to_close.is_empty() {
        return;
    }
    for topic in &topics_to_close {
        for hub in hubs.values() {
            hub.send_close_topic(topic);
        }
    }
    state
        .topic_chat
        .lock()
        .unwrap()
        .retain(|_, v| v != &chat_id);
    for topic in &topics_to_close {
        state.topic_starts.lock().unwrap().remove(topic);
    }
}

/// Download one reply attachment from the hub's files endpoint and send it
/// to the feishu chat (image vs. file chosen by content type).
async fn relay_attachment(
    files_base: &str,
    token: Option<&str>,
    client: &FeishuClient,
    chat_id: &str,
    att: &ReplyAttachmentRef,
    config: &jyc_types::AppConfig,
) -> Result<()> {
    use crate::feishu::client::{feishu_file_type, is_image_content_type};

    let tmp = super::fetch_topic_file(files_base, token, att, config).await?;

    if is_image_content_type(&att.content_type) {
        let key = client.upload_image(tmp.path(), &att.filename).await?;
        client.send_image_message(chat_id, &key).await?;
    } else {
        let ext = std::path::Path::new(&att.filename)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let key = client
            .upload_file(tmp.path(), &att.filename, feishu_file_type(&ext))
            .await?;
        client.send_file_message(chat_id, &key).await?;
    }
    tracing::info!(filename = %att.filename, "feishu pipe: attachment relayed");
    Ok(())
}
