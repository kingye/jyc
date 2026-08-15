//! Channel adapter construction for `jyc serve`.
//!
//! Extracted from the monolithic `serve.rs` run() function.

use serde_json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use anyhow::Result;

use jyc_channels::email::inbound::EmailMatcher;
use jyc_channels::email::outbound::EmailOutboundAdapter;
use jyc_channels::feishu::client::FeishuClient;
use jyc_channels::feishu::inbound::{FeishuInboundAdapter, FeishuMatcher};
use jyc_channels::feishu::outbound::FeishuOutboundAdapter;
use jyc_channels::gitee::inbound::GiteeMatcher;
use jyc_channels::gitee::outbound::GiteeOutboundAdapter;
use jyc_channels::github::inbound::GithubMatcher;
use jyc_channels::github::outbound::GithubOutboundAdapter;
use jyc_channels::websocket::inbound::{WebsocketInboundAdapter, WebsocketMatcher};
use jyc_channels::websocket::outbound::WebsocketOutboundAdapter;

use jyc_channels::wechat::inbound::WechatInboundAdapter;
use jyc_channels::wechat::outbound::WechatOutboundAdapter;
use jyc_channels::wecom::inbound::WecomInboundAdapter;
use jyc_channels::wecom::kf_client::KfApiClient;
use jyc_channels::wecom::kf_cursor::KfCursorStore;
use jyc_channels::wecom::kf_dedup::KfDedupStore;
use jyc_channels::wecom::kf_inbound::{WecomKfInboundAdapter, WecomKfMatcher};
use jyc_channels::wecom::kf_outbound::WecomKfOutboundAdapter;
use jyc_channels::wecom::outbound::WecomOutboundAdapter;
use jyc_channels::wecom::server::WecomWebhookServer;
use jyc_channels::wecom::token_cache::AccessTokenCache;
use jyc_channels::wecom_bot::client::WecomBotConnectionHandle;
use jyc_channels::wecom_bot::inbound::{WecomBotInboundAdapter, WecomBotMatcher};
use jyc_channels::wecom_bot::outbound::WecomBotOutboundAdapter;
use jyc_core::channel_orchestrator::ChannelOrchestrator;
use jyc_core::message_router::MessageRouter;
use jyc_core::message_storage::MessageStorage;
use jyc_core::state_manager::StateManager;
use jyc_core::topic_manager::TopicManager;
use jyc_services::imap::monitor::ImapMonitor;
use jyc_types::{
    ChannelConfig, ChannelInfo, ChannelMatcher, InboundAdapter, InboundAttachmentConfig,
    MonitorConfig, OutboundAdapter, OutboundAttachmentConfig,
};

/// Build the outbound adapter for a channel.
///
/// Returns `Ok(None)` for unsupported channel types (the caller skips them,
/// matching the original inline `continue` behavior).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_outbound_adapter(
    channel_type: &str,
    channel_config: &ChannelConfig,
    channel_name: &str,
    storage: Arc<MessageStorage>,
    outbound_attachment_config: Option<OutboundAttachmentConfig>,
    footer_enabled: bool,
    workspace_dir: &std::path::Path,
    inspect_broadcast: Arc<broadcast::Sender<String>>,
    wechat_sender_arc: &mut Option<Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>>,
    wecom_bot_handle_arc: &mut Option<Arc<Mutex<Option<WecomBotConnectionHandle>>>>,
    wecomkf_kf_client: &mut Option<Arc<KfApiClient>>,
    ws_handler_for_channel: &mut HashMap<String, Arc<WebsocketInboundAdapter>>,
    websocket_handlers: &mut Vec<Arc<WebsocketInboundAdapter>>,
    ws_broadcasts: std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
) -> Result<Option<Arc<dyn OutboundAdapter>>> {
    let outbound: Arc<dyn OutboundAdapter> = match channel_type {
        "email" => {
            let outbound_config = channel_config.outbound.as_ref().ok_or_else(|| {
                anyhow::anyhow!("channel '{channel_name}': missing outbound config")
            })?;
            Arc::new(EmailOutboundAdapter::new_with_attachments(
                outbound_config,
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            ))
        }
        "feishu" => {
            let feishu_config = channel_config
                .feishu
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing feishu config"))?
                .clone();
            Arc::new(FeishuOutboundAdapter::new_with_attachments(
                feishu_config,
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            ))
        }
        "gitee" => {
            let gitee_config = channel_config
                .gitee
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing gitee config"))?
                .clone();
            Arc::new(GiteeOutboundAdapter::with_footer_enabled(
                gitee_config,
                storage.clone(),
                footer_enabled,
            )?)
        }
        "github" => {
            let github_config = channel_config
                .github
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing github config"))?
                .clone();
            Arc::new(GithubOutboundAdapter::with_footer_enabled(
                github_config,
                storage.clone(),
                footer_enabled,
            )?)
        }
        "wechat" => {
            // WeChat config is validated and cloned in the inbound section.
            // Outbound only needs sender, storage, and footer config.
            let adapter = WechatOutboundAdapter::new_with_attachments(
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            );
            // Store the sender_arc for later use in the inbound section
            *wechat_sender_arc = Some(adapter.sender_arc());
            Arc::new(adapter)
        }
        "wecom_bot" => {
            let adapter = WecomBotOutboundAdapter::new_with_attachments(
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            );
            *wecom_bot_handle_arc = Some(adapter.handle_arc());
            Arc::new(adapter)
        }
        "wecom" => {
            let wecom_config = channel_config
                .wecom
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing wecom config"))?
                .clone();
            Arc::new(WecomOutboundAdapter::new_with_attachments(
                wecom_config.corp_id,
                wecom_config.corp_secret,
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            ))
        }
        "wecomkf" => {
            let wecomkf_config = channel_config
                .wecom_kf
                .as_ref()
                .ok_or_else(|| {
                    anyhow::anyhow!("channel '{channel_name}': missing wecom_kf config")
                })?
                .clone();

            let access_token_cache = Arc::new(AccessTokenCache::new(
                wecomkf_config.corp_id.clone(),
                wecomkf_config.corp_secret.clone(),
            ));
            let kf_client = Arc::new(KfApiClient::new(access_token_cache));
            *wecomkf_kf_client = Some(kf_client.clone());

            Arc::new(WecomKfOutboundAdapter::new(
                kf_client,
                storage.clone(),
                outbound_attachment_config,
                footer_enabled,
            ))
        }
        "websocket" => {
            let (broadcast_tx, _) = tokio::sync::broadcast::channel(64);
            let adapter = WebsocketOutboundAdapter::new(broadcast_tx.clone(), storage.clone());
            // Store the inbound adapter for later registration with the inspect server
            let mut handler =
                WebsocketInboundAdapter::new(channel_name.to_string(), broadcast_tx.clone());
            handler.set_workspace_dir(workspace_dir.to_path_buf());
            handler.set_inspect_broadcast(inspect_broadcast.clone());
            let handler = Arc::new(handler);
            ws_handler_for_channel.insert(channel_name.to_string(), handler.clone());
            websocket_handlers.push(handler);
            // Expose the broadcast so piped channels can subscribe to replies.
            ws_broadcasts
                .lock()
                .unwrap()
                .insert(channel_name.to_string(), broadcast_tx);
            Arc::new(adapter)
        }
        other => {
            tracing::warn!(
                channel = %channel_name,
                channel_type = %other,
                "Unsupported channel type, skipping"
            );
            return Ok(None);
        }
    };
    Ok(Some(outbound))
}
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
async fn wait_for_broadcast(
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

/// Loopback address for in-process calls to the inspect server: a wildcard
/// bind (`0.0.0.0`, `[::]`) is not a connectable destination.
fn loopback_addr(bind: &str) -> String {
    bind.replace("0.0.0.0", "127.0.0.1")
        .replace("[::]", "127.0.0.1")
}

/// Re-target a piped inbound message into the target channel/topic.
///
/// Attachments and metadata ride along untouched — only the routing
/// identity (channel + topic) is rewritten.
fn apply_pipe_retarget(
    mut msg: jyc_types::InboundMessage,
    pipe: &jyc_types::PipeTarget,
) -> jyc_types::InboundMessage {
    msg.channel = pipe.channel.clone();
    msg.topic = pipe.topic.clone();
    msg
}

/// One attachment entry parsed from a websocket `reply` broadcast payload.
#[derive(Debug, PartialEq, Eq)]
struct ReplyAttachmentRef {
    filename: String,
    url_path: String,
    content_type: String,
}

/// Parse the optional `attachments` array of a reply broadcast
/// (`{"type":"reply","attachments":[{"filename","path","content_type"}]}`).
/// Malformed entries are skipped.
fn parse_reply_attachments(v: &serde_json::Value) -> Vec<ReplyAttachmentRef> {
    let Some(arr) = v.get("attachments").and_then(|a| a.as_array()) else {
        return vec![];
    };
    arr.iter()
        .filter_map(|e| {
            Some(ReplyAttachmentRef {
                filename: e.get("filename")?.as_str()?.to_string(),
                url_path: e.get("path")?.as_str()?.to_string(),
                content_type: e
                    .get("content_type")
                    .and_then(|c| c.as_str())
                    .unwrap_or("application/octet-stream")
                    .to_string(),
            })
        })
        .collect()
}

/// Download one reply attachment from the inspect server and send it to the
/// feishu chat (image vs. file chosen by content type, same primitives as
/// `FeishuOutboundAdapter::send_attachments`).
async fn relay_attachment(
    inspect: &jyc_inspect::client::InspectClient,
    client: &FeishuClient,
    chat_id: &str,
    att: &ReplyAttachmentRef,
    config: &arc_swap::ArcSwap<jyc_types::AppConfig>,
) -> Result<()> {
    use jyc_channels::feishu::client::{feishu_file_type, is_image_content_type};

    let bytes = inspect.download_topic_file(&att.url_path).await?;

    // The feishu upload APIs take a path — stage the bytes in a temp file.
    // The operator's outbound policy (extension allowlist / size cap) applies
    // to files delivered to feishu users, same as the channel's own send path.
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

    if is_image_content_type(&att.content_type) {
        let key = client.upload_image(tmp.path(), &att.filename).await?;
        client.send_image_message(chat_id, &key).await?;
    } else {
        let ext = Path::new(&att.filename)
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

/// Shared per-channel context for spawning the inbound monitor task(s).
///
/// `state_manager` is consumed (moved into the IMAP monitor closure).
pub(crate) struct InboundSpawner<'a> {
    pub(crate) channel_type: &'a str,
    pub(crate) channel_config: &'a ChannelConfig,
    pub(crate) channel_name: String,
    pub(crate) workdir: &'a Path,
    pub(crate) workspace_dir: PathBuf,
    pub(crate) args: &'a crate::cli::serve::ServeArgs,
    pub(crate) inbound_attachment_config: Option<InboundAttachmentConfig>,
    pub(crate) topic_manager: Arc<TopicManager>,
    pub(crate) router: Arc<MessageRouter>,
    pub(crate) state_manager: StateManager,
    pub(crate) cancel: CancellationToken,
    pub(crate) cancel_child: CancellationToken,
    pub(crate) tasks: &'a mut Vec<JoinHandle<()>>,
    pub(crate) wechat_sender_arc: &'a mut Option<Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>>,
    pub(crate) wecom_bot_handle_arc: &'a mut Option<Arc<Mutex<Option<WecomBotConnectionHandle>>>>,
    pub(crate) wecomkf_kf_client: &'a mut Option<Arc<KfApiClient>>,
    pub(crate) orchestrator: Arc<ChannelOrchestrator>,
    pub(crate) channel_info: ChannelInfo,
    pub(crate) config_for_spawn: Arc<arc_swap::ArcSwap<jyc_types::AppConfig>>,
    pub(crate) wecom_server: Option<Arc<WecomWebhookServer>>,
    pub(crate) websocket_handlers: &'a mut [Arc<WebsocketInboundAdapter>],
    /// Per-channel websocket broadcast senders (for `pipe` reply relaying).
    pub(crate) ws_broadcasts:
        std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
    /// Per-channel MessageRouters (for `pipe` re-targeting).
    pub(crate) routers: std::sync::Arc<std::sync::Mutex<HashMap<String, Arc<MessageRouter>>>>,
}

impl InboundSpawner<'_> {
    /// Spawn the channel-type-specific inbound monitor task(s).
    ///
    /// Destructures the context into locals so the per-channel arms read
    /// exactly like the original inline match in `serve.rs`.
    pub(crate) async fn spawn(self) -> Result<()> {
        let InboundSpawner {
            channel_type,
            channel_config,
            channel_name,
            workdir,
            workspace_dir,
            args,
            inbound_attachment_config,
            topic_manager,
            router,
            state_manager,
            cancel,
            cancel_child,
            tasks,
            wechat_sender_arc,
            wecom_bot_handle_arc,
            wecomkf_kf_client,
            orchestrator,
            channel_info,
            config_for_spawn,
            wecom_server,
            websocket_handlers,
            ws_broadcasts,
            routers,
        } = self;
        let channel_name_owned = channel_name.clone();
        let tm = topic_manager.clone();
        let channel_span = tracing::info_span!("in", ch = %channel_name);
        match channel_type {
            "email" => {
                let inbound_config = channel_config
                    .inbound
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing inbound config")
                    })?
                    .clone();

                let monitor_config = channel_config.monitor.clone().unwrap_or_default();

                // Override IDLE mode if --no-idle flag
                let monitor_config = if args.no_idle {
                    MonitorConfig {
                        mode: "poll".to_string(),
                        ..monitor_config
                    }
                } else {
                    monitor_config
                };

                let task = tokio::spawn(
                    async move {
                        let mut monitor = ImapMonitor::new(
                            channel_name_owned.clone(),
                            inbound_config,
                            monitor_config,
                            router,
                            state_manager,
                            cancel_child,
                            Arc::new(EmailMatcher),
                        );

                        if let Err(e) = monitor.start().await {
                            tracing::error!(
                                error = %e,
                                "IMAP monitor error"
                            );
                        }

                        // Shutdown topic manager for this channel
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
            "feishu" => {
                let feishu_config = channel_config
                    .feishu
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing feishu config")
                    })?
                    .clone();
                // `pipe` targets: patterns can forward matching messages into
                // another (websocket) channel instead of this channel's own
                // TopicManager. Collect the distinct target channels for
                // reply relaying.
                let pipe_channels: std::collections::HashSet<String> = channel_config
                    .patterns
                    .iter()
                    .flatten()
                    .filter(|p| p.enabled)
                    .filter_map(|p| p.pipe.as_ref().map(|pipe| pipe.channel.clone()))
                    .collect();

                let router_for_callback = router.clone();
                let topic_manager_for_task = topic_manager.clone();
                let routers_for_pipe = routers.clone();
                let config_for_pipe = config_for_spawn.clone();
                let channel_name_for_pipe = channel_name.clone();
                // Owned copy for the task: `workdir` borrows from `self`.
                let workdir_for_task = workdir.to_path_buf();

                let task = tokio::spawn(async move {
                // Clone configs before moving into closures
                let feishu_config_cloned = feishu_config.clone();

                let adapter = FeishuInboundAdapter::new(&feishu_config_cloned, channel_name_owned.clone());

                let topic_manager_clone = topic_manager_for_task.clone();

                // Shared feishu client + topic->chat_id map for pipe relaying.
                let feishu_client = std::sync::Arc::new(FeishuClient::new(feishu_config_cloned.clone()));
                let topic_chat: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>> =
                    std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

                // One reply forwarder per distinct pipe target channel:
                // subscribe to the target channel's broadcast and relay
                // replies back to feishu.
                //
                // Attachment relay needs the inspect server (reply broadcasts
                // carry download paths served by its files endpoint). Built
                // once here; `None` when inspect is disabled — text relaying
                // is unaffected, attachments are dropped with a warning.
                let inspect_client = {
                    let cfg = config_for_pipe.load();
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
                    let feishu_client = feishu_client.clone();
                    let channel = channel.clone();
                    let inspect_client = inspect_client.clone();
                    let config_for_relay = config_for_pipe.clone();
                    tokio::spawn(async move {
                        let Some(broadcast_tx) = wait_for_broadcast(&ws_broadcasts, &channel).await else {
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
                            if let Err(e) = feishu_client.send_text_message(&chat_id, text).await {
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
                                if let Err(e) = relay_attachment(
                                    inspect,
                                    &feishu_client,
                                    &chat_id,
                                    &att,
                                    &config_for_relay,
                                )
                                .await
                                {
                                    tracing::warn!(
                                        filename = %att.filename,
                                        error = %e,
                                        "feishu pipe: failed to relay attachment"
                                    );
                                }
                            }
                        }
                    });
                }

                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let config_for_pipe = config_for_pipe.clone();
                        let topic_chat = topic_chat.clone();
                        let channel_name_self = channel_name_for_pipe.clone();
                        let router = router_for_callback.clone();
                        let routers = routers_for_pipe.clone();
                        tokio::spawn(async move {
                            // 1. Match this channel's patterns (rules).
                            let patterns = config_for_pipe
                                .load()
                                .channels
                                .get(&channel_name_self)
                                .and_then(|c| c.patterns.clone())
                                .unwrap_or_default();
                            let Some(pm) = FeishuMatcher.match_message(&message, &patterns) else {
                                tracing::debug!(chat = %message.topic, "feishu: no pattern matched, dropping");
                                return;
                            };
                            // 2. Per-pattern `pipe`: the matched pattern decides.
                            let matched = patterns.iter().find(|p| p.name == pm.pattern_name);
                            let Some(pipe) = matched.and_then(|p| p.pipe.as_ref()) else {
                                // No pipe on the matched pattern: route normally
                                // through this channel's own TopicManager.
                                router.route(&FeishuMatcher, message).await;
                                return;
                            };
                            // 3. Record topic -> chat_id for reply relay.
                            if let Some(chat_id) = message.metadata.get("chat_id").and_then(|v| v.as_str()) {
                                topic_chat.lock().unwrap().insert(pipe.topic.clone(), chat_id.to_string());
                            }
                            // 4. Re-target into the target channel/topic and
                            //    route through the target's own MessageRouter —
                            //    the exact same path as a chat-pane message, so
                            //    topic_path/template/skills apply identically.
                            let Some(target_router) = routers.lock().unwrap().get(&pipe.channel).cloned() else {
                                tracing::warn!(channel = %pipe.channel, "feishu pipe: target channel router not found, dropping");
                                return;
                            };
                            target_router
                                .route(
                                    &WebsocketMatcher::new(pipe.channel.clone()),
                                    apply_pipe_retarget(message, pipe),
                                )
                                .await;
                        });
                        Ok(())
                    }),
                    on_topic_close: Some(Box::new(move |topic_name: String| {
                        // Always close in this channel's own TopicManager:
                        // correct for non-pipe topics, a safe no-op for piped
                        // topics (they live in the target channel, whose
                        // topics are not auto-closed — the explicit pipe
                        // mapping can't be reversed from the derived name).
                        let tm = topic_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_topic(&topic_name).await {
                                tracing::error!(error = %e, topic = %topic_name, "Failed to close topic");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "Feishu inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(
                        error = %e,
                        "Feishu inbound adapter error"
                    );
                }

                // Shutdown topic manager for this channel
                tm.shutdown().await;
            }.instrument(channel_span));

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
            "gitee" => {
                let gitee_config = channel_config
                    .gitee
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing gitee config")
                    })?
                    .clone();

                let router_for_callback = router.clone();
                let workdir_owned = workdir.to_path_buf();

                let topic_manager_for_task = topic_manager.clone();

                let task = tokio::spawn(async move {
                use jyc_channels::gitee::inbound::GiteeInboundAdapter;

                let adapter = GiteeInboundAdapter::new(&gitee_config, channel_name_owned.clone(), &workdir_owned);

                let topic_manager_clone = topic_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();

                        tokio::spawn(async move {
                            router.route(&GiteeMatcher, message).await;
                        });

                        Ok(())
                    }),
                    on_topic_close: Some(Box::new(move |topic_name: String| {
                        let tm = topic_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_topic(&topic_name).await {
                                tracing::error!(error = %e, topic = %topic_name, "Failed to close topic");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "Gitee inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(error = %e, "Gitee inbound adapter error");
                }

                tm.shutdown().await;
            }.instrument(channel_span));

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
            "github" => {
                let github_config = channel_config
                    .github
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing github config")
                    })?
                    .clone();

                let config_for_adapter = config_for_spawn.clone();
                let router_for_callback = router.clone();
                let workdir_owned = workdir.to_path_buf();

                let topic_manager_for_task = topic_manager.clone();

                let task = tokio::spawn(async move {
                use jyc_channels::github::inbound::GithubInboundAdapter;

                let adapter = GithubInboundAdapter::new(&github_config, channel_name_owned.clone(), &workdir_owned, Some(config_for_adapter));

                let topic_manager_clone = topic_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();

                        tokio::spawn(async move {
                            router.route(&GithubMatcher, message).await;
                        });

                        Ok(())
                    }),
                    on_topic_close: Some(Box::new(move |topic_name: String| {
                        let tm = topic_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_topic(&topic_name).await {
                                tracing::error!(error = %e, topic = %topic_name, "Failed to close topic");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "GitHub inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(
                        error = %e,
                        "GitHub inbound adapter error"
                    );
                }

                // Shutdown topic manager for this channel
                tm.shutdown().await;
            }.instrument(channel_span));

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
            "wechat" => {
                let wechat_config = channel_config
                    .wechat
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing wechat config")
                    })?
                    .clone();

                let router_for_callback = router.clone();
                let wechat_sender_arc_clone = wechat_sender_arc.clone().unwrap();

                let topic_manager_for_task = topic_manager.clone();

                let task = tokio::spawn(async move {
                use jyc_channels::wechat::inbound::WechatMatcher;

                // Create the adapter with the shared sender Arc so it can
                // update the outbound sender on each reconnection.
                let adapter = WechatInboundAdapter::with_shared_sender(
                    &wechat_config,
                    channel_name_owned.clone(),
                    wechat_sender_arc_clone,
                );

                let topic_manager_clone = topic_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();

                        tokio::spawn(async move {
                            router.route(&WechatMatcher, message).await;
                        });

                        Ok(())
                    }),
                    on_topic_close: Some(Box::new(move |topic_name: String| {
                        let tm = topic_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_topic(&topic_name).await {
                                tracing::error!(error = %e, topic = %topic_name, "Failed to close topic");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "WeChat inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(
                        error = %e,
                        "WeChat inbound adapter error"
                    );
                }

                // Shutdown topic manager for this channel
                tm.shutdown().await;
            }.instrument(channel_span));

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
            "wecom_bot" => {
                let wecom_bot_config = channel_config
                    .wecom_bot
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing wecom_bot config")
                    })?
                    .clone();

                let router_for_callback = router.clone();
                let wecom_bot_handle_arc_clone = wecom_bot_handle_arc.clone().unwrap();

                let topic_manager_for_task = topic_manager.clone();

                let task = tokio::spawn(async move {

                let adapter = WecomBotInboundAdapter::with_shared_handle(
                    &wecom_bot_config,
                    channel_name_owned.clone(),
                    wecom_bot_handle_arc_clone,
                );

                let topic_manager_clone = topic_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();

                        tokio::spawn(async move {
                            router.route(&WecomBotMatcher, message).await;
                        });

                        Ok(())
                    }),
                    on_topic_close: Some(Box::new(move |topic_name: String| {
                        let tm = topic_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_topic(&topic_name).await {
                                tracing::error!(error = %e, topic = %topic_name, "Failed to close topic");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "WeCom Bot inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(
                        error = %e,
                        "WeCom Bot inbound adapter error"
                    );
                }

                tm.shutdown().await;
            }.instrument(channel_span));

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
            "wecom" => {
                let wecom_config = channel_config
                    .wecom
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing wecom config")
                    })?
                    .clone();

                let wecom_server = wecom_server
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("WeCom webhook server not initialized"))?;
                let router_for_callback = router.clone();
                let channel_name_owned = channel_name.clone();

                let topic_manager_for_task = topic_manager.clone();

                let task = tokio::spawn(async move {
                use jyc_channels::wecom::inbound::WecomMatcher;

                let adapter = WecomInboundAdapter::new(
                    &wecom_config,
                    &channel_name_owned,
                    wecom_server,
                );

                let topic_manager_clone = topic_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();

                        tokio::spawn(async move {
                            router.route(&WecomMatcher, message).await;
                        });

                        Ok(())
                    }),
                    on_topic_close: Some(Box::new(move |topic_name: String| {
                        let tm = topic_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_topic(&topic_name).await {
                                tracing::error!(error = %e, topic = %topic_name, "Failed to close topic");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "WeCom inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(
                        error = %e,
                        "WeCom inbound adapter error"
                    );
                }

                // Shutdown topic manager for this channel
                tm.shutdown().await;
            }.instrument(channel_span));

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
            "wecomkf" => {
                let wecomkf_config = channel_config
                    .wecom_kf
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("channel '{channel_name}': missing wecom_kf config")
                    })?
                    .clone();

                let wecom_server = wecom_server
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("WeCom webhook server not initialized"))?;
                let router_for_callback = router.clone();
                let channel_name_owned = channel_name.clone();

                let kf_client = wecomkf_kf_client.clone().ok_or_else(|| {
                    anyhow::anyhow!("KfApiClient not initialized for wecomkf channel")
                })?;

                let cursor_store = Arc::new(KfCursorStore::new(
                    wecomkf_config
                        .cursor_store_path
                        .as_ref()
                        .map(std::path::PathBuf::from),
                ));
                let dedup_store = Arc::new(KfDedupStore::new());

                let topic_manager_for_task = topic_manager.clone();

                let task = tokio::spawn(async move {

                let adapter = WecomKfInboundAdapter::new(
                    &wecomkf_config,
                    &channel_name_owned,
                    wecom_server,
                    kf_client,
                    cursor_store,
                    dedup_store,
                );

                let topic_manager_clone = topic_manager_for_task.clone();
                let options = jyc_types::InboundAdapterOptions {
                    on_message: Box::new(move |message| {
                        let router = router_for_callback.clone();

                        tokio::spawn(async move {
                            router.route(&WecomKfMatcher, message).await;
                        });

                        Ok(())
                    }),
                    on_topic_close: Some(Box::new(move |topic_name: String| {
                        let tm = topic_manager_clone.clone();
                        tokio::spawn(async move {
                            if let Err(e) = tm.close_topic(&topic_name).await {
                                tracing::error!(error = %e, topic = %topic_name, "Failed to close topic");
                            }
                        });
                        Ok(())
                    })),
                    on_error: Box::new(|error| {
                        tracing::error!(error = %error, "WeCom KF inbound error");
                    }),
                    attachment_config: inbound_attachment_config.clone(),
                };

                if let Err(e) = adapter.start(options, cancel_child).await {
                    tracing::error!(
                        error = %e,
                        "WeCom KF inbound adapter error"
                    );
                }

                // Shutdown topic manager for this channel
                tm.shutdown().await;
            }.instrument(channel_span));

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
            "websocket" => {
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
                            if let Err(e) = tm.close_topic(&topic_name).await {
                                tracing::error!(error = %e, topic = %topic_name, "Failed to close topic");
                            }
                        });
                        Ok(())
                    })),
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
            _ => {} // Gracefully skip unknown channel types
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reply_attachments_reads_entries() {
        let v = serde_json::json!({
            "type": "reply",
            "topic": "t",
            "text": "done",
            "attachments": [
                {"filename": "a.pdf", "path": "/api/topics/local_dev/t/files/a.pdf", "content_type": "application/pdf"},
                {"filename": "b.png", "path": "/api/topics/local_dev/t/files/b.png"}
            ]
        });
        let atts = parse_reply_attachments(&v);
        assert_eq!(atts.len(), 2);
        assert_eq!(atts[0].filename, "a.pdf");
        assert_eq!(atts[0].url_path, "/api/topics/local_dev/t/files/a.pdf");
        assert_eq!(atts[0].content_type, "application/pdf");
        // content_type is optional — defaults to octet-stream
        assert_eq!(atts[1].content_type, "application/octet-stream");
    }

    #[test]
    fn parse_reply_attachments_absent_or_malformed() {
        assert!(parse_reply_attachments(&serde_json::json!({"type": "reply"})).is_empty());
        assert!(parse_reply_attachments(&serde_json::json!({"attachments": "nope"})).is_empty());
        // Entry missing required fields is skipped, valid sibling kept
        let v = serde_json::json!({"attachments": [{"filename": "x"}, {"filename": "y", "path": "/p"}]});
        let atts = parse_reply_attachments(&v);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename, "y");
    }

    #[test]
    fn loopback_addr_replaces_wildcards() {
        assert_eq!(loopback_addr("127.0.0.1:9876"), "127.0.0.1:9876");
        assert_eq!(loopback_addr("0.0.0.0:9876"), "127.0.0.1:9876");
        assert_eq!(loopback_addr("[::]:9876"), "127.0.0.1:9876");
    }

    /// Regression: piping re-targets only channel/topic — attachment bytes
    /// and metadata (chat_id, sender identity) must survive the forward.
    #[test]
    fn pipe_retarget_preserves_attachments_and_metadata() {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("chat_id".to_string(), serde_json::json!("oc_abc"));
        let msg = jyc_types::InboundMessage {
            id: "m1".to_string(),
            channel: "feishu_bot".to_string(),
            channel_uid: "om_x".to_string(),
            sender: "金晔".to_string(),
            sender_address: "ou_abc".to_string(),
            recipients: vec![],
            topic: "greenfield 下单".to_string(),
            content: jyc_types::MessageContent {
                text: Some("[File: a.pdf]".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            references: None,
            reply_to_id: None,
            external_id: Some("om_x".to_string()),
            attachments: vec![jyc_types::MessageAttachment {
                filename: "a.pdf".to_string(),
                content_type: "application/pdf".to_string(),
                size: 3,
                content: Some(vec![1, 2, 3]),
                saved_path: None,
            }],
            metadata,
            matched_pattern: None,
        };
        let pipe = jyc_types::PipeTarget {
            channel: "local_dev".to_string(),
            topic: "jyc".to_string(),
        };

        let out = apply_pipe_retarget(msg, &pipe);

        assert_eq!(out.channel, "local_dev");
        assert_eq!(out.topic, "jyc");
        assert_eq!(out.sender, "金晔");
        assert_eq!(out.attachments.len(), 1);
        assert_eq!(out.attachments[0].content.as_deref(), Some(&[1, 2, 3][..]));
        assert_eq!(
            out.metadata.get("chat_id").and_then(|v| v.as_str()),
            Some("oc_abc")
        );
    }
}
