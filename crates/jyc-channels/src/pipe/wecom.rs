//! `wecom` / `wecomkf` pipe wiring for `jyc-pipe`.
//!
//! Hosts both WeCom channels of one pipe process: the group-bot callback
//! adapter and the customer-service (微信客服) adapter. They share one
//! [`WecomWebhookServer`] — a single axum listener on
//! `[wecom].bind_addr` with one `/webhook/{channel_name}` route per
//! channel — which is why they migrate together: either alone would
//! collide with the hub on the bind port.
//!
//! Owning no topics/agents, each adapter only matches patterns, re-targets
//! to a pipe target (hub) channel, and relays replies back through the
//! channel's own send API (`WecomSender` / `kf/send_msg`, text only).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use jyc_types::{ChannelConfig, InboundAdapter};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::wecom::kf_client::KfApiClient;
use crate::wecom::kf_cursor::KfCursorStore;
use crate::wecom::kf_dedup::KfDedupStore;
use crate::wecom::server::WecomWebhookServer;
use crate::wecom::token_cache::AccessTokenCache;

use super::{HubPipe, collect_pipe_target_channels, match_and_retarget, warn_on_bad_pipe_patterns};

/// Start the shared WeCom webhook HTTP server and return its handle for
/// the adapters to register their `/webhook/{channel_name}` routes on.
///
/// The bind port belongs to the pipe process now: `jyc serve` no longer
/// starts this server, so a failure here means every WeCom callback would
/// 404 — fail loudly instead (same startup probe the hub used: oneshot +
/// 5s timeout, since the server can also be slow rather than broken).
pub(crate) async fn start_webhook_server(
    config: &jyc_types::AppConfig,
    cancel: CancellationToken,
) -> Result<Arc<WecomWebhookServer>> {
    let bind_addr = config
        .wecom
        .as_ref()
        .map(|w| w.bind_addr.clone())
        .unwrap_or_else(|| "127.0.0.1:10001".to_string());
    let server = Arc::new(WecomWebhookServer::new(&bind_addr));

    let (startup_tx, startup_rx) = tokio::sync::oneshot::channel::<Result<()>>();
    let server_for_task = server.clone();
    tokio::spawn(async move {
        let result = server_for_task.start(cancel).await;
        if let Err(ref e) = result {
            tracing::error!(error = %e, "WeCom webhook server failed to start");
        }
        let _ = startup_tx.send(result);
    });

    match tokio::time::timeout(std::time::Duration::from_secs(5), startup_rx).await {
        Ok(Ok(Ok(()))) => {
            tracing::info!(bind_addr = %bind_addr, "WeCom webhook server started");
        }
        Ok(Ok(Err(e))) => {
            anyhow::bail!("WeCom webhook server failed to start: {e}");
        }
        Ok(Err(_)) => {
            anyhow::bail!("WeCom webhook server task panicked during startup");
        }
        Err(_) => {
            // Timeout: the bind has not reported yet. Assume "slow, not
            // broken" and continue — if it does fail later, the spawned task
            // logs it, but the adapters stay up without a listener (same
            // trade-off the hub made before this moved).
            tracing::info!(
                bind_addr = %bind_addr,
                "WeCom webhook server startup pending (may be slow to bind)"
            );
        }
    }
    Ok(server)
}

/// Spawn the wecom (group bot callback) pipe wiring: the inbound adapter
/// plus one reply forwarder per distinct pipe target channel.
///
/// Mirrors `pipe::wecom_bot::spawn_wecom_bot_pipe`, keeping the pipe-only
/// split's protocol boundaries: webhook registration on the shared
/// server, pattern match, re-target, and text replies via `WecomSender`.
/// No TopicManager / agent / orchestrator — the hub owns all of that.
pub(crate) fn spawn_wecom_pipe(
    channel_config: &ChannelConfig,
    channel_name: String,
    config: Arc<jyc_types::AppConfig>,
    hubs: Arc<HashMap<String, Arc<HubPipe>>>,
    server: Arc<WecomWebhookServer>,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Result<()> {
    use crate::wecom::inbound::{WecomInboundAdapter, WecomMatcher};
    use crate::wecom::outbound::WecomSender;

    let wecom_config = channel_config
        .wecom
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing wecom config"))?
        .clone();
    let inbound_attachment_config = config.attachments.as_ref().and_then(|a| a.inbound.clone());

    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("wecom", &channel_name, channel_config);

    // Pattern snapshot taken at startup (patterns may not change between
    // the hub reading them and this pipe claiming the channel).
    let patterns = channel_config.patterns.clone().unwrap_or_default();

    let channel_span = tracing::info_span!("in", ch = %channel_name);

    tasks.push(tokio::spawn(
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
            let topic_chats: Arc<tokio::sync::Mutex<HashMap<String, String>>> =
                Arc::new(tokio::sync::Mutex::new(HashMap::new()));

            // One reply forwarder per distinct pipe target channel.
            for target in &pipe_channels {
                let Some(hub) = hubs.get(target).cloned() else {
                    continue;
                };
                let topic_chats = topic_chats.clone();
                let sender = sender.clone();
                let target = target.clone();
                tokio::spawn(async move {
                    let mut rx = hub.subscribe_replies();
                    tracing::info!(channel = %target, "wecom pipe reply forwarder subscribed");
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
                    let hubs = hubs.clone();
                    let patterns = patterns.clone();
                    let topic_chats = topic_chats.clone();
                    tokio::spawn(async move {
                        let chat_id = message
                            .metadata
                            .get("chat_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let Some((message, _pipe)) =
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
                        let Some(hub) = hubs.get(&message.channel) else {
                            tracing::warn!(
                                channel = %message.channel,
                                "wecom pipe: no hub pipe for target channel, dropping"
                            );
                            return;
                        };
                        hub.send_message(
                            &message.topic,
                            message.content.text.as_deref().unwrap_or(""),
                            &message.sender,
                            &message.sender_address,
                            message.metadata,
                        );
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
    ));
    Ok(())
}

/// Spawn the wecomkf (customer service) pipe wiring.
///
/// Mirrors `spawn_wecom_pipe`. Keeps the protocol state (sync cursor,
/// msgid dedup) — same precedent as email's IMAP cursor and github's
/// dedup store. Replies go out via `kf/send_msg` (text only; attachments
/// are not relayed, same as the pre-migration behavior).
pub(crate) fn spawn_wecomkf_pipe(
    channel_config: &ChannelConfig,
    channel_name: String,
    config: Arc<jyc_types::AppConfig>,
    hubs: Arc<HashMap<String, Arc<HubPipe>>>,
    server: Arc<WecomWebhookServer>,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Result<()> {
    use crate::wecom::kf_inbound::{WecomKfInboundAdapter, WecomKfMatcher};
    use crate::wecom::kf_outbound::send_kf_text;

    let kf_config = channel_config
        .wecom_kf
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing wecom_kf config"))?
        .clone();
    let inbound_attachment_config = config.attachments.as_ref().and_then(|a| a.inbound.clone());

    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("wecomkf", &channel_name, channel_config);

    // Pattern snapshot taken at startup, like the other pipe channels.
    let patterns = channel_config.patterns.clone().unwrap_or_default();

    let channel_span = tracing::info_span!("in", ch = %channel_name);

    tasks.push(tokio::spawn(
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
                kf_config
                    .cursor_store_path
                    .as_ref()
                    .map(std::path::PathBuf::from),
            ));
            let dedup_store = Arc::new(KfDedupStore::new());

            // Resolved topic → (open_kfid, external_userid) for the reply
            // forwarder. In-memory only, repopulated by inbound messages.
            let topic_addrs: Arc<tokio::sync::Mutex<HashMap<String, (String, String)>>> =
                Arc::new(tokio::sync::Mutex::new(HashMap::new()));

            // One reply forwarder per distinct pipe target channel.
            for target in &pipe_channels {
                let Some(hub) = hubs.get(target).cloned() else {
                    continue;
                };
                let topic_addrs = topic_addrs.clone();
                let kf_client = kf_client.clone();
                let target = target.clone();
                tokio::spawn(async move {
                    let mut rx = hub.subscribe_replies();
                    tracing::info!(channel = %target, "wecomkf pipe reply forwarder subscribed");
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
                        if let Err(e) = send_kf_text(&kf_client, &open_kfid, &touser, text).await {
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
                    let hubs = hubs.clone();
                    let patterns = patterns.clone();
                    let topic_addrs = topic_addrs.clone();
                    tokio::spawn(async move {
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
                        let Some((message, _pipe)) =
                            match_and_retarget("wecomkf", &WecomKfMatcher, message, &patterns)
                        else {
                            return;
                        };
                        if let Some(addr) = addr {
                            topic_addrs.lock().await.insert(message.topic.clone(), addr);
                        }
                        let Some(hub) = hubs.get(&message.channel) else {
                            tracing::warn!(
                                channel = %message.channel,
                                "wecomkf pipe: no hub pipe for target channel, dropping"
                            );
                            return;
                        };
                        hub.send_message(
                            &message.topic,
                            message.content.text.as_deref().unwrap_or(""),
                            &message.sender,
                            &message.sender_address,
                            message.metadata,
                        );
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
    ));
    Ok(())
}
