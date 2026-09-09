//! `wecom` channel adapter wiring (extracted from serve/channels.rs).

use anyhow::Result;
use jyc_channels::wecom::kf_client::KfApiClient;
use jyc_channels::wecom::kf_cursor::KfCursorStore;
use jyc_channels::wecom::kf_dedup::KfDedupStore;
use jyc_channels::wecom::server::WecomWebhookServer;
use jyc_channels::wecom::token_cache::AccessTokenCache;
use jyc_types::{ChannelConfig, InboundAdapter, InboundAttachmentConfig};
use serde_json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::*;

/// Spawn a wecom (group bot callback) pipe-only adapter.
///
/// Mirrors spawn_wecom_bot_adapter. Protocol only: webhook registration
/// via the shared WecomWebhookServer, pattern match, pipe retarget, and a
/// reply forwarder per pipe target channel. No TopicManager / agent /
/// orchestrator — the hub owns all of that.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_wecom_adapter(
    channel_config: &ChannelConfig,
    channel_name: String,
    inbound_attachment_config: Option<InboundAttachmentConfig>,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
    config_for_spawn: Arc<arc_swap::ArcSwap<jyc_types::AppConfig>>,
    ws_broadcasts: std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
    routers: HubRegistry,
    wecom_server: Option<Arc<WecomWebhookServer>>,
) -> Result<()> {
    use jyc_channels::wecom::inbound::{WecomInboundAdapter, WecomMatcher};
    use jyc_channels::wecom::outbound::WecomSender;

    let wecom_config = channel_config
        .wecom
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing wecom config"))?
        .clone();
    let server =
        wecom_server.ok_or_else(|| anyhow::anyhow!("WeCom webhook server not initialized"))?;

    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("wecom", &channel_name, channel_config);

    let channel_span = tracing::info_span!("in", ch = %channel_name);
    let task = tokio::spawn(
        async move {
            let sender = Arc::new(WecomSender::new(
                wecom_config.corp_id.clone(),
                wecom_config.corp_secret.clone(),
            ));

            // Startup connectivity check (replaces the pre-migration
            // fail-fast `connect()`): surface bad credentials immediately
            // instead of on first reply.
            {
                let sender = sender.clone();
                let ch = channel_name.clone();
                tokio::spawn(async move {
                    if let Err(e) = sender.verify_connectivity().await {
                        tracing::error!(
                            channel = %ch,
                            error = format!("{e:#}"),
                            "wecom: credential check failed, replies will fail until fixed"
                        );
                    }
                });
            }

            // Resolved topic → chat_id for the reply forwarder. In-memory
            // only: a reply can only follow an inbound message, which
            // repopulates the entry.
            let topic_chats: std::sync::Arc<tokio::sync::Mutex<HashMap<String, String>>> =
                std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new()));

            // One reply forwarder per distinct pipe target channel.
            for channel in &pipe_channels {
                let ws_broadcasts = ws_broadcasts.clone();
                let topic_chats = topic_chats.clone();
                let sender = sender.clone();
                let channel = channel.clone();
                tokio::spawn(async move {
                    let Some(broadcast_tx) = wait_for_broadcast(&ws_broadcasts, &channel).await
                    else {
                        tracing::error!(
                            channel = %channel,
                            "wecom pipe: target channel broadcast never appeared (is it a websocket channel?), reply forwarder not started"
                        );
                        return;
                    };
                    let mut rx = broadcast_tx.subscribe();
                    tracing::info!(channel = %channel, "wecom pipe reply forwarder subscribed");
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
                        let Some(chat_id) = topic_chats.lock().await.get(topic).cloned() else {
                            tracing::debug!(
                                topic = %topic,
                                "wecom pipe: no chat_id for topic, skipping reply"
                            );
                            continue;
                        };
                        if let Err(e) = sender.send(&chat_id, text).await {
                            tracing::error!(
                                error = format!("{e:#}"),
                                topic = %topic,
                                "wecom pipe: failed to relay reply"
                            );
                        }
                    }
                });
            }

            let adapter = WecomInboundAdapter::new(&wecom_config, &channel_name, server);

            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let config_for_pipe = config_for_spawn.clone();
                    let topic_chats = topic_chats.clone();
                    let channel_name_self = channel_name.clone();
                    let routers = routers.clone();
                    tokio::spawn(async move {
                        let patterns = config_for_pipe
                            .load()
                            .channels
                            .get(&channel_name_self)
                            .and_then(|c| c.patterns.clone())
                            .unwrap_or_default();
                        let chat_id = message
                            .metadata
                            .get("chat_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let Some((message, pipe)) =
                            match_and_retarget("wecom", &WecomMatcher, message, &patterns)
                        else {
                            return;
                        };
                        if let Some(chat_id) = chat_id {
                            topic_chats
                                .lock()
                                .await
                                .insert(message.topic.clone(), chat_id);
                        }
                        route_into_pipe_target("wecom", &routers, &pipe, message).await;
                    });
                    Ok(())
                }),
                on_topic_close: None,
                on_close_event: None,
                on_error: Box::new(|error| {
                    tracing::error!(error = %error, "WeCom inbound error");
                }),
                attachment_config: inbound_attachment_config.clone(),
            };

            if let Err(e) = adapter.start(options, cancel).await {
                tracing::error!(error = %e, "WeCom inbound adapter error");
            }
        }
        .instrument(channel_span),
    );
    tasks.push(task);
    Ok(())
}

/// Spawn a wecomkf (customer service) pipe-only adapter.
///
/// Mirrors spawn_wecom_adapter. Keeps the protocol state (sync cursor,
/// msgid dedup) — same precedent as email's IMAP cursor and github's
/// dedup store. Replies go out via `kf/send_msg` (text only; attachments
/// are not relayed, same as the pre-migration behavior).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_wecomkf_adapter(
    channel_config: &ChannelConfig,
    channel_name: String,
    inbound_attachment_config: Option<InboundAttachmentConfig>,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
    config_for_spawn: Arc<arc_swap::ArcSwap<jyc_types::AppConfig>>,
    ws_broadcasts: std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
    routers: HubRegistry,
    wecom_server: Option<Arc<WecomWebhookServer>>,
) -> Result<()> {
    use jyc_channels::wecom::kf_inbound::{WecomKfInboundAdapter, WecomKfMatcher};
    use jyc_channels::wecom::kf_outbound::send_kf_text;

    let kf_config = channel_config
        .wecom_kf
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing wecom_kf config"))?
        .clone();
    let server =
        wecom_server.ok_or_else(|| anyhow::anyhow!("WeCom webhook server not initialized"))?;

    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("wecomkf", &channel_name, channel_config);

    let channel_span = tracing::info_span!("in", ch = %channel_name);
    let task = tokio::spawn(
        async move {
            let token_cache = Arc::new(AccessTokenCache::new(
                kf_config.corp_id.clone(),
                kf_config.corp_secret.clone(),
            ));
            let kf_client = Arc::new(KfApiClient::new(token_cache));

            // Startup connectivity check (replaces the pre-migration
            // fail-fast `connect()`): surface bad credentials immediately
            // instead of on first reply.
            {
                let kf_client = kf_client.clone();
                let ch = channel_name.clone();
                tokio::spawn(async move {
                    if let Err(e) = kf_client.verify_connectivity().await {
                        tracing::error!(
                            channel = %ch,
                            error = format!("{e:#}"),
                            "wecomkf: credential check failed, replies will fail until fixed"
                        );
                    }
                });
            }
            let cursor_store = Arc::new(KfCursorStore::new(
                kf_config.cursor_store_path.as_ref().map(std::path::PathBuf::from),
            ));
            let dedup_store = Arc::new(KfDedupStore::new());

            // Resolved topic → (open_kfid, external_userid) for the reply
            // forwarder. In-memory only, repopulated by inbound messages.
            let topic_addrs: std::sync::Arc<
                tokio::sync::Mutex<HashMap<String, (String, String)>>,
            > = std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new()));

            // One reply forwarder per distinct pipe target channel.
            for channel in &pipe_channels {
                let ws_broadcasts = ws_broadcasts.clone();
                let topic_addrs = topic_addrs.clone();
                let kf_client = kf_client.clone();
                let channel = channel.clone();
                tokio::spawn(async move {
                    let Some(broadcast_tx) = wait_for_broadcast(&ws_broadcasts, &channel).await
                    else {
                        tracing::error!(
                            channel = %channel,
                            "wecomkf pipe: target channel broadcast never appeared (is it a websocket channel?), reply forwarder not started"
                        );
                        return;
                    };
                    let mut rx = broadcast_tx.subscribe();
                    tracing::info!(channel = %channel, "wecomkf pipe reply forwarder subscribed");
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
                        let Some((open_kfid, touser)) =
                            topic_addrs.lock().await.get(topic).cloned()
                        else {
                            tracing::debug!(
                                topic = %topic,
                                "wecomkf pipe: no address for topic, skipping reply"
                            );
                            continue;
                        };
                        if let Err(e) =
                            send_kf_text(&kf_client, &open_kfid, &touser, text).await
                        {
                            tracing::error!(
                                error = format!("{e:#}"),
                                topic = %topic,
                                "wecomkf pipe: failed to relay reply"
                            );
                        }
                    }
                });
            }

            let adapter = WecomKfInboundAdapter::new(
                &kf_config,
                &channel_name,
                server,
                kf_client,
                cursor_store,
                dedup_store,
            );

            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let config_for_pipe = config_for_spawn.clone();
                    let topic_addrs = topic_addrs.clone();
                    let channel_name_self = channel_name.clone();
                    let routers = routers.clone();
                    tokio::spawn(async move {
                        let patterns = config_for_pipe
                            .load()
                            .channels
                            .get(&channel_name_self)
                            .and_then(|c| c.patterns.clone())
                            .unwrap_or_default();
                        let addr = match (
                            message.metadata.get("open_kfid").and_then(|v| v.as_str()),
                            message
                                .metadata
                                .get("external_userid")
                                .and_then(|v| v.as_str()),
                        ) {
                            (Some(k), Some(u)) => Some((k.to_string(), u.to_string())),
                            _ => None,
                        };
                        let Some((message, pipe)) =
                            match_and_retarget("wecomkf", &WecomKfMatcher, message, &patterns)
                        else {
                            return;
                        };
                        if let Some(addr) = addr {
                            topic_addrs
                                .lock()
                                .await
                                .insert(message.topic.clone(), addr);
                        }
                        route_into_pipe_target("wecomkf", &routers, &pipe, message).await;
                    });
                    Ok(())
                }),
                on_topic_close: None,
                on_close_event: None,
                on_error: Box::new(|error| {
                    tracing::error!(error = %error, "WeCom KF inbound error");
                }),
                attachment_config: inbound_attachment_config.clone(),
            };

            if let Err(e) = adapter.start(options, cancel).await {
                tracing::error!(error = %e, "WeCom KF inbound adapter error");
            }
        }
        .instrument(channel_span),
    );
    tasks.push(task);
    Ok(())
}

/// Download one reply attachment from the inspect server, upload it to
/// the WeCom user via the shared WebSocket handle, and send the media
/// message via `aibot_send_msg` (proactive) keyed by the recipient.
///
/// Mirrors `relay_attachment` (feishu). Proactive send is used here
/// instead of `aibot_respond_msg` because the agent's reply is async
/// and the WeCom passive reply window may have closed by the time the
/// forwarder relays attachments.
pub(super) async fn relay_wecom_attachment(
    handle: &jyc_channels::wecom_bot::client::WecomBotConnectionHandle,
    inspect: &jyc_inspect::client::InspectClient,
    recipient: &str,
    att: &ReplyAttachmentRef,
    config: &arc_swap::ArcSwap<jyc_types::AppConfig>,
) -> Result<()> {
    use jyc_channels::wecom_bot::{build_media_message_body, upload_attachment, wecom_media_type};

    let (_bytes, tmp) = fetch_reply_attachment(inspect, att, config).await?;

    let media_id = upload_attachment(handle, tmp.path(), &att.filename, &att.content_type).await?;
    let media_type = wecom_media_type(&att.content_type, &att.filename);
    let mut body = build_media_message_body(media_type, &media_id);
    body["chatid"] = serde_json::Value::String(recipient.to_string());

    let req_id = jyc_channels::wecom_bot::client::generate_req_id("aibot_send_msg");
    let json = serde_json::json!({
        "cmd": "aibot_send_msg",
        "headers": {"req_id": req_id},
        "body": body,
    })
    .to_string();

    handle
        .sender
        .send(json)
        .map_err(|e| anyhow::anyhow!("wecom_bot pipe: failed to send attachment: {e}"))?;
    tracing::info!(
        filename = %att.filename,
        recipient = %recipient,
        "wecom_bot pipe: attachment relayed"
    );
    Ok(())
}

/// Send a text reply via proactive `aibot_send_msg` to the recipient.
///
/// Fallback path when the streaming `finish=true` ack is rejected
/// (typically errcode 846604 — the WeCom passive-reply window has
/// closed, common for long agent runs). The body wire format is built
/// by the shared `build_proactive_text_body` helper.
pub(super) async fn send_wecom_proactive_text(
    handle: &jyc_channels::wecom_bot::client::WecomBotConnectionHandle,
    recipient: &str,
    text: &str,
) -> Result<()> {
    let body = jyc_channels::wecom_bot::build_proactive_text_body(recipient, text);
    let req_id = jyc_channels::wecom_bot::client::generate_req_id("aibot_send_msg");
    let json = serde_json::json!({
        "cmd": "aibot_send_msg",
        "headers": {"req_id": req_id},
        "body": body,
    })
    .to_string();
    handle
        .sender
        .send(json)
        .map_err(|e| anyhow::anyhow!("wecom_bot pipe: proactive text send failed: {e}"))?;
    tracing::info!(
        recipient = %recipient,
        text_len = text.len(),
        "wecom_bot pipe: proactive text reply sent"
    );
    Ok(())
}
