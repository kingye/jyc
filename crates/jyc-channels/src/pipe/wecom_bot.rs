//! WeCom smart-bot (`wecom_bot`) pipe wiring: the aibot WS callback adapter
//! running inside `jyc-pipe`, replacing the deleted in-process
//! `spawn_wecom_bot_adapter` (`jyc-cli/src/cli/serve/channels/wecom_bot.rs`)
//! over the hub websocket.
//!
//! - Inbound: `WecomBotInboundAdapter` (shared `WecomBotConnectionHandle`
//!   populated by the WS connect callback) → pattern match → retarget →
//!   `message` frame on the target channel's [`HubPipe`], with the media the
//!   adapter downloaded uploaded to the hub's inbound endpoint first.
//! - Replies: the pipe's reply stream → `finish=true` streaming update on
//!   the opened stream (falling back to proactive `aibot_send_msg` when
//!   the streaming window has closed), attachments via proactive send.
//! - No close events (adapters run per message; the WeCom side has no
//!   close-event stream).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use jyc_types::{ChannelConfig, InboundAdapter};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::wecom_bot;
use crate::wecom_bot::client::WecomBotConnectionHandle;
use crate::wecom_bot::inbound::{WecomBotInboundAdapter, WecomBotMatcher};

use super::{
    HubPipe, ReplyAttachmentRef, collect_pipe_target_channels, fetch_topic_file, match_pipe,
    parse_reply_attachments, retarget_or_drop, warn_on_bad_pipe_patterns,
};

/// State tracked per piped topic for the wecom_bot reply forwarder.
///
/// - `req_id`: correlation id from the inbound WebSocket callback. Echoed
///   in the streaming reply's `aibot_respond_msg` headers.
/// - `stream_id`: opaque stream id used to update the streaming message
///   in-place (the same id is reused for the `finish=false` indicator
///   and the `finish=true` final reply).
/// - `recipient`: the chat/single-chat target id (group chatid or single
///   userid) — used for the proactive `aibot_send_msg` channel for
///   outbound attachments. Storing it here avoids the forwarder having
///   to recompute it from the broadcast payload.
#[derive(Debug, Clone)]
struct WecomReplyState {
    req_id: String,
    stream_id: String,
    recipient: String,
}

/// Cadence at which the keep-alive task pings `finish=false` to keep
/// the WeCom passive-reply window open during long agent runs.
const WECOM_KEEP_ALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Safety deadline for the keep-alive task. If no reply is delivered
/// within this window (e.g. agent crashed or stuck), the task stops
/// itself and removes the recorded state so the entry does not leak.
const WECOM_KEEP_ALIVE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Braille spinner frames for the keep-alive "thinking…" indicator.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Spawn the wecom_bot pipe wiring: the inbound adapter plus one reply
/// forwarder per distinct pipe target channel.
///
/// Mirrors `pipe::github::spawn_github_pipe`. Unlike full channels, owns
/// no TopicManager/agent/orchestrator — all topics live in the pipe target
/// (hub) channel. wecom_bot specifics:
///
/// - Uses a shared `WecomBotConnectionHandle` (set by the inbound
///   adapter on WS connect) instead of an HTTP client.
/// - Sends a `finish=false` streaming reply immediately when a message
///   arrives (the user-visible "thinking" indicator). The streaming
///   window must be opened before the agent runs because the agent
///   can take minutes and the WeCom passive reply window is short.
/// - Text replies try `finish=true` first; when the streaming window
///   has already closed (no keep-alive spinner, common for long
///   agent runs) the server rejects the ack and the forwarder falls
///   back to proactive `aibot_send_msg` so the user still receives
///   the answer. Likewise, attachments always go via proactive
///   `aibot_send_msg` for the same reason.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_wecom_bot_pipe(
    channel_config: &ChannelConfig,
    channel_name: String,
    config: Arc<jyc_types::AppConfig>,
    hubs: Arc<HashMap<String, Arc<HubPipe>>>,
    files_base: Option<String>,
    token: Option<String>,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Result<()> {
    let wecom_bot_config = channel_config
        .wecom_bot
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing wecom_bot config"))?
        .clone();
    let inbound_attachment_config = config.attachments.as_ref().and_then(|a| a.inbound.clone());

    // wecom_bot is pipe-only: every enabled pattern must name a pipe
    // target (a websocket hub channel). Collect the distinct targets
    // for reply relaying; patterns without one are a configuration
    // error — warn at startup, drop matching messages at runtime.
    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("wecom_bot", &channel_name, channel_config);

    // Pattern snapshot taken at startup (patterns may not change between
    // the hub reading them and this pipe claiming the channel).
    let patterns = channel_config.patterns.clone().unwrap_or_default();

    let channel_span = tracing::info_span!("in", ch = %channel_name);

    tasks.push(tokio::spawn(
        async move {
            // Shared WS connection handle; populated by the inbound
            // adapter's `on_connect` callback after subscribe.
            let handle_arc: Arc<tokio::sync::Mutex<Option<WecomBotConnectionHandle>>> =
                Arc::new(tokio::sync::Mutex::new(None));

            // topic → {req_id, stream_id, recipient} for the reply forwarder.
            let topic_state: Arc<tokio::sync::Mutex<HashMap<String, WecomReplyState>>> =
                Arc::new(tokio::sync::Mutex::new(HashMap::new()));

            // One reply forwarder per distinct pipe target channel.
            for target in &pipe_channels {
                let Some(hub) = hubs.get(target).cloned() else {
                    continue;
                };
                let topic_state = topic_state.clone();
                let handle_arc = handle_arc.clone();
                let files_base = files_base.clone();
                let token = token.clone();
                let config = config.clone();
                let target = target.clone();
                tokio::spawn(async move {
                    let mut rx = hub.subscribe_replies();
                    tracing::info!(channel = %target, "wecom_bot pipe reply forwarder subscribed");
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

                        // Look up the streaming reply state for this topic.
                        //
                        // Known limitation: state is keyed by topic, so
                        // rapid successive messages in the same chat
                        // before a reply overwrites the previous entry —
                        // the older stream stays at "thinking…".
                        // The hub's reply broadcast does not carry the
                        // original req_id / stream_id, so per-message
                        // correlation would require threading a
                        // correlation id through the hub. Documented
                        // here; consider narrowing the key when a
                        // concrete case appears.
                        let state = topic_state.lock().await.remove(topic);
                        let Some(state) = state else {
                            tracing::debug!(
                                topic = %topic,
                                "wecom_bot pipe: no topic state for reply, skipping"
                            );
                            continue;
                        };

                        // Wait for the WS handle to be set (it is set
                        // by the inbound adapter on connect, before any
                        // message callback fires).
                        let Some(handle) = handle_arc.lock().await.clone() else {
                            tracing::warn!(
                                topic = %topic,
                                "wecom_bot pipe: handle not set, skipping reply"
                            );
                            continue;
                        };

                        // 1. Stream the final reply text (finish=true).
                        //    If the streaming window has already closed
                        //    (common for long agent runs — no keep-alive
                        //    spinner) the server rejects with errcode
                        //    846604. Fall back to proactive
                        //    aibot_send_msg so the user still receives
                        //    the answer.
                        let streamed =
                            wecom_bot::send_stream_reply_and_wait(
                                &handle,
                                &state.req_id,
                                &state.stream_id,
                                text,
                                true,
                            )
                            .await;
                        if let Err(e) = streamed {
                            tracing::warn!(
                                error = format!("{e:#}"),
                                topic = %topic,
                                "wecom_bot pipe: stream reply rejected, falling back to proactive send"
                            );
                            if let Err(e2) =
                                send_wecom_proactive_text(&handle, &state.recipient, text).await
                            {
                                tracing::error!(
                                    error = format!("{e2:#}"),
                                    topic = %topic,
                                    "wecom_bot pipe: proactive fallback also failed"
                                );
                            }
                        }

                        // 2. Relay attachments via proactive aibot_send_msg.
                        for att in parse_reply_attachments(&v) {
                            let Some(files_base) = files_base.as_deref() else {
                                tracing::warn!(
                                    filename = %att.filename,
                                    "wecom_bot pipe: attachment dropped (inspect server disabled)"
                                );
                                continue;
                            };
                            if let Err(e) = relay_wecom_attachment(
                                &handle,
                                files_base,
                                token.as_deref(),
                                &state.recipient,
                                &att,
                                &config,
                            )
                            .await
                            {
                                tracing::warn!(
                                    filename = %att.filename,
                                    error = format!("{e:#}"),
                                    "wecom_bot pipe: failed to relay attachment"
                                );
                            }
                        }
                    }
                });
            }

            let adapter =
                WecomBotInboundAdapter::with_shared_handle(&wecom_bot_config, channel_name.clone(), handle_arc.clone());

            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let hubs = hubs.clone();
                    let patterns = patterns.clone();
                    let topic_state = topic_state.clone();
                    let handle_arc = handle_arc.clone();
                    let hub_files = crate::pipe::HubFiles::new(
                        files_base.clone(),
                        token.clone(),
                    );
                    let config = config.clone();
                    tokio::spawn(async move {
                        let Some((_pm, pattern)) =
                            match_pipe("wecom_bot", &WecomBotMatcher, &message, &patterns)
                        else {
                            return;
                        };
                        let pipe = pattern
                            .pipe
                            .as_ref()
                            .expect("match_pipe guarantees a pipe target");

                        // Re-target into the target channel/topic.
                        let Some(message) = retarget_or_drop("wecom_bot", message, pipe) else {
                            return;
                        };

                        // Send the streaming "thinking" indicator
                        // (finish=false) immediately. The streaming
                        // window must be opened before the agent runs
                        // because the agent can take minutes and the
                        // WeCom passive reply window is short. No-op
                        // when the handle is not yet set or the
                        // original message lacks a req_id (a
                        // configured edge case — the reply can still
                        // be relayed without an indicator).
                        let req_id = message
                            .metadata
                            .get("req_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        let stream_id = uuid::Uuid::new_v4().to_string();
                        let recipient = message.channel_uid.clone();
                        if let Some(req_id) = req_id.as_deref()
                            && let Some(handle) = handle_arc.lock().await.clone()
                            && let Err(e) = wecom_bot::send_stream_reply(
                                &handle,
                                req_id,
                                &stream_id,
                                "正在思考中...",
                                false,
                            )
                            .await
                        {
                            tracing::warn!(
                                error = format!("{e:#}"),
                                "wecom_bot pipe: failed to send processing indicator"
                            );
                        }

                        // Record resolved topic → streaming state for
                        // the reply forwarder (and the keep-alive).
                        let resolved_topic = message.topic.clone();
                        let resolved_state = WecomReplyState {
                            req_id: req_id.unwrap_or_default(),
                            stream_id,
                            recipient,
                        };
                        topic_state
                            .lock()
                            .await
                            .insert(resolved_topic.clone(), resolved_state.clone());

                        // Spawn keep-alive task to keep the streaming
                        // window open during long agent runs. Sends
                        // `finish=false` with a rotating spinner every
                        // WECOM_KEEP_ALIVE_INTERVAL. Self-terminates
                        // when the reply is delivered (state removed
                        // by the forwarder) or when the safety
                        // deadline expires. No-op when the original
                        // message lacked a req_id (no stream was
                        // opened, so nothing to keep alive).
                        if !resolved_state.req_id.is_empty() {
                            let keep_alive_handle_arc = handle_arc.clone();
                            let keep_alive_topic_state = topic_state.clone();
                            let keep_alive_topic = resolved_topic.clone();
                            let keep_alive_req_id = resolved_state.req_id.clone();
                            let keep_alive_stream_id = resolved_state.stream_id.clone();
                            tokio::spawn(async move {
                                let mut interval = tokio::time::interval(WECOM_KEEP_ALIVE_INTERVAL);
                                let started = std::time::Instant::now();
                                let mut frame_idx = 0usize;
                                loop {
                                    interval.tick().await;
                                    // Reply delivered (forwarder removed the entry).
                                    if !keep_alive_topic_state
                                        .lock()
                                        .await
                                        .contains_key(&keep_alive_topic)
                                    {
                                        break;
                                    }
                                    // Safety deadline: give up after the
                                    // deadline and clean up the entry so it
                                    // does not leak (avoids the old
                                    // progress-oner "never-ending 846604
                                    // WARN storm" bug).
                                    if started.elapsed() > WECOM_KEEP_ALIVE_DEADLINE {
                                        tracing::warn!(
                                            topic = %keep_alive_topic,
                                            "wecom_bot keep-alive: deadline reached, cleaning up state"
                                        );
                                        keep_alive_topic_state
                                            .lock()
                                            .await
                                            .remove(&keep_alive_topic);
                                        break;
                                    }
                                    let frame = SPINNER_FRAMES[frame_idx % SPINNER_FRAMES.len()];
                                    let elapsed = started.elapsed().as_secs();
                                    let content =
                                        format!("{} 正在处理中... (已用 {}s)", frame, elapsed);
                                    if let Some(handle) =
                                        keep_alive_handle_arc.lock().await.clone()
                                        && let Err(e) = wecom_bot::send_stream_reply(
                                            &handle,
                                            &keep_alive_req_id,
                                            &keep_alive_stream_id,
                                            &content,
                                            false,
                                        )
                                        .await
                                    {
                                        tracing::debug!(
                                            error = format!("{e:#}"),
                                            topic = %keep_alive_topic,
                                            "wecom_bot keep-alive: send failed (window may be closed)"
                                        );
                                    }
                                    frame_idx += 1;
                                }
                            });
                        }

                        // Send the frame to the hub pipe for the
                        // retargeted channel.
                        let Some(hub) = hubs.get(&message.channel) else {
                            tracing::warn!(
                                channel = %message.channel,
                                "wecom_bot pipe: no hub pipe for target channel, dropping"
                            );
                            return;
                        };
                        // Upload the media the adapter downloaded to the hub
                        // first (it has no access to the hub's filesystem),
                        // then announce it in the frame.
                        let attachments = hub_files
                            .stage_attachments(
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
                    });
                    Ok(())
                }),
                on_topic_close: None,
                on_close_event: None,
                on_error: Box::new(|error| {
                    tracing::error!(error = %error, "WeCom Bot inbound error");
                }),
                attachment_config: inbound_attachment_config,
            };

            if let Err(e) = adapter.start(options, cancel).await {
                tracing::error!(error = %e, "WeCom Bot inbound adapter error");
            }
        }
        .instrument(channel_span),
    ));
    Ok(())
}

/// Send a proactive text message via `aibot_send_msg`, keyed by the
/// recipient (group chatid or single userid).
///
/// Fallback path when the streaming `finish=true` ack is rejected
/// (typically errcode 846604 — the WeCom passive-reply window has
/// closed, common for long agent runs). The body wire format is built
/// by the shared `build_proactive_text_body` helper.
async fn send_wecom_proactive_text(
    handle: &WecomBotConnectionHandle,
    recipient: &str,
    text: &str,
) -> Result<()> {
    let body = wecom_bot::build_proactive_text_body(recipient, text);
    send_aibot_msg(handle, body).await?;
    tracing::info!(
        recipient = %recipient,
        text_len = text.len(),
        "wecom_bot pipe: proactive text reply sent"
    );
    Ok(())
}

/// Wrap one `aibot_send_msg` body in the cmd envelope and push it onto
/// the shared WS handle's sender.
async fn send_aibot_msg(handle: &WecomBotConnectionHandle, body: serde_json::Value) -> Result<()> {
    let req_id = wecom_bot::client::generate_req_id("aibot_send_msg");
    let json = serde_json::json!({
        "cmd": "aibot_send_msg",
        "headers": {"req_id": req_id},
        "body": body,
    })
    .to_string();
    handle
        .sender
        .send(json)
        .map_err(|e| anyhow::anyhow!("wecom_bot pipe: failed to send aibot_send_msg: {e}"))?;
    Ok(())
}

/// Download one reply attachment from the hub's files endpoint, upload
/// it to WeCom media, and send the media message via `aibot_send_msg`
/// (proactive) keyed by the recipient.
///
/// Same wiring as feishu's `relay_attachment`, sharing the download +
/// outbound-policy check (`fetch_topic_file`) and differing only in the
/// upload/send calls. Proactive send is used here instead of
/// `aibot_respond_msg` because the agent's reply is async and the WeCom
/// passive reply window may have closed by the time the forwarder
/// relays attachments.
async fn relay_wecom_attachment(
    handle: &WecomBotConnectionHandle,
    files_base: &str,
    token: Option<&str>,
    recipient: &str,
    att: &ReplyAttachmentRef,
    config: &jyc_types::AppConfig,
) -> Result<()> {
    use crate::wecom_bot::{build_media_message_body, upload_attachment, wecom_media_type};

    let tmp = fetch_topic_file(files_base, token, att, config).await?;

    let media_id = upload_attachment(handle, tmp.path(), &att.filename, &att.content_type).await?;
    let media_type = wecom_media_type(&att.content_type, &att.filename);
    let mut body = build_media_message_body(media_type, &media_id);
    body["chatid"] = serde_json::Value::String(recipient.to_string());
    send_aibot_msg(handle, body).await?;
    tracing::info!(filename = %att.filename, recipient = %recipient, "wecom_bot pipe: attachment relayed");
    Ok(())
}
