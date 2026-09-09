//! `wecom_bot` channel adapter wiring (extracted from serve/channels.rs).

use anyhow::Result;
use jyc_channels::wecom_bot::inbound::{WecomBotInboundAdapter, WecomBotMatcher};
use jyc_types::{ChannelConfig, InboundAdapter, InboundAttachmentConfig};
use serde_json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::wecom::{relay_wecom_attachment, send_wecom_proactive_text};
use super::*;

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

/// Spawn a pipe-only wecom_bot adapter: the inbound adapter plus one
/// reply forwarder per distinct pipe target channel.
///
/// Mirrors `spawn_feishu_adapter` (see `docs/architecture/overview.md`).
/// Differences specific to wecom_bot:
///
/// - Uses a shared `WecomBotConnectionHandle` (set by the inbound
///   adapter on WS connect) instead of an HTTP client. The outbound
///   adapter is intentionally NOT constructed — there is no
///   `TopicManager`/agent/orchestrator wired for a pipe-only adapter.
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
pub(crate) fn spawn_wecom_bot_adapter(
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
    use jyc_channels::wecom_bot;
    let wecom_bot_config = channel_config
        .wecom_bot
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing wecom_bot config"))?
        .clone();

    // wecom_bot is pipe-only: every enabled pattern must name a pipe
    // target (a websocket hub channel). Collect the distinct targets
    // for reply relaying; patterns without one are a configuration
    // error — warn at startup, drop matching messages at runtime.
    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("wecom_bot", &channel_name, channel_config);

    let channel_span = tracing::info_span!("in", ch = %channel_name);
    let workdir_for_task = workdir.to_path_buf();

    let task = tokio::spawn(
        async move {
            // Shared WS connection handle; populated by the inbound
            // adapter's `on_connect` callback after subscribe.
            let handle_arc: std::sync::Arc<
                tokio::sync::Mutex<Option<wecom_bot::client::WecomBotConnectionHandle>>,
            > = std::sync::Arc::new(tokio::sync::Mutex::new(None));

            // topic → {req_id, stream_id, recipient} for the reply forwarder.
            let topic_state: std::sync::Arc<
                tokio::sync::Mutex<HashMap<String, WecomReplyState>>,
            > = std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new()));

            // Inspect client for attachment downloads (None when inspect
            // is disabled — text relaying still works, attachments are
            // dropped with a warning, same as feishu).
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

            // One reply forwarder per distinct pipe target channel.
            for channel in &pipe_channels {
                let ws_broadcasts = ws_broadcasts.clone();
                let topic_state = topic_state.clone();
                let handle_arc = handle_arc.clone();
                let channel = channel.clone();
                let inspect_client = inspect_client.clone();
                let config_for_relay = config_for_spawn.clone();
                tokio::spawn(async move {
                    let Some(broadcast_tx) = wait_for_broadcast(&ws_broadcasts, &channel).await
                    else {
                        tracing::error!(
                            channel = %channel,
                            "wecom_bot pipe: target channel broadcast never appeared (is it a websocket channel?), reply forwarder not started"
                        );
                        return;
                    };
                    let mut rx = broadcast_tx.subscribe();
                    tracing::info!(channel = %channel, "wecom_bot pipe reply forwarder subscribed");
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
                        let streamed = wecom_bot::send_stream_reply_and_wait(
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
                            if let Err(e2) = send_wecom_proactive_text(
                                &handle,
                                &state.recipient,
                                text,
                            )
                            .await
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
                            let Some(inspect) = &inspect_client else {
                                tracing::warn!(
                                    filename = %att.filename,
                                    "wecom_bot pipe: attachment dropped (inspect server disabled)"
                                );
                                continue;
                            };
                            if let Err(e) = relay_wecom_attachment(
                                &handle,
                                inspect,
                                &state.recipient,
                                &att,
                                &config_for_relay,
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

            let adapter = WecomBotInboundAdapter::with_shared_handle(
                &wecom_bot_config,
                channel_name.clone(),
                handle_arc.clone(),
            );

            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let config_for_pipe = config_for_spawn.clone();
                    let topic_state = topic_state.clone();
                    let handle_arc = handle_arc.clone();
                    let channel_name_self = channel_name.clone();
                    let routers = routers.clone();
                    tokio::spawn(async move {
                        let patterns = config_for_pipe
                            .load()
                            .channels
                            .get(&channel_name_self)
                            .and_then(|c| c.patterns.clone())
                            .unwrap_or_default();
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

                        // 4. Send the streaming "thinking" indicator
                        //    (finish=false) immediately. The streaming
                        //    window must be opened before the agent runs
                        //    because the agent can take minutes and the
                        //    WeCom passive reply window is short. No-op
                        //    when the handle is not yet set or the
                        //    original message lacks a req_id (a
                        //    configured edge case — the reply can still
                        //    be relayed without an indicator).
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

                        // 5. Record resolved topic → streaming state for
                        //    the reply forwarder (and the keep-alive).
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

                        // 5.5. Spawn keep-alive task to keep the streaming
                        //      window open during long agent runs. Sends
                        //      `finish=false` with a rotating spinner every
                        //      WECOM_KEEP_ALIVE_INTERVAL. Self-terminates
                        //      when the reply is delivered (state removed
                        //      by the forwarder) or when the safety
                        //      deadline expires. No-op when the original
                        //      message lacked a req_id (no stream was
                        //      opened, so nothing to keep alive).
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
                                    let content = format!(
                                        "{} 正在处理中... (已用 {}s)",
                                        frame, elapsed
                                    );
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

                        // 6. Route through the target channel's own
                        //    MessageRouter (identical to a chat-pane
                        //    message — topic_path/template/skills apply).
                        route_into_pipe_target("wecom_bot", &routers, pipe, message).await;
                    });
                    Ok(())
                }),
                on_topic_close: None,
                on_close_event: None,
                on_error: Box::new(|error| {
                    tracing::error!(error = %error, "WeCom Bot inbound error");
                }),
                attachment_config: inbound_attachment_config.clone(),
            };

            if let Err(e) = adapter.start(options, cancel).await {
                tracing::error!(error = %e, "WeCom Bot inbound adapter error");
            }
        }
        .instrument(channel_span),
    );
    tasks.push(task);
    Ok(())
}
