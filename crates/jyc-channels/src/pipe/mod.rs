//! External pipe process support (`jyc-pipe`).
//!
//! A pipe process hosts peripheral channel adapters (feishu, github,
//! gitee, wecom_bot, wecom, wecomkf, email) that translate platform
//! events and forward messages to the hub over websocket — owning no
//! topics, no agents, no core state. See
//! `docs/architecture/pipe-split.md`.
//!
//! Submodules:
//! - [`hub`]: one websocket connection per target hub channel, with
//!   reconnect backoff and frame demux (`reply` / `topic_event`).
//! - one module per channel type — [`feishu`], [`github`], [`gitee`],
//!   [`wecom_bot`], [`wecom`] (`wecom` + `wecomkf`, which share one
//!   webhook listener), [`email`]: adapter wiring, reply relay,
//!   channel-specific events (`close_topic`, status cards, …).
//!
//! The pattern-matching / retarget helpers below are used by the pipe
//! adapters (and were shared with the in-process ones before the split
//! completed in step 5).

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use jyc_types::{ChannelConfig, ChannelMatcher, ChannelPattern};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub mod email;
pub mod feishu;
pub mod gitee;
pub mod github;
pub mod hub;
pub mod wecom;
pub mod wecom_bot;

pub use hub::{HubPipe, PipeTopicEvent};

/// Channel types the pipe process can run adapters for. Ownership is
/// derived from this list on both sides (`jyc serve` skips in-process
/// spawn for configured channels whose type appears here), so no config
/// section is needed to coordinate the two processes. The split is
/// complete: every pipe-capable channel type is listed here and the hub
/// spawns none of them.
pub const SUPPORTED_CHANNEL_TYPES: &[&str] = &[
    "feishu",
    "github",
    "gitee",
    "wecom_bot",
    "wecom",
    "wecomkf",
    "email",
];

/// Run the pipe process: claim every configured channel whose type this
/// process can run, build one hub pipe per pipe-target channel its
/// patterns route through, spawn the connection tasks and the channel
/// adapters, then wait for cancellation.
///
/// Returns `Err` when a hub pipe fails fatally (e.g. the auth token is
/// rejected), the shared WeCom webhook listener cannot bind its port, or
/// an email channel cannot initialize its mailbox cursor state — the
/// caller should exit non-zero.
pub async fn run(
    config: Arc<jyc_types::AppConfig>,
    workdir: &std::path::Path,
    ws_origin: &str,
    token: Option<String>,
    no_idle: bool,
    reset: bool,
    cancel: CancellationToken,
) -> Result<()> {
    // Claim configured channels by capability. The hub applies the same
    // rule to skip in-process spawn, so the two sides agree without any
    // `[pipe]` config section — both just read the same channel list.
    let claimed = select_channels(&config)?;

    // Hub attachment download: only when the inspect server is enabled.
    let files_base = config
        .inspect
        .as_ref()
        .filter(|i| i.enabled)
        .map(|i| format!("http://{}", loopback_addr(&i.bind)));

    let hubs: std::collections::HashMap<String, Arc<HubPipe>> = claimed
        .targets
        .iter()
        .map(|t| (t.clone(), HubPipe::new(t, ws_origin, token.clone())))
        .collect();
    let hubs = Arc::new(hubs);

    let mut pipes = JoinSet::new();
    for hub in hubs.values() {
        let hub = hub.clone();
        let cancel = cancel.clone();
        pipes.spawn(async move { hub.run(cancel).await });
    }

    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    for (name, channel_config) in claimed.feishu {
        feishu::spawn_feishu_pipe(
            &channel_config,
            name,
            config.clone(),
            hubs.clone(),
            files_base.clone(),
            token.clone(),
            cancel.clone(),
            &mut tasks,
        )?;
    }
    for (name, channel_config) in claimed.github {
        github::spawn_github_pipe(
            &channel_config,
            name,
            config.clone(),
            hubs.clone(),
            workdir,
            cancel.clone(),
            &mut tasks,
        )?;
    }
    for (name, channel_config) in claimed.gitee {
        gitee::spawn_gitee_pipe(
            &channel_config,
            name,
            config.clone(),
            hubs.clone(),
            workdir,
            cancel.clone(),
            &mut tasks,
        )?;
    }
    for (name, channel_config) in claimed.wecom_bot {
        wecom_bot::spawn_wecom_bot_pipe(
            &channel_config,
            name,
            config.clone(),
            hubs.clone(),
            files_base.clone(),
            token.clone(),
            cancel.clone(),
            &mut tasks,
        )?;
    }

    // The shared WeCom webhook listener (one server for `wecom` and
    // `wecomkf`) is bound by the pipe now — the hub no longer starts it —
    // and only when one of the two channels is claimed. Binding failure is
    // fatal: every callback would 404 otherwise.
    if !claimed.wecom.is_empty() || !claimed.wecomkf.is_empty() {
        let server = wecom::start_webhook_server(&config, cancel.clone()).await?;
        for (name, channel_config) in claimed.wecom {
            wecom::spawn_wecom_pipe(
                &channel_config,
                name,
                config.clone(),
                hubs.clone(),
                server.clone(),
                cancel.clone(),
                &mut tasks,
            )?;
        }
        for (name, channel_config) in claimed.wecomkf {
            wecom::spawn_wecomkf_pipe(
                &channel_config,
                name,
                config.clone(),
                hubs.clone(),
                server.clone(),
                cancel.clone(),
                &mut tasks,
            )?;
        }
    }

    for (name, channel_config) in claimed.email {
        email::spawn_email_pipe(
            &channel_config,
            name,
            workdir,
            config.clone(),
            hubs.clone(),
            files_base.clone(),
            token.clone(),
            no_idle,
            reset,
            cancel.clone(),
            &mut tasks,
        )
        .await?;
    }

    tokio::select! {
        _ = cancel.cancelled() => Ok(()),
        next = pipes.join_next() => match next {
            Some(Ok(Ok(()))) => Err(anyhow::anyhow!("hub pipe connection task exited unexpectedly")),
            Some(Ok(Err(e))) => Err(e).context("hub pipe failed"),
            Some(Err(join_err)) => Err(join_err).context("hub pipe task panicked"),
            None => Ok(()),
        },
    }
}

/// Channels claimed by the pipe process: the distinct hub targets their
/// patterns route through, plus the channel configs to spawn, grouped by
/// adapter type.
#[derive(Debug)]
struct ClaimedChannels {
    targets: HashSet<String>,
    feishu: Vec<(String, ChannelConfig)>,
    github: Vec<(String, ChannelConfig)>,
    gitee: Vec<(String, ChannelConfig)>,
    wecom_bot: Vec<(String, ChannelConfig)>,
    wecom: Vec<(String, ChannelConfig)>,
    wecomkf: Vec<(String, ChannelConfig)>,
    email: Vec<(String, ChannelConfig)>,
}

/// Select the configured channels this process owns: every channel whose
/// type appears in [`SUPPORTED_CHANNEL_TYPES`]. Returns the distinct hub
/// targets their patterns route through plus the channel configs to
/// spawn. Errors when no configured channel is supported — running the
/// pipe process would do nothing.
fn select_channels(config: &jyc_types::AppConfig) -> Result<ClaimedChannels> {
    let mut claimed = ClaimedChannels {
        targets: HashSet::new(),
        feishu: Vec::new(),
        github: Vec::new(),
        gitee: Vec::new(),
        wecom_bot: Vec::new(),
        wecom: Vec::new(),
        wecomkf: Vec::new(),
        email: Vec::new(),
    };
    for (name, channel_config) in &config.channels {
        if !SUPPORTED_CHANNEL_TYPES.contains(&channel_config.channel_type.as_str()) {
            tracing::info!(
                channel = %name,
                channel_type = %channel_config.channel_type,
                "channel type not supported by jyc-pipe — leaving to hub in-process spawn"
            );
            continue;
        }
        match channel_config.channel_type.as_str() {
            "feishu" => claimed.feishu.push((name.clone(), channel_config.clone())),
            "github" => claimed.github.push((name.clone(), channel_config.clone())),
            "gitee" => claimed.gitee.push((name.clone(), channel_config.clone())),
            "wecom_bot" => claimed
                .wecom_bot
                .push((name.clone(), channel_config.clone())),
            "wecom" => claimed.wecom.push((name.clone(), channel_config.clone())),
            "wecomkf" => claimed.wecomkf.push((name.clone(), channel_config.clone())),
            "email" => claimed.email.push((name.clone(), channel_config.clone())),
            other => anyhow::bail!(
                "type '{other}' is in SUPPORTED_CHANNEL_TYPES but has no adapter in jyc-pipe"
            ),
        }
        claimed.targets.extend(collect_pipe_target_channels(
            channel_config.patterns.as_deref().unwrap_or(&[]),
        ));
    }
    let claimed_count = claimed.feishu.len()
        + claimed.github.len()
        + claimed.gitee.len()
        + claimed.wecom_bot.len()
        + claimed.wecom.len()
        + claimed.wecomkf.len()
        + claimed.email.len();
    if claimed_count == 0 {
        anyhow::bail!(
            "no configured channels supported by jyc-pipe (supports: {}) — nothing to run",
            SUPPORTED_CHANNEL_TYPES.join(", ")
        );
    }
    Ok(claimed)
}

/// Loopback address for calls to the hub's inspect server: a wildcard bind
/// (`0.0.0.0`, `[::]`) is not a connectable destination.
pub fn loopback_addr(bind: &str) -> String {
    bind.replace("0.0.0.0", "127.0.0.1")
        .replace("[::]", "127.0.0.1")
}

/// Collect the distinct hub-channel targets that a pipe channel's patterns
/// route through — one pipe connection (and reply relay) per entry.
///
/// Two forms accepted per pattern:
/// - legacy `pipe = { channel = "x" }` → inserts "x"
/// - new `pipe = { agent = "x", topic = "..." }` → inserts "agents"
///   (the synthesized channel name)
pub fn collect_pipe_target_channels(patterns: &[ChannelPattern]) -> HashSet<String> {
    let mut out = HashSet::new();
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

/// Validate that every enabled pattern names a pipe target; warn about
/// deprecated pipe fields and missing pipes (messages matching a
/// pipe-less pattern are dropped at runtime).
pub fn warn_on_bad_pipe_patterns(
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

/// Strip trailing separators and prefix a reply with its `[Role]` header
/// (skipped when the reply already carries it). Used by the github pipe
/// reply forwarder; gitee joins in step 4.
pub fn role_prefixed_body(text: &str, role: &str) -> String {
    let clean_reply = jyc_core::email_parser::strip_trailing_separators(text);
    if role.is_empty() || clean_reply.trim_start().starts_with(&format!("[{role}]")) {
        clean_reply
    } else {
        format!("[{role}] {clean_reply}")
    }
}

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
pub fn close_event_topics(
    patterns: &[ChannelPattern],
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

/// Pipe-adapter step 1 (shared): match the message against this channel's
/// patterns and return the matched pattern plus the match details. The
/// pattern is guaranteed to carry a `pipe` target; mismatches and
/// pipe-less patterns are logged here and dropped.
pub fn match_pipe<'a>(
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
/// targets.
pub fn retarget_or_drop(
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
pub fn match_and_retarget(
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

/// Record the channel a message actually arrived on just before it is
/// re-targeted into another channel's topic, so the agent turn can still
/// tell who it is talking to. `ask_user` looks that name up among the
/// target channel's registered adapters to decide whether a question box
/// can be drawn where the user is — see [`jyc_types::ORIGIN_CHANNEL_METADATA_KEY`].
fn stamp_origin_channel(msg: &mut jyc_types::InboundMessage) {
    msg.metadata.insert(
        jyc_types::ORIGIN_CHANNEL_METADATA_KEY.to_string(),
        serde_json::Value::String(msg.channel.clone()),
    );
}

pub fn apply_pipe_retarget(
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
        stamp_origin_channel(&mut msg);
        msg.channel = "agents".to_string();
        msg.topic = topic;
        return Some(msg);
    }

    // Legacy form.
    // (Deprecation warning fires once at startup in the spawn wiring,
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
    stamp_origin_channel(&mut msg);
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
pub fn resolve_msg_placeholders(template: &str, msg: &jyc_types::InboundMessage) -> Option<String> {
    resolve_placeholders_with(template, |key| lookup_msg_placeholder(key, msg))
}

/// Core of `resolve_msg_placeholders` with the value source as a closure, so
/// close events (which have a number but no message) can render the same
/// `pipe.topic` templates.
pub fn resolve_placeholders_with(
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
pub fn lookup_msg_placeholder(key: &str, msg: &jyc_types::InboundMessage) -> Option<String> {
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
pub struct ReplyAttachmentRef {
    pub filename: String,
    pub url_path: String,
    pub content_type: String,
}

/// Parse the optional `attachments` array of a reply broadcast
/// (`{"type":"reply","attachments":[{"filename","path","content_type"}]}`).
/// Malformed entries are skipped.
pub fn parse_reply_attachments(v: &serde_json::Value) -> Vec<ReplyAttachmentRef> {
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

/// Download one reply attachment from the hub's files endpoint through the
/// pipe's bearer token, spool it to a temp file, and apply the outbound
/// attachment policy to it.
///
/// `att.url_path` is the relative URL from the reply broadcast's
/// `attachments[].path` (leading slash included, percent-encoded). The
/// caller uploads the returned file with its own channel client.
pub(crate) async fn fetch_topic_file(
    files_base: &str,
    token: Option<&str>,
    att: &ReplyAttachmentRef,
    config: &jyc_types::AppConfig,
) -> Result<tempfile::NamedTempFile> {
    let mut req = reqwest::Client::new().get(format!("{files_base}{}", att.url_path));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let bytes = req
        .send()
        .await
        .context("failed to download topic file")?
        .error_for_status()
        .context("topic file request returned an error status")?
        .bytes()
        .await
        .context("failed to read topic file body")?;

    let tmp = tempfile::NamedTempFile::new()?;
    tokio::fs::write(tmp.path(), &bytes).await?;
    if let Some(cfg) = config.attachments.as_ref().and_then(|a| a.outbound.clone()) {
        jyc_utils::attachment_validator::validate_outbound_file(tmp.path(), &att.filename, &cfg)
            .await?;
    }
    Ok(tmp)
}

#[cfg(test)]
mod tests;
