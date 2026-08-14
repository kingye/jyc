//! Feishu channel bridge for JYC.
//!
//! Connects to jyc over WebSocket (`/ws/<channel>`) and relays feishu events
//! per the route table in the bridge config. See `docs/plugin-architecture.md`.

mod config;
mod feishu;
mod ws;

use anyhow::{Context, Result};
use jyc_types::InboundMessage;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// `(jyc channel, jyc thread) -> feishu chat_id` — the reply direction of
/// the route table.
type ReverseMap = Arc<std::sync::Mutex<HashMap<(String, String), String>>>;
/// `jyc channel -> WS sender` used to forward inbound messages.
type ClientMap = Arc<std::sync::Mutex<HashMap<String, ws::ChannelClient>>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let cfg = config::BridgeConfig::load(&path)?;
    tracing::info!(name = %cfg.name, routes = cfg.routes.len(), "bridge config loaded");

    // jyc connection: spawned bridges get JYC_URL/JYC_TOKEN from the
    // environment; externally-managed bridges fall back to config.
    let jyc_url = std::env::var("JYC_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.jyc_url.clone())
        .context("no jyc URL: set JYC_URL or jyc_url in config")?;
    let token = std::env::var("JYC_TOKEN").unwrap_or_default();

    let feishu_cfg = jyc_types::FeishuConfig {
        app_id: cfg.app_id.clone(),
        app_secret: cfg.app_secret.clone(),
        base_url: cfg.base_url.clone(),
        websocket: Default::default(),
        events: Default::default(),
        message_format: "markdown".to_string(),
        metadata: Default::default(),
    };
    let feishu_client = Arc::new(feishu::client::FeishuClient::new(feishu_cfg.clone()));

    // Connect to each distinct jyc channel (route table + default).
    let mut senders: HashMap<String, ws::ChannelClient> = HashMap::new();
    let mut replies: Vec<(String, ws::ChannelReplies)> = Vec::new();
    for channel in cfg.channels() {
        match ws::connect(&jyc_url, &token, &channel).await {
            Ok((client, rx)) => {
                senders.insert(channel.clone(), client);
                replies.push((channel, rx));
            }
            Err(e) => tracing::error!(channel = %channel, error = %e, "failed to connect to jyc"),
        }
    }
    if senders.is_empty() {
        anyhow::bail!("no jyc channels connected");
    }

    let reverse: ReverseMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let clients: ClientMap = Arc::new(std::sync::Mutex::new(senders));

    // Reply tasks: jyc reply -> feishu (one task per channel connection).
    for (channel, mut reply_rx) in replies {
        let reverse = reverse.clone();
        let client = feishu_client.clone();
        tokio::spawn(async move {
            while let Some(reply) = reply_rx.recv_reply().await {
                let key = (channel.clone(), reply.thread.clone());
                let Some(chat_id) = reverse.lock().unwrap().get(&key).cloned() else {
                    tracing::warn!(
                        channel = %channel,
                        thread = %reply.thread,
                        "no chat mapping for reply, dropping"
                    );
                    continue;
                };
                if let Err(e) = client.send_text_message(&chat_id, &reply.text).await {
                    tracing::error!(error = %e, chat_id = %chat_id, "failed to send reply to feishu");
                }
            }
            tracing::info!(channel = %channel, "reply task ended");
        });
    }

    // Feishu event loop (with reconnection, same driver as the compiled-in
    // adapter). `on_message` translates the feishu event into a jyc frame.
    let cancel = CancellationToken::new();
    let mut feishu_ws = feishu::websocket::FeishuWebSocket::new_with_attachments(
        &feishu_cfg,
        feishu_client.clone(),
        None,
    );

    let on_message = {
        let cfg = &cfg;
        let clients = clients.clone();
        let reverse = reverse.clone();
        move |message: InboundMessage| -> Result<()> {
            let chat_name = message
                .metadata
                .get("chat_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let chat_id = message
                .metadata
                .get("chat_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let Some(route) = cfg.route(&chat_name) else {
                tracing::debug!(chat_name = %chat_name, "no route for chat, dropping event");
                return Ok(());
            };

            // Respond filter: when the route declares mentions, forward only
            // if one of them is @-mentioned (matches id or display name).
            if let Some(required) = &route.mentions {
                let mentioned: Vec<String> = message
                    .metadata
                    .get("mentions")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| {
                                m.as_str()
                                    .or_else(|| m.get("name").and_then(|n| n.as_str()))
                                    .or_else(|| m.get("id").and_then(|i| i.as_str()))
                            })
                            .map(|s| s.to_lowercase())
                            .collect()
                    })
                    .unwrap_or_default();
                if !required
                    .iter()
                    .any(|r| mentioned.contains(&r.to_lowercase()))
                {
                    tracing::debug!(chat_name = %chat_name, "not @-mentioned, dropping event");
                    return Ok(());
                }
            }

            let channel = route.channel.as_deref().unwrap_or(&cfg.channel);
            let thread = &route.thread;

            // Remember where to send replies for this thread.
            reverse
                .lock()
                .unwrap()
                .insert((channel.to_string(), thread.to_string()), chat_id);

            let frame = ws::InboundFrame::message(
                thread.clone(),
                message.content.text.unwrap_or_default(),
                Some(message.sender),
                Some(message.sender_address),
            );
            let senders = clients.lock().unwrap();
            match senders.get(channel) {
                Some(c) => c.send_message(frame)?,
                None => tracing::warn!(channel = %channel, "no jyc connection for channel"),
            }
            Ok(())
        }
    };

    let on_thread_close = {
        let cfg = &cfg;
        let clients = clients.clone();
        move |derived_thread: String| -> Result<()> {
            let Some((channel, thread)) = close_route_for(cfg, &derived_thread) else {
                tracing::warn!(
                    thread = %derived_thread,
                    "no route for disbanded chat, skipping close"
                );
                return Ok(());
            };
            let senders = clients.lock().unwrap();
            if let Some(c) = senders.get(&channel) {
                let frame = ws::InboundFrame::message(thread, "/close -y", None, None);
                c.send_message(frame)?;
            }
            Ok(())
        }
    };

    loop {
        tracing::info!("connecting to feishu websocket");
        match feishu_ws
            .run(
                "feishu-bridge",
                &on_message,
                Some(&on_thread_close),
                &cancel,
            )
            .await
        {
            Ok(()) => {
                tracing::info!("feishu websocket stopped cleanly");
                break;
            }
            Err(e) => {
                tracing::error!(error = %e, "feishu websocket error");
                if !feishu_ws.handle_reconnection().await {
                    tracing::error!("max reconnection attempts reached, giving up");
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Resolve a disbanded chat to `(channel, thread)` by matching the
/// websocket-derived thread name against the sanitized route chat names.
fn close_route_for(cfg: &config::BridgeConfig, derived_thread: &str) -> Option<(String, String)> {
    cfg.routes.iter().find_map(|r| {
        (jyc_utils::helpers::sanitize_for_filesystem(&r.chat_name) == derived_thread).then(|| {
            (
                r.channel.clone().unwrap_or_else(|| cfg.channel.clone()),
                r.thread.clone(),
            )
        })
    })
}
