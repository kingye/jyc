//! Feishu channel bridge for JYC.
//!
//! Connects to jyc over WebSocket (`/ws/<channel>`) and relays feishu events
//! per the route table in the bridge config. See `docs/plugin-architecture.md`.

mod config;
mod feishu;
mod router;
mod ws;

use anyhow::{Context, Result};
use jyc_types::InboundMessage;
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// `(jyc channel, jyc thread) -> (feishu reply target, receive_id_type)` —
/// the reply direction of the route table. `receive_id_type` is `"chat_id"`
/// for groups and `"open_id"` for p2p.
type ReverseMap = Arc<std::sync::Mutex<HashMap<(String, String), (String, String)>>>;
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
                let Some((receive_id, receive_id_type)) =
                    reverse.lock().unwrap().get(&key).cloned()
                else {
                    tracing::warn!(
                        channel = %channel,
                        thread = %reply.thread,
                        "no chat mapping for reply, dropping"
                    );
                    continue;
                };
                let send = if receive_id_type == "open_id" {
                    client
                        .send_text_message_to_user(&receive_id, &reply.text)
                        .await
                } else {
                    client.send_text_message(&receive_id, &reply.text).await
                };
                if let Err(e) = send {
                    tracing::error!(error = %e, receive_id = %receive_id, "failed to send reply to feishu");
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
            let Some(forward) = router::forward_for(cfg, &message) else {
                tracing::debug!(chat = %message.topic, "dropped event (no route or respond filter)");
                return Ok(());
            };

            // Remember where to send replies for this thread.
            reverse.lock().unwrap().insert(
                (forward.channel.clone(), forward.thread.clone()),
                (forward.receive_id.clone(), forward.receive_id_type.clone()),
            );

            let frame = ws::InboundFrame::message(
                forward.thread,
                forward.text,
                forward.sender,
                forward.sender_address,
            );
            let senders = clients.lock().unwrap();
            match senders.get(&forward.channel) {
                Some(c) => c.send_message(frame)?,
                None => tracing::warn!(
                    channel = %forward.channel,
                    "no jyc connection for channel"
                ),
            }
            Ok(())
        }
    };

    let on_thread_close = {
        let clients = clients.clone();
        let reverse = reverse.clone();
        move |chat_id: String| -> Result<()> {
            // Close every jyc thread that was mapped to this disbanded chat.
            let targets: Vec<(String, String)> = reverse
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, (cid, _))| *cid == chat_id)
                .map(|((ch, th), _)| (ch.clone(), th.clone()))
                .collect();
            let senders = clients.lock().unwrap();
            for (channel, thread) in targets {
                if let Some(c) = senders.get(&channel) {
                    let frame = ws::InboundFrame::message(thread, "/close -y", None, None);
                    if let Err(e) = c.send_message(frame) {
                        tracing::warn!(error = %e, "failed to send /close -y");
                    }
                }
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
