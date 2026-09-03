//! Channel adapter construction for `jyc serve`.
//!
//! Extracted from the monolithic `serve.rs` run() function.

use anyhow::Context;
use serde_json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use anyhow::Result;

use jyc_channels::email::inbound::EmailMatcher;
use jyc_channels::feishu::client::FeishuClient;
use jyc_channels::feishu::inbound::{FeishuInboundAdapter, FeishuMatcher};
use jyc_channels::github::inbound::GithubMatcher;
use jyc_channels::websocket::inbound::{WebsocketInboundAdapter, WebsocketMatcher};

use jyc_channels::wecom::kf_client::KfApiClient;
use jyc_channels::wecom::kf_cursor::KfCursorStore;
use jyc_channels::wecom::kf_dedup::KfDedupStore;
use jyc_channels::wecom::server::WecomWebhookServer;
use jyc_channels::wecom::token_cache::AccessTokenCache;
use jyc_channels::wecom_bot::inbound::{WecomBotInboundAdapter, WecomBotMatcher};
use jyc_core::channel_orchestrator::ChannelOrchestrator;
use jyc_core::duration::{DurationStyle, format_duration_secs};
use jyc_core::message_router::MessageRouter;
use jyc_core::state_manager::StateManager;
use jyc_core::topic_manager::TopicManager;
use jyc_services::imap::monitor::ImapMonitor;
use jyc_types::{
    ChannelConfig, ChannelInfo, ChannelMatcher, ChannelPattern, InboundAdapter,
    InboundAttachmentConfig, MonitorConfig,
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

/// Strip trailing separators and prefix a reply with its `[Role]` header
/// (skipped when the reply already carries it). Shared by the GitHub and
/// Gitee pipe reply forwarders.
fn role_prefixed_body(text: &str, role: &str) -> String {
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
fn close_event_topics(
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
    resolve_placeholders_with(template, |key| lookup_msg_placeholder(key, msg))
}

/// Core of `resolve_msg_placeholders` with the value source as a closure, so
/// close events (which have a number but no message) can render the same
/// `pipe.topic` templates.
fn resolve_placeholders_with(
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
fn lookup_msg_placeholder(key: &str, msg: &jyc_types::InboundMessage) -> Option<String> {
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

/// Hub channels a pipe-only adapter can route into, keyed by channel name.
///
/// Carries the `TopicManager` alongside the router because pipe-only adapters
/// own no workspace: routing needs the router, and close events (GitHub
/// issue/PR closed) need the hub's TopicManager.
pub(crate) type HubRegistry =
    std::sync::Arc<std::sync::Mutex<HashMap<String, (Arc<MessageRouter>, Arc<TopicManager>)>>>;

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
/// Mirrors `spawn_feishu_adapter` (see `docs/architecture/overview.md`).
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
    routers: HubRegistry,
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
    warn_on_bad_pipe_patterns("email", &channel_name, channel_config);

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
                        let patterns = config_for_pipe
                            .load()
                            .channels
                            .get(&channel_name_self)
                            .and_then(|c| c.patterns.clone())
                            .unwrap_or_default();
                        let Some((pm, pattern)) =
                            match_pipe("email", &EmailMatcher, &message, &patterns)
                        else {
                            return;
                        };
                        let pipe = pattern
                            .pipe
                            .as_ref()
                            .expect("match_pipe guarantees a pipe target");

                        // Reply state, captured before re-targeting
                        // rewrites channel/topic.
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

                        // Re-target into the target channel/topic. Without
                        // an explicit `pipe.topic`, the pattern's
                        // `topic_name` override wins, else the
                        // subject-derived topic name (same precedence the
                        // MessageRouter applied before the migration).
                        // The derived name also replaces `message.topic`
                        // (the parse-time subject, already `Re:`/`Fw:`
                        // stripped but not pattern-prefix stripped or
                        // sanitized) so `${msg.topic}` in a template
                        // resolves to the same name the no-template path
                        // would use. `apply_pipe_retarget` overwrites
                        // `message.topic` with the resolved template anyway.
                        let derived_topic = pattern.topic_name.clone().unwrap_or_else(|| {
                            EmailMatcher.derive_topic_name(&message, &patterns, Some(&pm))
                        });
                        let pipe = email_pipe_with_topic(pipe, &derived_topic);
                        message.topic = derived_topic;
                        let Some(message) = retarget_or_drop("email", message, &pipe) else {
                            return;
                        };

                        // Record resolved topic -> reply state.
                        topic_state
                            .lock()
                            .unwrap()
                            .insert(message.topic.clone(), reply_state);

                        // Route through the target's own MessageRouter —
                        // the same path as a chat-pane message.
                        route_into_pipe_target("email", &routers, &pipe, message).await;
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

/// Spawn a pipe-only GitHub adapter: the poller inbound adapter plus one
/// reply forwarder per distinct pipe target channel.
///
/// Mirrors spawn_email_adapter. Unlike full channels, owns no
/// TopicManager/agent/orchestrator — all topics live in the pipe target
/// (hub) channel. Keeps a GithubInboundAdapter under
/// <workdir>/channels/<channel>/.github/ for dedup/cursor state.
///
/// Differences from the old full-channel architecture:
/// - No template/metadata injection (initialization is a skill on the agent side).
/// - No shared-repo directory grouping (that feature was removed).
/// - Comments carry the [Role] prefix (GPT summarizer, GitHub reviewer, etc.)
///   but no model/mode/token footer.
/// - Close events (issue/PR closed) use the hub registry's TopicManager to
///   close the routed topics in the hub workspace.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_github_adapter(
    channel_config: &ChannelConfig,
    channel_name: String,
    workdir: &Path,
    inbound_attachment_config: Option<InboundAttachmentConfig>,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
    config_for_spawn: std::sync::Arc<arc_swap::ArcSwap<jyc_types::AppConfig>>,
    ws_broadcasts: std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
    routers: HubRegistry,
) -> Result<()> {
    use jyc_channels::github::inbound::GithubInboundAdapter;

    let github_config = channel_config
        .github
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing github config"))?
        .clone();

    // Pipe-only validation: every enabled pattern must have a pipe target.
    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("github", &channel_name, channel_config);

    // State (dedup, cursor) lives under <workdir>/channels/<channel>/.github/.
    // One-time rename migration from the old location.
    let old_state_dir = workdir.join(&channel_name).join(".github");
    let new_state_dir = workdir.join("channels").join(&channel_name).join(".github");
    if old_state_dir.exists() && !new_state_dir.exists() {
        if let Some(parent) = new_state_dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::rename(&old_state_dir, &new_state_dir) {
            tracing::warn!(
                from = %old_state_dir.display(),
                to = %new_state_dir.display(),
                error = %e,
                "github state dir migration failed (dedup will start fresh)"
            );
        }
    }

    // Build the client before spawning: an unusable token (invalid header bytes)
    // must fail startup, not panic a detached task and leave a silently dead channel.
    let client = Arc::new(
        jyc_channels::github::client::GithubClient::new(&github_config)
            .with_context(|| format!("github client for channel '{channel_name}'"))?,
    );

    let channel_span = tracing::info_span!("in", ch = %channel_name);
    let workdir_for_task = workdir.to_path_buf();
    let channel_name_for_task = channel_name.clone();
    let cancel_child = cancel.child_token();

    let task = tokio::spawn(
        async move {
            // Shared topic -> reply state for pipe relaying.
            let topic_state: std::sync::Arc<
                std::sync::Mutex<HashMap<String, (u64, String) /* number, role */>>,
            > = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

            // One reply forwarder per distinct pipe target channel.
            for channel in &pipe_channels {
                let ws_broadcasts = ws_broadcasts.clone();
                let topic_state = topic_state.clone();
                let client = client.clone();
                let channel = channel.clone();
                tokio::spawn(async move {
                    let Some(broadcast_tx) = wait_for_broadcast(&ws_broadcasts, &channel).await
                    else {
                        tracing::error!(
                            channel = %channel,
                            "github pipe: target channel broadcast never appeared (is it a websocket channel?), reply forwarder not started"
                        );
                        return;
                    };
                    let mut rx = broadcast_tx.subscribe();
                    tracing::info!(channel = %channel, "github pipe reply forwarder subscribed");
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
                        let Some((number, role)) =
                            topic_state.lock().unwrap().get(topic).cloned()
                        else {
                            tracing::debug!(
                                topic = %topic,
                                "github pipe: no number mapping for reply, skipping"
                            );
                            continue;
                        };

                        // Build comment body: [Role] prefix, no footer
                        let body = role_prefixed_body(text, &role);

                        // Post comment via GitHub API (attachments not supported)
                        if let Err(e) = client.create_comment(number, &body).await {
                            tracing::error!(
                                error = format!("{e:#}"),
                                topic = %topic,
                                number = number,
                                "github pipe: failed to relay reply"
                            );
                        }
                    }
                });
            }

            // Hub TopicManager lookup for close events.
            let routers = routers.clone();

            // Inbound adapter: poller + pattern matching + pipe retarget.
            // Passing <workdir>/channels makes the adapter compute its
            // state_dir as <workdir>/channels/<channel>/.github (same
            // convention as the email adapter's StateManager).
            let adapter = GithubInboundAdapter::new(
                &github_config,
                channel_name_for_task.clone(),
                &workdir_for_task.join("channels"),
                Some(config_for_spawn.clone()),
            );
            // The close handler needs its own handles (on_message takes ownership).
            let topic_state_for_close = topic_state.clone();
            let routers_for_close = routers.clone();
            let config_for_close = config_for_spawn.clone();
            let channel_name_for_close = channel_name_for_task.clone();
            let repo_for_close = github_config.repo.clone();
            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let config_for_pipe = config_for_spawn.clone();
                    let topic_state = topic_state.clone();
                    let channel_name_self = channel_name_for_task.clone();
                    let routers = routers.clone();
                    tokio::spawn(async move {
                        let patterns = config_for_pipe
                            .load()
                            .channels
                            .get(&channel_name_self)
                            .and_then(|c| c.patterns.clone())
                            .unwrap_or_default();
                        let Some((_pm, pattern)) =
                            match_pipe("github", &GithubMatcher, &message, &patterns)
                        else {
                            return;
                        };
                        let pipe = pattern
                            .pipe
                            .as_ref()
                            .expect("match_pipe guarantees a pipe target");

                        // Capture number and role for reply routing. Without a
                        // number a reply could not be addressed, so drop loudly
                        // rather than commenting on issue #0.
                        let Some(number) = message
                            .metadata
                            .get("github_number")
                            .and_then(|v| v.as_u64())
                        else {
                            tracing::warn!(
                                message_id = %message.id,
                                "github: message has no github_number metadata, dropping"
                            );
                            return;
                        };
                        let role = pattern.role.as_deref().unwrap_or("").to_string();

                        // Re-target into the target channel/topic.
                        let Some(message) = retarget_or_drop("github", message, pipe) else {
                            return;
                        };

                        // Record resolved topic -> (number, role).
                        topic_state
                            .lock()
                            .unwrap()
                            .insert(message.topic.clone(), (number, role));

                        // Route through the target's own MessageRouter.
                        route_into_pipe_target("github", &routers, pipe, message).await;
                    });
                    Ok(())
                }),
                on_topic_close: None,
                on_close_event: Some(Box::new(move |number: u64, github_type: &str| {
                    let topic_state = topic_state_for_close.clone();
                    let routers = routers_for_close.clone();
                    // Derive the routed topics from config (restart-proof) and
                    // union with whatever this process actually routed (covers
                    // topics whose template changed since routing).
                    let patterns = config_for_close
                        .load()
                        .channels
                        .get(&channel_name_for_close)
                        .and_then(|c| c.patterns.clone())
                        .unwrap_or_default();
                    let mut targets =
                        close_event_topics(&patterns, number, github_type, &repo_for_close);
                    let github_type = github_type.to_string();
                    tokio::spawn(async move {
                        // Collect out of the std mutexes before awaiting:
                        // their guards are not Send.
                        {
                            let state = topic_state.lock().unwrap();
                            for topic in state
                                .iter()
                                .filter(|(_, v)| v.0 == number)
                                .map(|(t, _)| t.clone())
                            {
                                if !targets.iter().any(|(t, _)| *t == topic) {
                                    // Hub unknown for remembered topics: try every hub.
                                    targets.push((topic, String::new()));
                                }
                            }
                        }
                        if targets.is_empty() {
                            tracing::info!(
                                number = number,
                                github_type = %github_type,
                                "github pipe: close event resolved no topics (no pipe pattern with a number-dependent topic template)"
                            );
                            return;
                        }
                        let hubs: Vec<(String, Arc<TopicManager>)> = {
                            let reg = routers.lock().unwrap();
                            reg.iter()
                                .map(|(name, (_, tm))| (name.clone(), tm.clone()))
                                .collect()
                        };
                        for (topic, target_hub) in &targets {
                            for (hub_name, tm) in &hubs {
                                if !target_hub.is_empty() && hub_name != target_hub {
                                    continue;
                                }
                                if let Err(e) = tm.auto_close_topic(topic).await {
                                    tracing::debug!(
                                        hub = %hub_name,
                                        topic = %topic,
                                        number = number,
                                        error = %e,
                                        "github pipe: auto_close_topic ignored (no such topic in this hub)"
                                    );
                                }
                            }
                        }
                        topic_state.lock().unwrap().retain(|_, v| v.0 != number);
                    });
                })),
                on_error: Box::new(|error| {
                    tracing::error!(error = %error, "GitHub inbound error");
                }),
                attachment_config: inbound_attachment_config.clone(),
            };

            if let Err(e) = adapter.start(options, cancel_child).await {
                tracing::error!(error = %e, "GitHub inbound adapter error");
            }
        }
        .instrument(channel_span),
    );
    tasks.push(task);
    Ok(())
}

/// Reply routing state for the Gitee pipe forwarder, keyed by resolved topic.
/// Gitee keeps issues and PRs in separate number spaces, so the reply needs
/// both the number and the item type (`create_comment` takes an explicit
/// `is_pr` flag), and a close event only removes same-type entries.
#[derive(Debug, Clone)]
struct GiteeReplyState {
    /// Gitee issue/PR number (string, the format the API expects).
    number: String,
    /// Matched pattern role (e.g. "Planner"), rendered as a `[Role]` prefix.
    role: String,
    /// Whether the item is a pull request (vs an issue).
    is_pr: bool,
}

/// Spawn a pipe-only Gitee adapter: the poller inbound adapter plus one
/// reply forwarder per distinct pipe target channel.
///
/// Mirrors `spawn_github_adapter`. Owns no TopicManager/agent/orchestrator —
/// all topics live in the pipe target (hub) channel. Keeps a
/// GiteeInboundAdapter under `<workdir>/channels/<channel>/.gitee/` for
/// dedup/cursor state. Unlike GitHub, Gitee uses separate number spaces for
/// issues and PRs, so the topic→reply map also records whether the item is a
/// PR (`create_comment` needs the explicit `is_pr` flag).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_gitee_adapter(
    channel_config: &ChannelConfig,
    channel_name: String,
    workdir: &Path,
    inbound_attachment_config: Option<InboundAttachmentConfig>,
    cancel: CancellationToken,
    tasks: &mut Vec<JoinHandle<()>>,
    config_for_spawn: std::sync::Arc<arc_swap::ArcSwap<jyc_types::AppConfig>>,
    ws_broadcasts: std::sync::Arc<std::sync::Mutex<HashMap<String, broadcast::Sender<String>>>>,
    routers: HubRegistry,
) -> Result<()> {
    use jyc_channels::gitee::inbound::GiteeInboundAdapter;
    use jyc_channels::gitee::inbound::GiteeMatcher;

    let gitee_config = channel_config
        .gitee
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_name}': missing gitee config"))?
        .clone();

    // Pipe-only validation: every enabled pattern must have a pipe target.
    let pipe_channels =
        collect_pipe_target_channels(channel_config.patterns.as_deref().unwrap_or(&[]));
    warn_on_bad_pipe_patterns("gitee", &channel_name, channel_config);

    // State (dedup, cursor) lives under <workdir>/channels/<channel>/.gitee/.
    // One-time rename migration from the old location.
    let old_state_dir = workdir.join(&channel_name).join(".gitee");
    let new_state_dir = workdir.join("channels").join(&channel_name).join(".gitee");
    if old_state_dir.exists() && !new_state_dir.exists() {
        if let Some(parent) = new_state_dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::rename(&old_state_dir, &new_state_dir) {
            tracing::warn!(
                from = %old_state_dir.display(),
                to = %new_state_dir.display(),
                error = %e,
                "gitee state dir migration failed (dedup will start fresh)"
            );
        }
    }

    // Build the client before spawning: an unusable token (invalid header bytes)
    // must fail startup, not panic a detached task and leave a silently dead channel.
    let client = Arc::new(
        jyc_channels::gitee::client::GiteeClient::new(&gitee_config)
            .with_context(|| format!("gitee client for channel '{channel_name}'"))?,
    );

    let channel_span = tracing::info_span!("in", ch = %channel_name);
    let workdir_for_task = workdir.to_path_buf();
    let channel_name_for_task = channel_name.clone();
    let cancel_child = cancel.child_token();

    let task = tokio::spawn(
        async move {
            // Shared topic -> reply state for pipe relaying.
            // Gitee keeps issues and PRs in separate number spaces, so the
            // reply state carries both the number and the item type.
            let topic_state: std::sync::Arc<
                std::sync::Mutex<HashMap<String, GiteeReplyState>>,
            > = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

            // One reply forwarder per distinct pipe target channel.
            for channel in &pipe_channels {
                let ws_broadcasts = ws_broadcasts.clone();
                let topic_state = topic_state.clone();
                let client = client.clone();
                let channel = channel.clone();
                tokio::spawn(async move {
                    let Some(broadcast_tx) = wait_for_broadcast(&ws_broadcasts, &channel).await
                    else {
                        tracing::error!(
                            channel = %channel,
                            "gitee pipe: target channel broadcast never appeared (is it a websocket channel?), reply forwarder not started"
                        );
                        return;
                    };
                    let mut rx = broadcast_tx.subscribe();
                    tracing::info!(channel = %channel, "gitee pipe reply forwarder subscribed");
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
                        let Some(GiteeReplyState {
                            number,
                            role,
                            is_pr,
                        }) = topic_state.lock().unwrap().get(topic).cloned()
                        else {
                            tracing::debug!(
                                topic = %topic,
                                "gitee pipe: no number mapping for reply, skipping"
                            );
                            continue;
                        };

                        // Build comment body: [Role] prefix, no footer
                        let body = role_prefixed_body(text, &role);

                        // Post comment via Gitee API (attachments not supported)
                        if let Err(e) = client.create_comment(&number, &body, is_pr).await {
                            tracing::error!(
                                error = format!("{e:#}"),
                                topic = %topic,
                                number = %number,
                                "gitee pipe: failed to relay reply"
                            );
                        }
                    }
                });
            }

            // Hub TopicManager lookup for close events.
            let routers = routers.clone();

            // Inbound adapter: poller + pattern matching + pipe retarget.
            // Passing <workdir>/channels makes the adapter compute its
            // state_dir as <workdir>/channels/<channel>/.gitee (same
            // convention as the email adapter's StateManager).
            let adapter = GiteeInboundAdapter::new(
                &gitee_config,
                channel_name_for_task.clone(),
                &workdir_for_task.join("channels"),
            );
            // The close handler needs its own handles (on_message takes ownership).
            let topic_state_for_close = topic_state.clone();
            let routers_for_close = routers.clone();
            let config_for_close = config_for_spawn.clone();
            let channel_name_for_close = channel_name_for_task.clone();
            let repo_for_close = gitee_config.repo.clone();
            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let config_for_pipe = config_for_spawn.clone();
                    let topic_state = topic_state.clone();
                    let channel_name_self = channel_name_for_task.clone();
                    let routers = routers.clone();
                    tokio::spawn(async move {
                        let patterns = config_for_pipe
                            .load()
                            .channels
                            .get(&channel_name_self)
                            .and_then(|c| c.patterns.clone())
                            .unwrap_or_default();
                        let Some((_pm, pattern)) =
                            match_pipe("gitee", &GiteeMatcher, &message, &patterns)
                        else {
                            return;
                        };
                        let pipe = pattern
                            .pipe
                            .as_ref()
                            .expect("match_pipe guarantees a pipe target");

                        // Capture number/type and role for reply routing.
                        let Some(number) = message
                            .metadata
                            .get("gitee_number")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                        else {
                            tracing::warn!(
                                message_id = %message.id,
                                "gitee: message has no gitee_number metadata, dropping"
                            );
                            return;
                        };
                        let is_pr = message
                            .metadata
                            .get("gitee_type")
                            .and_then(|v| v.as_str())
                            == Some("pull_request");
                        let role = pattern.role.as_deref().unwrap_or("").to_string();

                        // Re-target into the target channel/topic.
                        let Some(message) = retarget_or_drop("gitee", message, pipe) else {
                            return;
                        };

                        // Record resolved topic -> (number, role, is_pr).
                        topic_state.lock().unwrap().insert(
                            message.topic.clone(),
                            GiteeReplyState {
                                number,
                                role,
                                is_pr,
                            },
                        );

                        // Route through the target's own MessageRouter.
                        route_into_pipe_target("gitee", &routers, pipe, message).await;
                    });
                    Ok(())
                }),
                on_topic_close: None,
                on_close_event: Some(Box::new(move |number: u64, gitee_type: &str| {
                    let topic_state = topic_state_for_close.clone();
                    let routers = routers_for_close.clone();
                    // Derive the routed topics from config (restart-proof) and
                    // union with whatever this process actually routed (covers
                    // topics whose template changed since routing).
                    let patterns = config_for_close
                        .load()
                        .channels
                        .get(&channel_name_for_close)
                        .and_then(|c| c.patterns.clone())
                        .unwrap_or_default();
                    let mut targets =
                        close_event_topics(&patterns, number, gitee_type, &repo_for_close);
                    let gitee_type = gitee_type.to_string();
                    tokio::spawn(async move {
                        // Collect out of the std mutexes before awaiting:
                        // their guards are not Send.
                        {
                            let state = topic_state.lock().unwrap();
                            for topic in state
                                .iter()
                                .filter(|(_, v)| {
                                    v.number == number.to_string()
                                        && v.is_pr == (gitee_type == "pull_request")
                                })
                                .map(|(t, _)| t.clone())
                            {
                                if !targets.iter().any(|(t, _)| *t == topic) {
                                    // Hub unknown for remembered topics: try every hub.
                                    targets.push((topic, String::new()));
                                }
                            }
                        }
                        if targets.is_empty() {
                            tracing::info!(
                                number = number,
                                gitee_type = %gitee_type,
                                "gitee pipe: close event resolved no topics (no pipe pattern with a number-dependent topic template)"
                            );
                            return;
                        }
                        let hubs: Vec<(String, Arc<TopicManager>)> = {
                            let reg = routers.lock().unwrap();
                            reg.iter()
                                .map(|(name, (_, tm))| (name.clone(), tm.clone()))
                                .collect()
                        };
                        for (topic, target_hub) in &targets {
                            for (hub_name, tm) in &hubs {
                                if !target_hub.is_empty() && hub_name != target_hub {
                                    continue;
                                }
                                if let Err(e) = tm.auto_close_topic(topic).await {
                                    tracing::debug!(
                                        hub = %hub_name,
                                        topic = %topic,
                                        number = number,
                                        error = %e,
                                        "gitee pipe: auto_close_topic ignored (no such topic in this hub)"
                                    );
                                }
                            }
                        }
                        topic_state.lock().unwrap().retain(|_, v| {
                            // Only purge same-type entries: Gitee keeps
                            // separate number spaces, so closing issue #5
                            // must not erase the PR #5 reply mapping.
                            !(v.number == number.to_string() && v.is_pr == (gitee_type == "pull_request"))
                        });
                    });
                })),
                on_error: Box::new(|error| {
                    tracing::error!(error = %error, "Gitee inbound error");
                }),
                attachment_config: inbound_attachment_config.clone(),
            };

            if let Err(e) = adapter.start(options, cancel_child).await {
                tracing::error!(error = %e, "Gitee inbound adapter error");
            }
        }
        .instrument(channel_span),
    );
    tasks.push(task);
    Ok(())
}

/// Spawn a pipe-only feishu adapter: the inbound adapter plus one reply
/// forwarder per distinct pipe target channel.
///
/// Unlike full channels, a feishu adapter has no outbound adapter, agent
/// service, TopicManager, StateManager, or orchestrator registration — all
/// topics live in the pipe target (hub) channel. See
/// `docs/architecture/overview.md`.
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
    routers: HubRegistry,
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
    warn_on_bad_pipe_patterns("feishu", &channel_name, channel_config);

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
            // Per-topic start times, used for the reply footer
            // ("⏱ 耗时 <elapsed>") and the live status card. Entries live from
            // inbound until the topic is closed (chat disband) or
            // overwritten by the next message. In-memory only; lost on
            // restart (status cards stay frozen until cleared manually).
            let topic_starts: std::sync::Arc<
                std::sync::Mutex<HashMap<String, std::time::Instant>>,
            > = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

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
                let topic_starts = topic_starts.clone();
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
                        let send_result = {
                            // Completion footer: every relayed reply carries
                            // the elapsed time since the indicator started —
                            // useful for both the final reply and mid-run
                            // progress replies.
                            let elapsed = topic_starts
                                .lock()
                                .unwrap()
                                .get(topic)
                                .map(|s| s.elapsed().as_secs());
                            let text = match elapsed {
                                Some(s) => format!(
                                    "{text}\n\n⏱ 耗时 {}",
                                    format_duration_secs(s, DurationStyle::Precise)
                                ),
                                None => text.to_string(),
                            };
                            feishu_client.send_text_message(&chat_id, &text).await
                        };
                        if let Err(e) = &send_result {
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

            let topic_chat_for_close = topic_chat.clone();
            let topic_starts_for_close = topic_starts.clone();
            let routers_for_close = routers.clone();

            let options = jyc_types::InboundAdapterOptions {
                on_message: Box::new(move |message| {
                    let config_for_pipe = config_for_spawn.clone();
                    let topic_chat = topic_chat.clone();
                    let topic_starts = topic_starts.clone();
                    let feishu_client = feishu_client.clone();
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
                            match_pipe("feishu", &FeishuMatcher, &message, &patterns)
                        else {
                            return;
                        };
                        let pipe = pattern
                            .pipe
                            .as_ref()
                            .expect("match_pipe guarantees a pipe target");

                        // Re-target into the target channel/topic —
                        //    resolves the effective topic (`topic ?? pattern`)
                        //    and `${msg.chat_name}` placeholders against
                        //    message metadata.
                        let Some(message) = retarget_or_drop("feishu", message, pipe) else {
                            return;
                        };

                        // Record resolved topic -> chat_id for reply relay.
                        let chat_id = message
                            .metadata
                            .get("chat_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        if let Some(chat_id) = &chat_id {
                            topic_chat
                                .lock()
                                .unwrap()
                                .insert(message.topic.clone(), chat_id.clone());
                        }

                        // Progress indicator (best-effort): the watcher
                        // sends a live status card in the chat once
                        // processing actually starts, then updates it from
                        // the topic's event bus. The footer timing is
                        // recorded unconditionally.
                        if let Some(cid) = &chat_id {
                            let start = std::time::Instant::now();
                            // Event freshness cutoff for the watcher — taken
                            // here, before routing, so no event of this run
                            // can predate it.
                            let seen_after = chrono::Utc::now();
                            let hub_tm = {
                                let reg = routers.lock().unwrap();
                                reg.get(&message.channel).map(|(_, tm)| tm.clone())
                            };
                            if let Some(tm) = hub_tm {
                                jyc_channels::feishu::progress::spawn_progress_watcher(
                                    feishu_client.clone(),
                                    tm,
                                    message.topic.clone(),
                                    cid.clone(),
                                    start,
                                    seen_after,
                                );
                            }
                            topic_starts
                                .lock()
                                .unwrap()
                                .insert(message.topic.clone(), start);
                        }

                        // Route through the target's own MessageRouter — the
                        // exact same path as a chat-pane message, so
                        // topic_path/template/skills apply identically.
                        route_into_pipe_target("feishu", &routers, pipe, message).await;
                    });
                    Ok(())
                }),
                on_topic_close: Some(Box::new(move |chat_id: String| {
                    let topic_chat = topic_chat_for_close.clone();
                    let topic_starts = topic_starts_for_close.clone();
                    let routers = routers_for_close.clone();
                    tokio::spawn(async move {
                        // Reverse-lookup: the disband event carries the
                        // upstream chat_id; the map records topic → chat_id.
                        let topics_to_close: Vec<String> = {
                            let map = topic_chat.lock().unwrap();
                            map.iter()
                                .filter(|(_, v)| v.as_str() == chat_id)
                                .map(|(t, _)| t.clone())
                                .collect()
                        };
                        if topics_to_close.is_empty() {
                            return;
                        }
                        // Collect out of both std mutexes before awaiting:
                        // their guards are not Send.
                        let hubs: Vec<(String, Arc<TopicManager>)> = {
                            let reg = routers.lock().unwrap();
                            reg.iter()
                                .map(|(name, (_, tm))| (name.clone(), tm.clone()))
                                .collect()
                        };
                        for topic in &topics_to_close {
                            for (hub_name, tm) in &hubs {
                                if let Err(e) = tm.auto_close_topic(topic).await {
                                    tracing::debug!(
                                        hub = %hub_name,
                                        topic = %topic,
                                        chat_id = %chat_id,
                                        error = %e,
                                        "feishu pipe: auto_close_topic ignored (no such topic in this hub)"
                                    );
                                }
                            }
                        }
                        topic_chat.lock().unwrap().retain(|_, v| v != &chat_id);
                        for topic in &topics_to_close {
                            topic_starts.lock().unwrap().remove(topic);
                        }
                    });
                    Ok(())
                })),
                on_close_event: None,
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

/// Validate that every enabled pattern names a pipe target; warn about
/// deprecated pipe fields and missing pipes (messages matching a
/// pipe-less pattern are dropped at runtime).
fn warn_on_bad_pipe_patterns(
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
fn match_pipe<'a>(
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
fn retarget_or_drop(
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
fn match_and_retarget(
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
async fn route_into_pipe_target(
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

    /// Numeric metadata resolves in `${msg.<key>}` templates: the GitHub
    /// adapter stores `issue_number`/`pr_number` as JSON integers (the
    /// matcher consumes them via `as_u64()`), so a string-only lookup
    /// would fail and drop the message as "unresolvable target". Mirrors
    /// the documented `topic = "plan-${msg.issue_number}"` config.
    #[test]
    fn pipe_retarget_resolves_numeric_metadata_placeholder() {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("issue_number".to_string(), serde_json::json!(605));
        let pipe = agent_pipe_target("jyc_git_planner", Some("plan-${msg.issue_number}"));
        let out = apply_pipe_retarget(pipe_msg(metadata), &pipe).unwrap();
        assert_eq!(out.topic, "plan-605");
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

    // ---- close_event_topics ----

    /// Regression for #611: a close event must resolve its topics from config,
    /// not from the in-memory routing map (empty after a restart). Type-gated
    /// placeholders keep an issue close from touching PR topics.
    #[test]
    fn close_event_topics_renders_number_templates() {
        let patterns = vec![
            pattern_with(agent_pipe_target(
                "jyc_git_planner",
                Some("plan-${msg.issue_number}"),
            )),
            pattern_with(agent_pipe_target("jyc_git", Some("dev-${msg.pr_number}"))),
        ];

        // Issue close → only the issue_number template resolves.
        assert_eq!(
            close_event_topics(&patterns, 607, "issue", "jyc"),
            vec![("plan-607".to_string(), "agents".to_string())]
        );
        // PR close → only the pr_number template resolves.
        assert_eq!(
            close_event_topics(&patterns, 609, "pull_request", "jyc"),
            vec![("dev-609".to_string(), "agents".to_string())]
        );
    }

    /// A static `pipe.topic` collects many items into one shared topic, so
    /// closing one item must never delete it. Disabled patterns are ignored.
    #[test]
    fn close_event_topics_skips_static_and_disabled() {
        let patterns = vec![
            pattern_with(agent_pipe_target("jyc_git", Some("shared-inbox"))),
            ChannelPattern {
                name: "disabled".to_string(),
                enabled: false,
                pipe: Some(agent_pipe_target(
                    "jyc_git",
                    Some("plan-${msg.issue_number}"),
                )),
                ..Default::default()
            },
        ];
        assert!(close_event_topics(&patterns, 607, "issue", "jyc").is_empty());
    }

    /// `${msg.repo}` disambiguates two channels piping into one agent, and the
    /// legacy `pipe.channel` form resolves to that channel as the hub.
    #[test]
    fn close_event_topics_repo_placeholder_and_legacy_channel() {
        let patterns = vec![pattern_with(pipe_target(
            None,
            Some("review-${msg.repo}-${msg.pr_number}"),
        ))];
        assert_eq!(
            close_event_topics(&patterns, 42, "pull_request", "jyc"),
            vec![("review-jyc-42".to_string(), "local_dev".to_string())]
        );
    }

    /// The legacy form may carry the template in `pipe.pattern` when
    /// `pipe.topic` is absent — mirror apply_pipe_retarget's fallback.
    #[test]
    fn close_event_topics_legacy_pattern_template_fallback() {
        let patterns = vec![pattern_with(pipe_target(
            Some("dev-${msg.pr_number}"),
            None,
        ))];
        assert_eq!(
            close_event_topics(&patterns, 609, "pull_request", "jyc"),
            vec![("dev-609".to_string(), "local_dev".to_string())]
        );
    }

    /// Gitee templates use `${msg.gitee_number}` (same semantics as
    /// `${msg.github_number}`): a close event must resolve it.
    #[test]
    fn close_event_topics_resolves_gitee_number() {
        let patterns = vec![pattern_with(agent_pipe_target(
            "jyc_git",
            Some("gitee-${msg.gitee_number}"),
        ))];
        assert_eq!(
            close_event_topics(&patterns, 42, "issue", "jyc"),
            vec![("gitee-42".to_string(), "agents".to_string())]
        );
    }
}
