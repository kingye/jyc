//! Channel adapter construction for `jyc serve`.
//!
//! Extracted from the monolithic `serve.rs` run() function.

use serde_json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::broadcast;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use anyhow::Result;

use jyc_channels::email::inbound::EmailMatcher;
use jyc_channels::feishu::client::FeishuClient;
use jyc_channels::feishu::inbound::{FeishuInboundAdapter, FeishuMatcher};
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
use jyc_channels::wecom_bot::inbound::{WecomBotInboundAdapter, WecomBotMatcher};
use jyc_core::channel_orchestrator::ChannelOrchestrator;
use jyc_core::message_router::MessageRouter;
use jyc_core::message_storage::MessageStorage;
use jyc_core::state_manager::StateManager;
use jyc_core::topic_manager::TopicManager;
use jyc_services::imap::monitor::ImapMonitor;
use jyc_types::{
    ChannelConfig, ChannelInfo, ChannelMatcher, ChannelPattern, InboundAdapter,
    InboundAttachmentConfig, MonitorConfig, OutboundAdapter, OutboundAttachmentConfig,
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
    wecomkf_kf_client: &mut Option<Arc<KfApiClient>>,
    ws_handler_for_channel: &mut HashMap<String, Arc<WebsocketInboundAdapter>>,
    websocket_handlers: &mut Vec<Arc<WebsocketInboundAdapter>>,
    ws_broadcasts: std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
) -> Result<Option<Arc<dyn OutboundAdapter>>> {
    let outbound: Arc<dyn OutboundAdapter> = match channel_type {
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
fn apply_pipe_retarget(
    mut msg: jyc_types::InboundMessage,
    pipe: &jyc_types::PipeTarget,
) -> Option<jyc_types::InboundMessage> {
    // Mutually exclusive: reject configs that mix the new agent form
    // with the legacy channel/pattern form.
    if pipe.agent.is_some() && (pipe.channel.is_some() || pipe.pattern.is_some()) {
        tracing::warn!(
            agent = ?pipe.agent,
            channel = ?pipe.channel,
            pattern = ?pipe.pattern,
            "pipe.agent is mutually exclusive with pipe.channel/pipe.pattern; dropping"
        );
        return None;
    }

    // New form: pipe.agent routes through the synthesized "agents"
    // channel. The agent name is the routing identity (selects which
    // [agents.<name>] pattern to apply); pipe.topic (if present) selects
    // the per-conversation sub-topic directory under the agent's
    // workspace.
    //
    // Record the agent name as `pipe_pattern` so the WebsocketMatcher
    // selects the agent's pattern by name — even when pipe.topic is
    // dynamic (e.g. `${msg.channel_uid}`) and the resolved topic name
    // matches no existing pattern. Without this hint, the matcher
    // would fall back to using the topic name as the pattern name and
    // the agent's mcps/skills/model/template would never apply.
    if let Some(agent_name) = &pipe.agent {
        let template = pipe.topic.as_deref().unwrap_or(agent_name.as_str());
        let topic = resolve_msg_placeholders(template, &msg)?;
        msg.metadata.insert(
            jyc_types::PIPE_PATTERN_METADATA_KEY.to_string(),
            serde_json::Value::String(agent_name.clone()),
        );
        msg.channel = "agents".to_string();
        msg.topic = topic;
        return Some(msg);
    }

    // Legacy form.
    // (Deprecation warning fires once at startup in spawn_feishu_adapter,
    // not per-message — keeps the chat log clean when the adapter is
    // chatty.)
    let template = pipe.topic.as_deref().or(pipe.pattern.as_deref())?;
    let topic = resolve_msg_placeholders(template, &msg)?;
    if let Some(pattern) = &pipe.pattern {
        msg.metadata.insert(
            jyc_types::PIPE_PATTERN_METADATA_KEY.to_string(),
            serde_json::Value::String(pattern.clone()),
        );
    }
    msg.channel = pipe
        .channel
        .clone()
        .expect("legacy pipe form requires channel");
    msg.topic = topic;
    Some(msg)
}

/// Resolve every `${msg.<key>}` in `template` against the message's metadata
/// (or the `channel_uid`/`topic` core fields for those special keys). Returns
/// `None` if any placeholder is present but the resolved value is
/// missing/empty (caller drops with warning). When the template contains no
/// `${msg.*}` placeholders, returns the template unchanged.
fn resolve_msg_placeholders(template: &str, msg: &jyc_types::InboundMessage) -> Option<String> {
    static PLACEHOLDER_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re =
        PLACEHOLDER_RE.get_or_init(|| regex::Regex::new(r"\$\{msg\.([A-Za-z0-9_]+)\}").unwrap());

    if !template.contains("${msg.") {
        return Some(template.to_string());
    }

    let mut out = template.to_string();
    for caps in re.captures_iter(template) {
        let full = caps.get(0).unwrap().as_str();
        let key = caps.get(1).unwrap().as_str();
        let raw = lookup_msg_placeholder(key, msg)?;
        let sanitized = jyc_utils::helpers::sanitize_for_filesystem(&raw);
        if sanitized.is_empty() {
            tracing::warn!(
                key = %key,
                "pipe topic placeholder ${{msg.<key>}} resolved to empty after sanitization, dropping"
            );
            return None;
        }
        out = out.replace(full, &sanitized);
    }
    Some(out)
}

/// Look up a single `${msg.<key>}` value: metadata first, then the
/// `channel_uid` core field (unifies group chatid / single-chat userid
/// in one template) or the `topic` core field (the channel's own derived
/// conversation name — for email, the subject with `Re:`/`Fw:` prefixes
/// already stripped). Returns `None` when the key is missing/empty.
fn lookup_msg_placeholder(key: &str, msg: &jyc_types::InboundMessage) -> Option<String> {
    if let Some(v) = msg.metadata.get(key).and_then(|v| v.as_str())
        && !v.is_empty()
    {
        return Some(v.to_string());
    }
    if key == "channel_uid" && !msg.channel_uid.is_empty() {
        return Some(msg.channel_uid.clone());
    }
    if key == "topic" && !msg.topic.is_empty() {
        return Some(msg.topic.clone());
    }
    None
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

/// Download one reply attachment from the inspect server, apply the
/// operator's outbound policy, and stage it in a temp file.
///
/// Shared by all pipe reply forwarders (feishu / email / wecom_bot): the
/// upload APIs take a path, the validator takes a path, SMTP takes bytes —
/// so both are returned. The temp file lives until the caller drops it.
async fn fetch_reply_attachment(
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

/// Download one reply attachment from the inspect server and send it to the
/// feishu chat (image vs. file chosen by content type).
async fn relay_attachment(
    inspect: &jyc_inspect::client::InspectClient,
    client: &FeishuClient,
    chat_id: &str,
    att: &ReplyAttachmentRef,
    config: &arc_swap::ArcSwap<jyc_types::AppConfig>,
) -> Result<()> {
    use jyc_channels::feishu::client::{feishu_file_type, is_image_content_type};

    let (_bytes, tmp) = fetch_reply_attachment(inspect, att, config).await?;

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

/// Collect the distinct WebSocket-channel broadcast targets that a
/// feishu adapter's pipe patterns route through. One entry per
/// distinct channel — the reply-forwarder spawns one subscriber per
/// entry, so missing a target here means feishu replies on that
/// channel vanish into the dashboard's broadcast only.
///
/// Two forms accepted per pattern:
/// - legacy `pipe = { channel = "x" }` → inserts "x"
/// - new `pipe = { agent = "x", topic = "..." }` → inserts "agents"
///   (the synthesized channel name)
fn collect_pipe_target_channels(patterns: &[ChannelPattern]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for p in patterns.iter().filter(|p| p.enabled) {
        if let Some(pipe) = &p.pipe {
            if let Some(ch) = &pipe.channel {
                out.insert(ch.clone());
            } else if pipe.agent.is_some() {
                out.insert("agents".to_string());
            }
        }
    }
    out
}

/// State recorded per piped topic so the email reply forwarder can
/// reply into the original mail thread.
///
/// Known limitation (same as feishu/wecom_bot): the map is in-memory and
/// keyed by resolved topic, so it is rebuilt from inbound traffic after a
/// restart, and two senders sharing one subject share one entry
/// (last writer wins).
#[derive(Debug, Clone)]
struct EmailReplyState {
    /// Recipient address (the original sender).
    recipient: String,
    /// Original (prefix-stripped) subject; SMTP adds the `Re:` prefix.
    subject: String,
    /// Original `Message-ID`, echoed back as `In-Reply-To`.
    in_reply_to: Option<String>,
    /// `References` chain: the original chain plus the original Message-ID.
    references: Vec<String>,
}

/// Spawn a pipe-only email adapter: the IMAP monitor plus one reply
/// forwarder per distinct pipe target channel.
///
/// Mirrors `spawn_feishu_adapter` (see `docs/core-hub-adapters.md`).
/// Differences specific to email:
///
/// - Keeps a `StateManager` (`<workdir>/channels/<channel>/.imap/`): it
///   tracks the mailbox cursor (sequence number + processed UIDs), which
///   is protocol-level dedup state, not conversation state.
/// - When the matched pattern's `pipe` has no explicit `topic`, the
///   subject-derived topic name is used (email's natural topic identity).
/// - Replies are plain text — no model/mode/token footer.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_email_adapter(
    channel_config: &ChannelConfig,
    channel_name: String,
    workdir: &Path,
    args: &crate::cli::serve::ServeArgs,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
    config_for_spawn: Arc<arc_swap::ArcSwap<jyc_types::AppConfig>>,
    ws_broadcasts: std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
    routers: std::sync::Arc<std::sync::Mutex<HashMap<String, Arc<MessageRouter>>>>,
) -> Result<()> {
    let imap_config = channel_config
        .inbound
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing inbound config"))?
        .clone();
    let smtp_config = channel_config
        .outbound
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing outbound config"))?
        .clone();

    // `--no-idle` forces polling.
    let monitor_config = channel_config.monitor.clone().unwrap_or_default();
    let monitor_config = if args.no_idle {
        MonitorConfig {
            mode: "poll".to_string(),
            ..monitor_config
        }
    } else {
        monitor_config
    };

    // Email is pipe-only: every enabled pattern must name a pipe target
    // (a websocket hub channel). Patterns without one are a configuration
    // error — warn at startup, drop matching messages at runtime.
    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    for p in channel_config
        .patterns
        .iter()
        .flatten()
        .filter(|p| p.enabled)
    {
        match &p.pipe {
            Some(pipe) => {
                if pipe.channel.is_some() || pipe.pattern.is_some() {
                    tracing::warn!(
                        channel = %channel_name,
                        pattern = %p.name,
                        "email pipe.channel/pipe.pattern is deprecated; use pipe = {{ agent = \"...\", topic = \"...\" }}"
                    );
                }
            }
            None => tracing::warn!(
                channel = %channel_name,
                pattern = %p.name,
                "email pattern has no pipe target; matching messages will be dropped"
            ),
        }
    }

    // Mailbox cursor state lives under <workdir>/channels/<channel>/.imap/.
    let mut state_manager = StateManager::for_channel(&workdir.join("channels"), &channel_name);
    state_manager.initialize().await?;
    if args.reset {
        state_manager.reset().await?;
        tracing::info!(channel = %channel_name, "State reset");
    }
    tracing::info!(
        channel = %channel_name,
        last_seq = state_manager.last_sequence_number(),
        processed_uids = state_manager.processed_uid_count(),
        "State loaded"
    );

    let channel_span = tracing::info_span!("in", ch = %channel_name);
    let workdir_for_task = workdir.to_path_buf();

    let task = tokio::spawn(
        async move {
            // Shared SMTP client + topic -> reply state for pipe relaying.
            let smtp = Arc::new(Mutex::new(jyc_services::smtp::client::SmtpClient::new(
                smtp_config.clone(),
            )));
            let from_address = smtp_config
                .from_address
                .clone()
                .unwrap_or_else(|| smtp_config.username.clone());
            let from_name = smtp_config.from_name.clone();
            let topic_state: std::sync::Arc<std::sync::Mutex<HashMap<String, EmailReplyState>>> =
                std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

            // Attachment relay needs the inspect server (reply broadcasts
            // carry download paths served by its files endpoint). `None`
            // when inspect is disabled — text relaying is unaffected,
            // attachments are dropped with a warning (same as feishu).
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
                let smtp = smtp.clone();
                let from_address = from_address.clone();
                let from_name = from_name.clone();
                let channel = channel.clone();
                let inspect_client = inspect_client.clone();
                let config_for_relay = config_for_spawn.clone();
                tokio::spawn(async move {
                    let Some(broadcast_tx) = wait_for_broadcast(&ws_broadcasts, &channel).await
                    else {
                        tracing::error!(
                            channel = %channel,
                            "email pipe: target channel broadcast never appeared (is it a websocket channel?), reply forwarder not started"
                        );
                        return;
                    };
                    let mut rx = broadcast_tx.subscribe();
                    tracing::info!(channel = %channel, "email pipe reply forwarder subscribed");
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
                        let Some(state) = topic_state.lock().unwrap().get(topic).cloned() else {
                            tracing::debug!(
                                topic = %topic,
                                "email pipe: no address mapping for reply, skipping"
                            );
                            continue;
                        };

                        // Download reply attachments (inspect files endpoint),
                        // applying the operator's outbound policy.
                        let mut email_attachments = Vec::new();
                        for att in parse_reply_attachments(&v) {
                            let Some(inspect) = &inspect_client else {
                                tracing::warn!(
                                    filename = %att.filename,
                                    "email pipe: attachment dropped (inspect server disabled)"
                                );
                                continue;
                            };
                            match load_reply_attachment(inspect, &att, &config_for_relay).await {
                                Ok(loaded) => email_attachments.push(loaded),
                                Err(e) => tracing::warn!(
                                    filename = %att.filename,
                                    error = format!("{e:#}"),
                                    "email pipe: failed to load attachment"
                                ),
                            }
                        }

                        let body = jyc_core::email_parser::strip_trailing_separators(text);
                        let mut smtp = smtp.lock().await;
                        // Lazy connect: the transport is built on first use
                        // (and rebuilt by send_with_retry on drops).
                        if !smtp.is_connected()
                            && let Err(e) = smtp.connect().await
                        {
                            tracing::error!(error = %e, "email pipe: SMTP connect failed, reply dropped");
                            continue;
                        }
                        if let Err(e) = smtp
                            .send_reply(
                                &from_address,
                                from_name.as_deref(),
                                &state.recipient,
                                &state.subject,
                                &body,
                                state.in_reply_to.as_deref(),
                                if state.references.is_empty() {
                                    None
                                } else {
                                    Some(&state.references)
                                },
                                if email_attachments.is_empty() {
                                    None
                                } else {
                                    Some(&email_attachments)
                                },
                            )
                            .await
                        {
                            tracing::error!(
                                error = format!("{e:#}"),
                                topic = %topic,
                                "email pipe: failed to relay reply"
                            );
                        }
                    }
                });
            }

            let channel_name_for_monitor = channel_name.clone();
            let mut monitor = ImapMonitor::new(
                channel_name.clone(),
                imap_config,
                monitor_config,
                state_manager,
                cancel,
                Box::new(move |message| {
                    let config_for_pipe = config_for_spawn.clone();
                    let topic_state = topic_state.clone();
                    let channel_name_self = channel_name_for_monitor.clone();
                    let routers = routers.clone();
                    tokio::spawn(async move {
                        let mut message = message;
                        // 1. Match this channel's patterns (rules).
                        let patterns = config_for_pipe
                            .load()
                            .channels
                            .get(&channel_name_self)
                            .and_then(|c| c.patterns.clone())
                            .unwrap_or_default();
                        let Some(pm) = EmailMatcher.match_message(&message, &patterns) else {
                            tracing::debug!(
                                subject = %message.topic,
                                "email: no pattern matched, dropping"
                            );
                            return;
                        };
                        // 2. Per-pattern `pipe`: the matched pattern decides.
                        let matched = patterns.iter().find(|p| p.name == pm.pattern_name);
                        let Some(pipe) = matched.and_then(|p| p.pipe.as_ref()) else {
                            tracing::warn!(
                                pattern = %pm.pattern_name,
                                "email: matched pattern has no pipe target, dropping message"
                            );
                            return;
                        };

                        // 3. Reply state, captured before re-targeting
                        //    rewrites channel/topic.
                        let mut references = message.references.clone().unwrap_or_default();
                        if let Some(ext_id) = &message.external_id {
                            references.push(ext_id.clone());
                        }
                        let reply_state = EmailReplyState {
                            recipient: message.sender_address.clone(),
                            subject: message.topic.clone(),
                            in_reply_to: message.external_id.clone(),
                            references,
                        };

                        // 4. Re-target into the target channel/topic. Without
                        //    an explicit `pipe.topic`, the pattern's
                        //    `topic_name` override wins, else the
                        //    subject-derived topic name (same precedence the
                        //    MessageRouter applied before the migration).
                        //    The derived name also replaces `message.topic`
                        //    (the parse-time subject, already `Re:`/`Fw:`
                        //    stripped but not pattern-prefix stripped or
                        //    sanitized) so `${msg.topic}` in a template
                        //    resolves to the same name the no-template path
                        //    would use. `apply_pipe_retarget` overwrites
                        //    `message.topic` with the resolved template anyway.
                        let derived_topic = matched
                            .and_then(|p| p.topic_name.clone())
                            .unwrap_or_else(|| {
                                EmailMatcher.derive_topic_name(&message, &patterns, Some(&pm))
                            });
                        let pipe = email_pipe_with_topic(pipe, &derived_topic);
                        message.topic = derived_topic;
                        let drop_debug = message.id.clone();
                        let Some(message) = apply_pipe_retarget(message, &pipe) else {
                            tracing::warn!(
                                topic = ?pipe.topic,
                                pattern = ?pipe.pattern,
                                agent = ?pipe.agent,
                                message_id = %drop_debug,
                                "email pipe: unresolvable target, dropping"
                            );
                            return;
                        };

                        // 5. Record resolved topic -> reply state.
                        topic_state
                            .lock()
                            .unwrap()
                            .insert(message.topic.clone(), reply_state);

                        // 6. Route through the target's own MessageRouter —
                        //    the same path as a chat-pane message.
                        let target_channel = pipe
                            .channel
                            .clone()
                            .or_else(|| pipe.agent.as_ref().map(|_| "agents".to_string()))
                            .expect("validated upstream: agent or channel required");
                        let Some(target_router) =
                            routers.lock().unwrap().get(&target_channel).cloned()
                        else {
                            tracing::warn!(
                                channel = %target_channel,
                                "email pipe: target channel router not found, dropping"
                            );
                            return;
                        };
                        target_router
                            .route(&WebsocketMatcher::new(target_channel), message)
                            .await;
                    });
                    Ok(())
                }),
            );

            if let Err(e) = monitor.start().await {
                tracing::error!(error = %e, "IMAP monitor error");
            }
        }
        .instrument(channel_span),
    );
    tasks.push(task);
    Ok(())
}

/// Effective pipe target for an email message: when the pattern's `pipe`
/// names no `topic`, the subject-derived topic name is used — email's
/// natural topic identity (one thread per subject).
fn email_pipe_with_topic(
    pipe: &jyc_types::PipeTarget,
    derived_topic: &str,
) -> jyc_types::PipeTarget {
    if pipe.topic.is_some() {
        return pipe.clone();
    }
    jyc_types::PipeTarget {
        topic: Some(derived_topic.to_string()),
        ..pipe.clone()
    }
}

/// Download one reply attachment from the inspect server, apply the
/// operator's outbound policy, and return it as an SMTP attachment.
async fn load_reply_attachment(
    inspect: &jyc_inspect::client::InspectClient,
    att: &ReplyAttachmentRef,
    config: &arc_swap::ArcSwap<jyc_types::AppConfig>,
) -> Result<jyc_services::smtp::client::EmailAttachment> {
    let (bytes, _tmp) = fetch_reply_attachment(inspect, att, config).await?;
    Ok(jyc_services::smtp::client::EmailAttachment {
        filename: att.filename.clone(),
        content_type: att.content_type.clone(),
        data: bytes,
    })
}

/// Spawn a pipe-only feishu adapter: the inbound adapter plus one reply
/// forwarder per distinct pipe target channel.
///
/// Unlike full channels, a feishu adapter has no outbound adapter, agent
/// service, TopicManager, StateManager, or orchestrator registration — all
/// topics live in the pipe target (hub) channel. See
/// `docs/core-hub-adapters.md`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_feishu_adapter(
    channel_config: &ChannelConfig,
    channel_name: String,
    workdir: &Path,
    inbound_attachment_config: Option<InboundAttachmentConfig>,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
    config_for_spawn: Arc<arc_swap::ArcSwap<jyc_types::AppConfig>>,
    ws_broadcasts: std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
    routers: std::sync::Arc<std::sync::Mutex<HashMap<String, Arc<MessageRouter>>>>,
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
    for p in channel_config
        .patterns
        .iter()
        .flatten()
        .filter(|p| p.enabled)
    {
        match &p.pipe {
            Some(pipe) => {
                // One-shot deprecation at startup: don't repeat per
                // message. Per-pattern (not per-message) so a feishu
                // adapter with 5 legacy pipes still emits 5 warns.
                if pipe.channel.is_some() || pipe.pattern.is_some() {
                    tracing::warn!(
                        channel = %channel_name,
                        pattern = %p.name,
                        "pipe.channel/pipe.pattern is deprecated; use pipe = {{ agent = \"...\", topic = \"...\" }}"
                    );
                }
            }
            None => tracing::warn!(
                channel = %channel_name,
                pattern = %p.name,
                "feishu pattern has no pipe target; matching messages will be dropped"
            ),
        }
    }

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

            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let config_for_pipe = config_for_spawn.clone();
                    let topic_chat = topic_chat.clone();
                    let channel_name_self = channel_name.clone();
                    let routers = routers.clone();
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
                            // Pipe-only adapter: a matched pattern without a
                            // pipe target is a configuration error.
                            tracing::warn!(
                                pattern = %pm.pattern_name,
                                "feishu: matched pattern has no pipe target, dropping message"
                            );
                            return;
                        };
                        // 3. Re-target into the target channel/topic —
                        //    resolves the effective topic (`topic ?? pattern`)
                        //    and `${msg.chat_name}` placeholders against
                        //    message metadata. Identifiers captured before
                        //    the move, for the drop warning below.
                        let drop_debug =
                            (message.id.clone(), message.metadata.get("chat_id").cloned());
                        let Some(message) = apply_pipe_retarget(message, pipe) else {
                            tracing::warn!(
                                topic = ?pipe.topic,
                                pattern = ?pipe.pattern,
                                agent = ?pipe.agent,
                                message_id = %drop_debug.0,
                                chat_id = ?drop_debug.1,
                                "feishu pipe: unresolvable target (no topic/pattern configured, or ${{msg.chat_name}} without chat_name metadata), dropping"
                            );
                            return;
                        };
                        // 4. Record resolved topic -> chat_id for reply relay.
                        if let Some(chat_id) =
                            message.metadata.get("chat_id").and_then(|v| v.as_str())
                        {
                            topic_chat
                                .lock()
                                .unwrap()
                                .insert(message.topic.clone(), chat_id.to_string());
                        }
                        // 5. Route through the target's own MessageRouter —
                        //    the exact same path as a chat-pane message, so
                        //    topic_path/template/skills apply identically.
                        //    New form: pipe.agent routes into the synthesized
                        //    "agents" channel; legacy form: pipe.channel.
                        let target_channel = pipe
                            .channel
                            .clone()
                            .or_else(|| pipe.agent.as_ref().map(|_| "agents".to_string()))
                            .expect("validated upstream: agent or channel required");
                        let Some(target_router) =
                            routers.lock().unwrap().get(&target_channel).cloned()
                        else {
                            tracing::warn!(channel = %target_channel, "feishu pipe: target channel router not found, dropping");
                            return;
                        };
                        target_router
                            .route(&WebsocketMatcher::new(target_channel), message)
                            .await;
                    });
                    Ok(())
                }),
                on_topic_close: None,
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
/// Mirrors `spawn_feishu_adapter` (see `docs/core-hub-adapters.md`).
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
    routers: std::sync::Arc<std::sync::Mutex<HashMap<String, Arc<MessageRouter>>>>,
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
    for p in channel_config
        .patterns
        .iter()
        .flatten()
        .filter(|p| p.enabled)
    {
        match &p.pipe {
            Some(pipe) => {
                if pipe.channel.is_some() || pipe.pattern.is_some() {
                    tracing::warn!(
                        channel = %channel_name,
                        pattern = %p.name,
                        "wecom_bot pipe.channel/pipe.pattern is deprecated; use pipe = {{ agent = \"...\", topic = \"...\" }}"
                    );
                }
            }
            None => tracing::warn!(
                channel = %channel_name,
                pattern = %p.name,
                "wecom_bot pattern has no pipe target; matching messages will be dropped"
            ),
        }
    }

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
                        // 1. Match this channel's patterns (rules).
                        let patterns = config_for_pipe
                            .load()
                            .channels
                            .get(&channel_name_self)
                            .and_then(|c| c.patterns.clone())
                            .unwrap_or_default();
                        let Some(pm) = WecomBotMatcher.match_message(&message, &patterns) else {
                            tracing::debug!(
                                chat = %message.topic,
                                "wecom_bot: no pattern matched, dropping"
                            );
                            return;
                        };
                        // 2. Per-pattern `pipe`: the matched pattern decides.
                        let matched = patterns.iter().find(|p| p.name == pm.pattern_name);
                        let Some(pipe) = matched.and_then(|p| p.pipe.as_ref()) else {
                            tracing::warn!(
                                pattern = %pm.pattern_name,
                                "wecom_bot: matched pattern has no pipe target, dropping message"
                            );
                            return;
                        };

                        // 3. Re-target into the target channel/topic.
                        let drop_debug = (message.id.clone(), message.channel_uid.clone());
                        let Some(message) = apply_pipe_retarget(message, pipe) else {
                            tracing::warn!(
                                topic = ?pipe.topic,
                                pattern = ?pipe.pattern,
                                agent = ?pipe.agent,
                                message_id = %drop_debug.0,
                                channel_uid = %drop_debug.1,
                                "wecom_bot pipe: unresolvable target (no topic/pattern configured, or ${{msg.<key>}} unresolved), dropping"
                            );
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
                        let target_channel = pipe
                            .channel
                            .clone()
                            .or_else(|| pipe.agent.as_ref().map(|_| "agents".to_string()))
                            .expect("validated upstream: agent or channel required");
                        let Some(target_router) =
                            routers.lock().unwrap().get(&target_channel).cloned()
                        else {
                            tracing::warn!(
                                channel = %target_channel,
                                "wecom_bot pipe: target channel router not found, dropping"
                            );
                            return;
                        };
                        target_router
                            .route(&WebsocketMatcher::new(target_channel), message)
                            .await;
                    });
                    Ok(())
                }),
                on_topic_close: None,
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

/// Download one reply attachment from the inspect server, upload it to
/// the WeCom user via the shared WebSocket handle, and send the media
/// message via `aibot_send_msg` (proactive) keyed by the recipient.
///
/// Mirrors `relay_attachment` (feishu). Proactive send is used here
/// instead of `aibot_respond_msg` because the agent's reply is async
/// and the WeCom passive reply window may have closed by the time the
/// forwarder relays attachments.
async fn relay_wecom_attachment(
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
async fn send_wecom_proactive_text(
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

/// Shared per-channel context for spawning the inbound monitor task(s).
pub(crate) struct InboundSpawner<'a> {
    pub(crate) channel_type: &'a str,
    pub(crate) channel_config: &'a ChannelConfig,
    pub(crate) channel_name: String,
    pub(crate) workdir: &'a Path,
    pub(crate) workspace_dir: PathBuf,
    pub(crate) inbound_attachment_config: Option<InboundAttachmentConfig>,
    pub(crate) topic_manager: Arc<TopicManager>,
    pub(crate) router: Arc<MessageRouter>,
    pub(crate) cancel: CancellationToken,
    pub(crate) cancel_child: CancellationToken,
    pub(crate) tasks: &'a mut Vec<JoinHandle<()>>,
    pub(crate) wechat_sender_arc: &'a mut Option<Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>>,
    pub(crate) wecomkf_kf_client: &'a mut Option<Arc<KfApiClient>>,
    pub(crate) orchestrator: Arc<ChannelOrchestrator>,
    pub(crate) channel_info: ChannelInfo,
    pub(crate) config_for_spawn: Arc<arc_swap::ArcSwap<jyc_types::AppConfig>>,
    pub(crate) wecom_server: Option<Arc<WecomWebhookServer>>,
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
            channel_config,
            channel_name,
            workdir,
            workspace_dir,
            inbound_attachment_config,
            topic_manager,
            router,
            cancel,
            cancel_child,
            tasks,
            wechat_sender_arc,
            wecomkf_kf_client,
            orchestrator,
            channel_info,
            config_for_spawn,
            wecom_server,
            websocket_handlers,
        } = self;
        let channel_name_owned = channel_name.clone();
        let tm = topic_manager.clone();
        let channel_span = tracing::info_span!("in", ch = %channel_name);
        match channel_type {
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

    fn pipe_msg(
        metadata: std::collections::HashMap<String, serde_json::Value>,
    ) -> jyc_types::InboundMessage {
        jyc_types::InboundMessage {
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
        }
    }

    fn pipe_target(pattern: Option<&str>, topic: Option<&str>) -> jyc_types::PipeTarget {
        jyc_types::PipeTarget {
            agent: None,
            channel: Some("local_dev".to_string()),
            pattern: pattern.map(str::to_string),
            topic: topic.map(str::to_string),
        }
    }

    fn agent_pipe_target(agent: &str, topic: Option<&str>) -> jyc_types::PipeTarget {
        jyc_types::PipeTarget {
            agent: Some(agent.to_string()),
            channel: None,
            pattern: None,
            topic: topic.map(str::to_string),
        }
    }

    /// Regression: piping re-targets only channel/topic — attachment bytes
    /// and metadata (chat_id, sender identity) must survive the forward.
    #[test]
    fn pipe_retarget_preserves_attachments_and_metadata() {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("chat_id".to_string(), serde_json::json!("oc_abc"));
        let msg = pipe_msg(metadata);
        let pipe = pipe_target(None, Some("jyc"));

        let out = apply_pipe_retarget(msg, &pipe).unwrap();

        assert_eq!(out.channel, "local_dev");
        assert_eq!(out.topic, "jyc");
        assert_eq!(out.sender, "金晔");
        assert_eq!(out.attachments.len(), 1);
        assert_eq!(out.attachments[0].content.as_deref(), Some(&[1, 2, 3][..]));
        assert_eq!(
            out.metadata.get("chat_id").and_then(|v| v.as_str()),
            Some("oc_abc")
        );
        // Legacy topic-only form carries no pattern hint.
        assert!(
            !out.metadata
                .contains_key(jyc_types::PIPE_PATTERN_METADATA_KEY)
        );
    }

    /// `${msg.chat_name}` in `pipe.topic` resolves from message metadata,
    /// sanitized for filesystem use.
    #[test]
    fn pipe_retarget_resolves_chat_name_placeholder() {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("chat_name".to_string(), serde_json::json!("dev-jyc"));
        let pipe = pipe_target(None, Some("${msg.chat_name}"));
        let out = apply_pipe_retarget(pipe_msg(metadata), &pipe).unwrap();
        assert_eq!(out.topic, "dev-jyc");

        // Embedded placeholder with a prefix also resolves.
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            "chat_name".to_string(),
            serde_json::json!("greenfield 下单"),
        );
        let pipe = pipe_target(None, Some("feishu-${msg.chat_name}"));
        let out = apply_pipe_retarget(pipe_msg(metadata), &pipe).unwrap();
        assert_eq!(out.topic, "feishu-greenfield 下单");
    }

    /// Placeholder present but no chat_name metadata (e.g. P2P chat):
    /// returns None so the caller drops with a warning instead of
    /// misrouting to a literal "${msg.chat_name}" topic.
    #[test]
    fn pipe_retarget_unresolved_placeholder_returns_none() {
        let pipe = pipe_target(None, Some("${msg.chat_name}"));
        assert!(apply_pipe_retarget(pipe_msg(Default::default()), &pipe).is_none());

        let mut metadata = std::collections::HashMap::new();
        metadata.insert("chat_name".to_string(), serde_json::json!(""));
        assert!(apply_pipe_retarget(pipe_msg(metadata), &pipe).is_none());
    }

    /// `pattern`-only shorthand: the pattern name doubles as the topic name,
    /// and the pattern hint is recorded for the target matcher.
    #[test]
    fn pipe_retarget_pattern_shorthand() {
        let pipe = pipe_target(Some("jyc"), None);
        let out = apply_pipe_retarget(pipe_msg(Default::default()), &pipe).unwrap();
        assert_eq!(out.topic, "jyc");
        assert_eq!(
            out.metadata
                .get(jyc_types::PIPE_PATTERN_METADATA_KEY)
                .and_then(|v| v.as_str()),
            Some("jyc")
        );
    }

    /// Full dynamic form: pattern supplies config, topic derives per chat.
    #[test]
    fn pipe_retarget_pattern_with_dynamic_topic() {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("chat_name".to_string(), serde_json::json!("dev-jyc"));
        let pipe = pipe_target(Some("group_chat"), Some("${msg.chat_name}"));
        let out = apply_pipe_retarget(pipe_msg(metadata), &pipe).unwrap();
        assert_eq!(out.topic, "dev-jyc");
        assert_eq!(
            out.metadata
                .get(jyc_types::PIPE_PATTERN_METADATA_KEY)
                .and_then(|v| v.as_str()),
            Some("group_chat")
        );
    }

    /// Neither `topic` nor `pattern` set: config error, returns None.
    #[test]
    fn pipe_retarget_no_target_returns_none() {
        let pipe = pipe_target(None, None);
        assert!(apply_pipe_retarget(pipe_msg(Default::default()), &pipe).is_none());
    }

    /// New form: `pipe.agent` retargets into the synthesized "agents"
    /// channel with the agent name as the topic identity (when no
    /// `pipe.topic` is set).
    #[test]
    fn pipe_retarget_agent_only_uses_agent_name_as_topic() {
        let pipe = agent_pipe_target("jyc", None);
        let out = apply_pipe_retarget(pipe_msg(Default::default()), &pipe).unwrap();
        assert_eq!(out.channel, "agents");
        assert_eq!(out.topic, "jyc");
        // agent form records the agent name as the pattern hint so
        // WebsocketMatcher selects the [agents.jyc] pattern by name
        // (even when pipe.topic is dynamic).
        assert_eq!(
            out.metadata
                .get(jyc_types::PIPE_PATTERN_METADATA_KEY)
                .and_then(|v| v.as_str()),
            Some("jyc")
        );
    }

    /// New form with `${msg.chat_name}` placeholder in `pipe.topic`.
    #[test]
    fn pipe_retarget_agent_with_chat_name_placeholder() {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("chat_name".to_string(), serde_json::json!("dev-jyc"));
        let pipe = agent_pipe_target("jyc", Some("${msg.chat_name}"));
        let out = apply_pipe_retarget(pipe_msg(metadata), &pipe).unwrap();
        assert_eq!(out.channel, "agents");
        assert_eq!(out.topic, "dev-jyc");
        // The pattern hint must still be the agent name (not the resolved
        // topic) so WebsocketMatcher selects the [agents.jyc] pattern by
        // name even when the topic is dynamic.
        assert_eq!(
            out.metadata
                .get(jyc_types::PIPE_PATTERN_METADATA_KEY)
                .and_then(|v| v.as_str()),
            Some("jyc")
        );
    }

    /// New form without `pipe.topic`: falls back to the agent name.
    #[test]
    fn pipe_retarget_agent_explicit_topic_wins() {
        let pipe = agent_pipe_target("jyc", Some("general"));
        let out = apply_pipe_retarget(pipe_msg(Default::default()), &pipe).unwrap();
        assert_eq!(out.channel, "agents");
        assert_eq!(out.topic, "general");
        // The pattern hint must still be the agent name even when
        // pipe.topic is a static literal — WebsocketMatcher selects
        // [agents.jyc] by name, not the topic directory.
        assert_eq!(
            out.metadata
                .get(jyc_types::PIPE_PATTERN_METADATA_KEY)
                .and_then(|v| v.as_str()),
            Some("jyc")
        );
    }

    /// `pipe.agent` mixed with `pipe.channel` is rejected (mutual exclusion).
    #[test]
    fn pipe_retarget_agent_channel_mix_returns_none() {
        let pipe = jyc_types::PipeTarget {
            agent: Some("jyc".to_string()),
            channel: Some("local_dev".to_string()),
            pattern: None,
            topic: None,
        };
        assert!(apply_pipe_retarget(pipe_msg(Default::default()), &pipe).is_none());
    }

    /// `pipe.agent` mixed with `pipe.pattern` is rejected too.
    #[test]
    fn pipe_retarget_agent_pattern_mix_returns_none() {
        let pipe = jyc_types::PipeTarget {
            agent: Some("jyc".to_string()),
            channel: None,
            pattern: Some("jyc".to_string()),
            topic: None,
        };
        assert!(apply_pipe_retarget(pipe_msg(Default::default()), &pipe).is_none());
    }

    /// `${msg.chat_name}` placeholder with no `chat_name` metadata:
    /// agent form returns None (drops with warning at the call site).
    #[test]
    fn pipe_retarget_agent_unresolved_placeholder_returns_none() {
        let pipe = agent_pipe_target("jyc", Some("${msg.chat_name}"));
        assert!(apply_pipe_retarget(pipe_msg(Default::default()), &pipe).is_none());
    }

    /// Generalized placeholder: `${msg.<key>}` resolves any metadata key
    /// (not just hardcoded `chat_name`). Used by wecom_bot pipe configs
    /// like `topic = "bot-${msg.chatid}"`.
    #[test]
    fn pipe_retarget_resolves_arbitrary_metadata_key() {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("chatid".to_string(), serde_json::json!("chat_abc"));
        let pipe = agent_pipe_target("jin", Some("bot-${msg.chatid}"));
        let out = apply_pipe_retarget(pipe_msg(metadata), &pipe).unwrap();
        assert_eq!(out.channel, "agents");
        assert_eq!(out.topic, "bot-chat_abc");
    }

    /// `${msg.channel_uid}` unifies group chat (channel_uid = chatid) and
    /// single chat (channel_uid = userid) in one topic template — matches
    /// the wecom_bot `derive_topic_name` behavior.
    #[test]
    fn pipe_retarget_resolves_channel_uid_placeholder() {
        // channel_uid is set on the message itself (not metadata).
        let mut msg = pipe_msg(Default::default());
        msg.channel_uid = "user_xyz".to_string();
        let pipe = agent_pipe_target("jin", Some("bot-${msg.channel_uid}"));
        let out = apply_pipe_retarget(msg, &pipe).unwrap();
        assert_eq!(out.topic, "bot-user_xyz");
    }

    /// Unresolved `${msg.<key>}` with no metadata and no fallback field
    /// returns None (caller drops with warning).
    #[test]
    fn pipe_retarget_unresolved_unknown_key_returns_none() {
        let pipe = agent_pipe_target("jin", Some("bot-${msg.nonexistent}"));
        assert!(apply_pipe_retarget(pipe_msg(Default::default()), &pipe).is_none());
    }

    /// `${msg.topic}` resolves to the message's own topic field — the
    /// channel's derived conversation name. For email the adapter sets it
    /// to the subject with `Re:`/`Fw:` prefixes already stripped, so
    /// `topic = "mail-${msg.topic}"` composes a prefix with the subject.
    #[test]
    fn pipe_retarget_resolves_topic_placeholder() {
        let mut msg = pipe_msg(Default::default());
        msg.topic = "Invoice 42".to_string();
        let pipe = agent_pipe_target("jin", Some("mail-${msg.topic}"));
        let out = apply_pipe_retarget(msg, &pipe).unwrap();
        assert_eq!(out.topic, "mail-Invoice 42");
    }

    /// `${msg.topic}` on a message with an empty topic returns None
    /// (dropped rather than misrouted to a literal placeholder).
    #[test]
    fn pipe_retarget_empty_topic_placeholder_returns_none() {
        let mut msg = pipe_msg(Default::default());
        msg.topic = String::new();
        let pipe = agent_pipe_target("jin", Some("mail-${msg.topic}"));
        assert!(apply_pipe_retarget(msg, &pipe).is_none());
    }

    /// Multiple placeholders in one template all resolve, in left-to-right
    /// order. Sanitization happens per substitution (the `chatid`
    /// contains a `/` so the result is `chat_1`, exercising the
    /// filesystem-safe substitution).
    #[test]
    fn pipe_retarget_resolves_multiple_placeholders() {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("chatid".to_string(), serde_json::json!("chat/1"));
        let pipe = agent_pipe_target("jin", Some("agent/${msg.chatid}/${msg.channel_uid}"));
        let mut msg = pipe_msg(metadata);
        msg.channel_uid = "u1".to_string();
        let out = apply_pipe_retarget(msg, &pipe).unwrap();
        assert_eq!(out.topic, "agent/chat_1/u1");
    }

    // ---- collect_pipe_target_channels ----

    fn pattern_with(pipe: jyc_types::PipeTarget) -> ChannelPattern {
        ChannelPattern {
            name: format!("p-{}", pipe.topic.clone().unwrap_or_default()),
            channel: "feishu_bot".to_string(),
            enabled: true,
            pipe: Some(pipe),
            ..Default::default()
        }
    }

    /// Regression for PR #582: pipe.agent form was missing from
    /// pipe_channels, so the feishu reply forwarder wasn't subscribed
    /// to the synthesized "agents" channel's broadcast. As a result,
    /// feishu replies on agent-form pipes vanished into the dashboard
    /// only. (Catches the next time someone rearranges this loop.)
    #[test]
    fn collect_pipe_target_channels_legacy_form() {
        // Legacy pipe_target helper hardcodes channel = "local_dev";
        // the collector must use pipe.channel (not the pattern name).
        let patterns = vec![pattern_with(pipe_target(Some("jyc"), None))];
        let targets = collect_pipe_target_channels(&patterns);
        assert_eq!(
            targets,
            std::collections::HashSet::from(["local_dev".to_string()])
        );
    }

    #[test]
    fn collect_pipe_target_channels_agent_form_routes_to_agents() {
        let patterns = vec![pattern_with(agent_pipe_target("group_chat", Some("foo")))];
        let targets = collect_pipe_target_channels(&patterns);
        assert_eq!(
            targets,
            std::collections::HashSet::from(["agents".to_string()]),
            "pipe.agent must route to the synthesized 'agents' channel"
        );
    }

    #[test]
    fn collect_pipe_target_channels_dedupes_repeated_targets() {
        // Two patterns, both pointing at the same agent → one entry.
        let patterns = vec![
            pattern_with(agent_pipe_target("jyc", None)),
            pattern_with(agent_pipe_target("jyc", Some("topic"))),
        ];
        let targets = collect_pipe_target_channels(&patterns);
        assert_eq!(targets.len(), 1);
    }

    #[test]
    fn collect_pipe_target_channels_handles_empty_and_disabled() {
        let patterns = vec![
            // No pipe → no entry.
            ChannelPattern {
                name: "no-pipe".to_string(),
                enabled: true,
                ..Default::default()
            },
            // Disabled pattern with pipe → ignored.
            ChannelPattern {
                name: "disabled".to_string(),
                enabled: false,
                pipe: Some(agent_pipe_target("jyc", None)),
                ..Default::default()
            },
            // Enabled legacy form → still works (channel = "local_dev").
            pattern_with(pipe_target(Some("legacy"), None)),
        ];
        let targets = collect_pipe_target_channels(&patterns);
        assert_eq!(
            targets,
            std::collections::HashSet::from(["local_dev".to_string()])
        );
    }

    // ---- email_pipe_with_topic ----

    /// Without an explicit `pipe.topic`, email falls back to the derived
    /// topic (subject / pattern `topic_name`) — one thread per subject, the
    /// pre-migration MessageRouter behavior.
    #[test]
    fn email_pipe_with_topic_fills_derived_topic() {
        let pipe = email_pipe_with_topic(&agent_pipe_target("jin", None), "Invoice 42");
        assert_eq!(pipe.topic.as_deref(), Some("Invoice 42"));
        assert_eq!(pipe.agent.as_deref(), Some("jin"));
    }

    /// An explicit `pipe.topic` wins (including `${msg.*}` templates, which
    /// are resolved later by `apply_pipe_retarget`).
    #[test]
    fn email_pipe_with_topic_keeps_explicit_topic() {
        let pipe = email_pipe_with_topic(&agent_pipe_target("jin", Some("invoices")), "Invoice 42");
        assert_eq!(pipe.topic.as_deref(), Some("invoices"));
    }
}
