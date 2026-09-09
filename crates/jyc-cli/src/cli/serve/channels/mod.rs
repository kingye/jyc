//! Channel adapter construction for `jyc serve`.
//!
//! Extracted from the monolithic `serve.rs` run() function.

use anyhow::Result;
use jyc_channels::feishu::client::FeishuClient;
use jyc_channels::websocket::inbound::{WebsocketInboundAdapter, WebsocketMatcher};
use jyc_core::channel_orchestrator::ChannelOrchestrator;
use jyc_core::message_router::MessageRouter;
use jyc_core::topic_manager::TopicManager;
use jyc_types::{
    ChannelConfig, ChannelInfo, ChannelMatcher, ChannelPattern, InboundAdapter,
    InboundAttachmentConfig,
};
use serde_json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

mod email;
mod feishu;
mod gitee;
mod github;
#[cfg(test)]
mod tests;
mod wecom;
mod wecom_bot;

pub(crate) use email::spawn_email_adapter;
pub(crate) use feishu::spawn_feishu_adapter;
pub(crate) use gitee::spawn_gitee_adapter;
pub(crate) use github::spawn_github_adapter;
pub(crate) use wecom::{spawn_wecom_adapter, spawn_wecomkf_adapter};
pub(crate) use wecom_bot::spawn_wecom_bot_adapter;

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

/// Loopback address for in-process calls to the inspect server: a wildcard
/// bind (`0.0.0.0`, `[::]`) is not a connectable destination.
pub(super) fn loopback_addr(bind: &str) -> String {
    bind.replace("0.0.0.0", "127.0.0.1")
        .replace("[::]", "127.0.0.1")
}

/// Strip trailing separators and prefix a reply with its `[Role]` header
/// (skipped when the reply already carries it). Shared by the GitHub and
/// Gitee pipe reply forwarders.
pub(super) fn role_prefixed_body(text: &str, role: &str) -> String {
    let clean_reply = jyc_core::email_parser::strip_trailing_separators(text);
    if role.is_empty() || clean_reply.trim_start().starts_with(&format!("[{role}]")) {
        clean_reply
    } else {
        format!("[{role}] {clean_reply}")
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
/// Topics to close for a GitHub/Gitee close event, derived from config alone.
///
/// The routed topic name is a pure function of `pipe.topic` and the item
/// number, so re-rendering the template beats remembering what was routed:
/// the in-memory topic map is empty after a restart, and a close event for an
/// item routed before the restart would otherwise close nothing (#611).
///
/// Only number-dependent templates are considered. A static `pipe.topic`
/// collects many items into one shared topic, which must survive any single
/// item closing. `${msg.pr_number}` / `${msg.issue_number}` are type-gated
/// exactly as at routing time, so an issue close never resolves a PR topic.
/// `${msg.github_number}` / `${msg.gitee_number}` resolve for both hosts.
///
/// Returns `(topic, target_hub_channel)` pairs.
pub(super) fn close_event_topics(
    patterns: &[jyc_types::ChannelPattern],
    number: u64,
    github_type: &str,
    repo: &str,
) -> Vec<(String, String)> {
    patterns
        .iter()
        .filter(|p| p.enabled)
        .filter_map(|p| {
            let pipe = p.pipe.as_ref()?;
            // Same template resolution as apply_pipe_retarget: pipe.topic
            // wins, legacy pipe.pattern is the fallback.
            let template = pipe.topic.as_deref().or(pipe.pattern.as_deref())?;
            if !template.contains("${msg.") {
                return None;
            }
            let topic = resolve_placeholders_with(template, |key| match key {
                "github_number" | "gitee_number" => Some(number.to_string()),
                "pr_number" if github_type == "pull_request" => Some(number.to_string()),
                "issue_number" if github_type != "pull_request" => Some(number.to_string()),
                "repo" => Some(repo.to_string()),
                _ => None,
            })?;
            let hub = pipe
                .channel
                .clone()
                .or_else(|| pipe.agent.as_ref().map(|_| "agents".to_string()))?;
            Some((topic, hub))
        })
        .collect()
}

pub(super) fn apply_pipe_retarget(
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
pub(super) fn resolve_msg_placeholders(
    template: &str,
    msg: &jyc_types::InboundMessage,
) -> Option<String> {
    resolve_placeholders_with(template, |key| lookup_msg_placeholder(key, msg))
}

/// Core of `resolve_msg_placeholders` with the value source as a closure, so
/// close events (which have a number but no message) can render the same
/// `pipe.topic` templates.
pub(super) fn resolve_placeholders_with(
    template: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Option<String> {
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
        let raw = lookup(key)?;
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
///
/// Numeric metadata (e.g. GitHub `issue_number`/`pr_number`/
/// `github_number`, stored as JSON integers) is stringified — a
/// string-only lookup would silently fail to resolve and drop the
/// message as "unresolvable target".
pub(super) fn lookup_msg_placeholder(key: &str, msg: &jyc_types::InboundMessage) -> Option<String> {
    match msg.metadata.get(key) {
        Some(serde_json::Value::String(s)) if !s.is_empty() => return Some(s.clone()),
        Some(serde_json::Value::Number(n)) => return Some(n.to_string()),
        _ => {}
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
pub(super) struct ReplyAttachmentRef {
    filename: String,
    url_path: String,
    content_type: String,
}

/// Parse the optional `attachments` array of a reply broadcast
/// (`{"type":"reply","attachments":[{"filename","path","content_type"}]}`).
/// Malformed entries are skipped.
pub(super) fn parse_reply_attachments(v: &serde_json::Value) -> Vec<ReplyAttachmentRef> {
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

/// Download one reply attachment from the inspect server and send it to the
/// feishu chat (image vs. file chosen by content type).
pub(super) async fn relay_attachment(
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
pub(super) fn collect_pipe_target_channels(
    patterns: &[ChannelPattern],
) -> std::collections::HashSet<String> {
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

/// Hub channels a pipe-only adapter can route into, keyed by channel name.
///
/// Carries the `TopicManager` alongside the router because pipe-only adapters
/// own no workspace: routing needs the router, and close events (GitHub
/// issue/PR closed) need the hub's TopicManager.
pub(crate) type HubRegistry =
    std::sync::Arc<std::sync::Mutex<HashMap<String, (Arc<MessageRouter>, Arc<TopicManager>)>>>;

/// Validate that every enabled pattern names a pipe target; warn about
/// deprecated pipe fields and missing pipes (messages matching a
/// pipe-less pattern are dropped at runtime).
pub(super) fn warn_on_bad_pipe_patterns(
    channel_type: &str,
    channel_name: &str,
    channel_config: &ChannelConfig,
) {
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
                        "{channel_type} pipe.channel/pipe.pattern is deprecated; use pipe = {{ agent = \"...\", topic = \"...\" }}"
                    );
                }
            }
            None => tracing::warn!(
                channel = %channel_name,
                pattern = %p.name,
                "{channel_type} pattern has no pipe target; matching messages will be dropped"
            ),
        }
    }
}

/// Pipe-adapter step 1 (shared): match the message against this channel's
/// patterns and return the matched pattern plus the match details. The
/// pattern is guaranteed to carry a `pipe` target; mismatches and
/// pipe-less patterns are logged here and dropped.
pub(super) fn match_pipe<'a>(
    channel_type: &str,
    matcher: &dyn ChannelMatcher,
    message: &jyc_types::InboundMessage,
    patterns: &'a [ChannelPattern],
) -> Option<(jyc_types::PatternMatch, &'a ChannelPattern)> {
    let Some(pm) = matcher.match_message(message, patterns) else {
        tracing::debug!(
            channel_type,
            topic = %message.topic,
            "pipe: no pattern matched, dropping"
        );
        return None;
    };
    let matched = patterns
        .iter()
        .find(|p| p.name == pm.pattern_name)
        .filter(|p| p.pipe.is_some());
    let Some(pattern) = matched else {
        tracing::warn!(
            channel_type,
            pattern = %pm.pattern_name,
            "pipe: matched pattern has no pipe target, dropping message"
        );
        return None;
    };
    Some((pm, pattern))
}

/// Pipe-adapter step 2 (shared): retarget the message into the pipe's
/// target channel/topic, with standard drop logging on unresolvable
/// targets. Adapters with a custom middle step (reply-state capture,
/// topic defaulting) call this directly instead of `match_and_retarget`.
pub(super) fn retarget_or_drop(
    channel_type: &str,
    message: jyc_types::InboundMessage,
    pipe: &jyc_types::PipeTarget,
) -> Option<jyc_types::InboundMessage> {
    let drop_debug = (message.id.clone(), message.channel_uid.clone());
    let Some(message) = apply_pipe_retarget(message, pipe) else {
        tracing::warn!(
            channel_type,
            topic = ?pipe.topic,
            agent = ?pipe.agent,
            message_id = %drop_debug.0,
            channel_uid = %drop_debug.1,
            "pipe: unresolvable target (no topic configured, or ${{msg.<key>}} unresolved), dropping"
        );
        return None;
    };
    Some(message)
}

/// Match the message against this channel's patterns and retarget it into
/// the pattern's pipe. Returns the retargeted message and the pipe.
/// Composition of `match_pipe` + `retarget_or_drop` for adapters without a
/// custom middle step.
pub(super) fn match_and_retarget(
    channel_type: &str,
    matcher: &dyn ChannelMatcher,
    message: jyc_types::InboundMessage,
    patterns: &[ChannelPattern],
) -> Option<(jyc_types::InboundMessage, jyc_types::PipeTarget)> {
    let (_pm, pattern) = match_pipe(channel_type, matcher, &message, patterns)?;
    let pipe = pattern
        .pipe
        .as_ref()
        .expect("match_pipe guarantees a pipe target");
    let message = retarget_or_drop(channel_type, message, pipe)?;
    Some((message, pipe.clone()))
}

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
