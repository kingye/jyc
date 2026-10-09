//! Channel adapter construction for `jyc serve`.
//!
//! Extracted from the monolithic `serve.rs` run() function.

use anyhow::Result;
use jyc_core::channel_orchestrator::ChannelOrchestrator;
use jyc_core::message_router::MessageRouter;
use jyc_core::topic_manager::TopicManager;
use jyc_inspect::server::websocket::inbound::{WebsocketInboundAdapter, WebsocketMatcher};
use jyc_types::{ChannelInfo, InboundAdapter, InboundAttachmentConfig};
use serde_json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

mod email;
#[cfg(test)]
mod tests;
mod wecom;

pub(crate) use email::spawn_email_adapter;
pub(crate) use wecom::{spawn_wecom_adapter, spawn_wecomkf_adapter};

// Pipe helpers shared with `jyc-pipe` live in `jyc_channels::pipe`;
// re-exported so the remaining in-process pipe adapters keep working
// until their channels migrate (docs/architecture/pipe-split.md).
pub(crate) use jyc_channels::pipe::{
    ReplyAttachmentRef, collect_pipe_target_channels, loopback_addr, match_and_retarget,
    match_pipe, parse_reply_attachments, retarget_or_drop, warn_on_bad_pipe_patterns,
};

/// Re-target a piped inbound message into the target channel/topic, applying
/// the target channel's pattern (template/role) for that topic.
///
/// The target channel's `pattern_for_topic` resolves the pattern named after
/// the topic (= the feishu chat name); its template/role are injected as
/// metadata so the target worker initializes the topic with them.
/// Wait (bounded) for a websocket channel's broadcast sender to be registered.
///
/// The target's broadcast is inserted into `ws_broadcasts` when its outbound
/// adapter is built during startup; a piped reply forwarder may be spawned
/// before that, so wait briefly. Returns `None` after a timeout (the target
/// is probably not a websocket channel) instead of looping forever.
pub(super) async fn wait_for_broadcast(
    ws_broadcasts: &std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
    target: &str,
) -> Option<broadcast::Sender<String>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Some(tx) = ws_broadcasts.lock().unwrap().get(target) {
            return Some(tx.clone());
        }
        if std::time::Instant::now() > deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Runtime placeholder resolved from message metadata (or the
/// `channel_uid` core field) when retargeting a piped message. The
/// `msg.` namespace keeps it immune to the load-time `${ENV_VAR}`
/// expansion (whose regex requires `\w+`, no dots).
///
/// Resolution: `${msg.<key>}` looks up `metadata[key]` first; if the
/// key is `channel_uid` and the metadata lookup misses, the message's
/// `channel_uid` field is used instead. This unifies group chat id
/// and single chat user id in one topic template. The key `topic`
/// likewise falls back to the message's `topic` field (the channel's
/// own derived conversation name).
///
/// If any placeholder is present but the value is missing/empty, the
/// caller drops the message with a warning (avoids misrouting to a
/// literal `"${msg.<key>}"` topic).
/// Download one reply attachment from the inspect server, apply the
/// operator's outbound policy, and stage it in a temp file.
///
/// Shared by the pipe reply forwarders (email / wecom): the
/// upload APIs take a path, the validator takes a path, SMTP takes bytes —
/// so both are returned. The temp file lives until the caller drops it.
pub(super) async fn fetch_reply_attachment(
    inspect: &jyc_inspect::client::InspectClient,
    att: &ReplyAttachmentRef,
    config: &arc_swap::ArcSwap<jyc_types::AppConfig>,
) -> Result<(Vec<u8>, tempfile::NamedTempFile)> {
    let bytes = inspect.download_topic_file(&att.url_path).await?;
    let tmp = tempfile::NamedTempFile::new()?;
    tokio::fs::write(tmp.path(), &bytes).await?;
    if let Some(cfg) = config
        .load()
        .attachments
        .as_ref()
        .and_then(|a| a.outbound.clone())
    {
        jyc_utils::attachment_validator::validate_outbound_file(tmp.path(), &att.filename, &cfg)
            .await?;
    }
    Ok((bytes, tmp))
}

/// Hub channels a pipe-only adapter can route into, keyed by channel name.
///
/// Carries the `TopicManager` alongside the router because pipe-only adapters
/// own no workspace: routing needs the router, and close events (GitHub
/// issue/PR closed) need the hub's TopicManager.
pub(crate) type HubRegistry =
    std::sync::Arc<std::sync::Mutex<HashMap<String, (Arc<MessageRouter>, Arc<TopicManager>)>>>;

/// Route a retargeted message into the pipe target channel's router.
pub(super) async fn route_into_pipe_target(
    channel_type: &str,
    routers: &HubRegistry,
    pipe: &jyc_types::PipeTarget,
    message: jyc_types::InboundMessage,
) {
    let target_channel = pipe
        .channel
        .clone()
        .or_else(|| pipe.agent.as_ref().map(|_| "agents".to_string()))
        .expect("validated upstream: agent or channel required");
    let Some(target_router) = routers
        .lock()
        .unwrap()
        .get(&target_channel)
        .map(|(r, _)| r.clone())
    else {
        tracing::warn!(
            channel_type,
            channel = %target_channel,
            "pipe: target channel router not found, dropping"
        );
        return;
    };
    target_router
        .route(&WebsocketMatcher::new(target_channel), message)
        .await;
}

/// Shared per-channel context for spawning the inbound monitor task(s).
pub(crate) struct InboundSpawner<'a> {
    pub(crate) channel_type: &'a str,
    pub(crate) channel_name: String,
    pub(crate) workspace_dir: PathBuf,
    pub(crate) inbound_attachment_config: Option<InboundAttachmentConfig>,
    pub(crate) topic_manager: Arc<TopicManager>,
    pub(crate) router: Arc<MessageRouter>,
    pub(crate) cancel: CancellationToken,
    pub(crate) cancel_child: CancellationToken,
    pub(crate) tasks: &'a mut Vec<JoinHandle<()>>,
    pub(crate) orchestrator: Arc<ChannelOrchestrator>,
    pub(crate) channel_info: ChannelInfo,
    pub(crate) websocket_handlers: &'a mut [Arc<WebsocketInboundAdapter>],
}

impl InboundSpawner<'_> {
    /// Spawn the channel-type-specific inbound monitor task(s).
    ///
    /// Destructures the context into locals so the per-channel arms read
    /// exactly like the original inline match in `serve.rs`.
    pub(crate) async fn spawn(self) -> Result<()> {
        let InboundSpawner {
            channel_type,
            channel_name,
            workspace_dir,
            inbound_attachment_config,
            topic_manager,
            router,
            cancel,
            cancel_child,
            tasks,
            orchestrator,
            channel_info,
            websocket_handlers,
        } = self;
        let channel_name_owned = channel_name.clone();
        let tm = topic_manager.clone();
        let channel_span = tracing::info_span!("in", ch = %channel_name);
        if channel_type == "websocket" {
            let router_for_callback = router.clone();
            let channel_name_for_matcher = channel_name_owned.clone();

            // The websocket handler was already created when the outbound adapter was built.
            // Find it in the list and start it (sets the on_message callback).
            let handler = websocket_handlers.last().cloned().ok_or_else(|| {
                anyhow::anyhow!("channel '{channel_name}': websocket handler not found")
            })?;

            let topic_manager_clone = topic_manager.clone();
            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let router = router_for_callback.clone();
                    let channel_name = channel_name_for_matcher.clone();

                    tokio::spawn(async move {
                        router
                            .route(&WebsocketMatcher::new(channel_name), message)
                            .await;
                    });

                    Ok(())
                }),
                on_topic_close: Some(Box::new(move |topic_name: String| {
                    let tm = topic_manager_clone.clone();
                    tokio::spawn(async move {
                        if let Err(e) = tm.auto_close_topic(&topic_name).await {
                            tracing::error!(error = %e, topic = %topic_name, "Failed to close topic");
                        }
                    });
                    Ok(())
                })),
                on_close_event: None,
                on_error: Box::new(|error| {
                    tracing::error!(error = %error, "WebSocket inbound error");
                }),
                attachment_config: inbound_attachment_config.clone(),
            };

            // Start the adapter (sets the on_message callback; no independent listener)
            if let Err(e) = handler.start(options, cancel_child.clone()).await {
                tracing::error!(
                    error = %e,
                    "WebSocket inbound adapter error"
                );
            }

            // WebSocket channel does not need a background task (handler is registered on the inspect server)
            // But we still need to keep the topic_manager alive, so we push a no-op task
            let task = tokio::spawn(
                async move {
                    // Wait for cancellation
                    cancel_child.cancelled().await;
                    tm.shutdown().await;
                }
                .instrument(channel_span),
            );

            orchestrator
                .register_channel(
                    channel_name.to_string(),
                    jyc_core::channel_orchestrator::ChannelHandle {
                        cancel: cancel.clone(),

                        topic_manager: topic_manager.clone(),

                        channel_info: channel_info.clone(),

                        workspace_dir: workspace_dir.clone(),
                    },
                )
                .await;

            tasks.push(task);
        }
        Ok(())
    }
}
