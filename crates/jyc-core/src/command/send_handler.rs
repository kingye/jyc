//! What every send to another agent shares: resolving the addressee, and the
//! message that lands in its queue.
//!
//! `/send` (the command) and `jyc_send_to_topic` (the tool) are one
//! implementation with two triggers, exactly as `/spawn` and
//! `jyc_spawn_subagent` are — the command parses its arguments into a
//! [`SendRequest`], the tool its JSON input, and both call
//! [`send_to_topic`].
//!
//! Addressing is by **agent**, not by channel: a topic is identified by its
//! name, and the manager that serves it is looked up
//! ([`TopicManagers::serving`]) rather than named by the caller. An agent's own
//! topic lives in the agents channel, which is where `/spawn` and the dashboard
//! put agents. A name no topic owns is refused rather than created: a send
//! reaches topics that exist, it does not make them.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use jyc_types::{InboundMessage, MessageAttachment, MessageContent, PatternMatch};

use super::flags::{SEND_FLAGS, SendFlag};
use super::handler::{CommandContext, CommandHandler, CommandResult};
use super::spec;
use crate::topic_manager::{TopicManager, TopicManagers};
use crate::topic_path::{topic_home, topic_work_dir};

/// The channel that holds agents' own topics: `/spawn` and the dashboard put
/// every agent there, and a message addressed to an agent lands on the topic
/// named after it.
pub const AGENTS_CHANNEL: &str = "agents";

/// Where a send comes from — carried into the message so the target can reply
/// back to the right place.
pub struct SendSource<'a> {
    /// Channel the sender runs in.
    pub channel: Option<&'a str>,
    /// Topic the sender runs in.
    pub topic: Option<&'a str>,
    /// The sender's working directory: attachments are resolved against it and
    /// must stay inside it.
    pub working_dir: &'a Path,
}

/// What to send, once the addressee is named.
pub struct SendRequest<'a> {
    /// The agent to send to: the name of the topic to deliver to — that agent's
    /// own topic, unless `topic` names another. The name must already exist:
    /// a send never creates a topic.
    pub agent: &'a str,
    /// One of that agent's topics, when it has more than one.
    pub topic: Option<&'a str>,
    /// Message body.
    pub message: &'a str,
    /// Filenames inside [`SendSource::working_dir`] to attach.
    pub attachments: &'a [String],
    /// Recipient address/ID for the injected message, enabling
    /// channel-appropriate reply routing.
    pub recipient: Option<&'a str>,
    /// Mark the message as expecting a reply from the target agent.
    pub require_reply: bool,
}

/// Send `req.message` to the agent named in `req`, through the one manager that
/// serves it.
///
/// Returns the user-facing confirmation, or the reason nothing was sent — the
/// address is unresolvable, the message empty, an attachment missing.
pub async fn send_to_topic(
    managers: &TopicManagers,
    source: SendSource<'_>,
    req: SendRequest<'_>,
) -> Result<String> {
    if req.agent.trim().is_empty() {
        bail!("Agent cannot be empty");
    }
    if req.message.trim().is_empty() {
        bail!("Message cannot be empty");
    }
    let (topic_name, named_topic) = match req.topic {
        Some(topic) if !topic.trim().is_empty() => (topic, Some(topic)),
        Some(_) => bail!("Topic cannot be empty"),
        None => (req.agent, None),
    };
    // The names are also path components — they name the topic's home under the
    // agents dir, and resolution probes that home — so path syntax is refused
    // before either is joined into a path. Only path syntax: a topic name comes
    // from the channel layer, whose alphabet is much wider than `/spawn`'s
    // (`sanitize_for_filesystem` only replaces separators and a few more), and a
    // sender address or a subject line is exactly the topic a reply must reach.
    // `/spawn` is where a name is created, and is held to the stricter rule
    // there.
    for name in [Some(req.agent), named_topic].into_iter().flatten() {
        if let Some(reason) = invalid_address_name(name) {
            bail!("Invalid address '{name}': {reason}");
        }
    }

    let target_tm = resolve(managers, req.agent, named_topic).await?;

    let validated_attachments = validate_attachments(req.attachments, source.working_dir)?;

    // Build InboundMessage with source metadata.
    let mut inbound = InboundMessage {
        id: uuid::Uuid::new_v4().to_string(),
        channel: target_tm.channel_name().to_string(),
        channel_uid: format!("jyc-send-to-topic-{}", uuid::Uuid::new_v4()),
        sender: "Agent".to_string(),
        sender_address: req.recipient.unwrap_or("agent@jyc").to_string(),
        recipients: vec![],
        topic: topic_name.to_string(),
        content: MessageContent {
            text: Some(req.message.to_string()),
            html: None,
            markdown: None,
        },
        timestamp: chrono::Utc::now(),
        references: None,
        reply_to_id: None,
        external_id: None,
        attachments: validated_attachments
            .iter()
            .map(|filename| build_attachment(filename, source.working_dir))
            .collect(),
        metadata: {
            let mut m = HashMap::new();
            if let Some(src_ch) = source.channel {
                m.insert(
                    "source_channel".to_string(),
                    serde_json::Value::String(src_ch.to_string()),
                );
            }
            if let Some(src_th) = source.topic {
                m.insert(
                    "source_topic".to_string(),
                    serde_json::Value::String(src_th.to_string()),
                );
            }
            m.insert(
                "require_reply".to_string(),
                serde_json::Value::Bool(req.require_reply),
            );
            m
        },
        matched_pattern: None,
    };

    // Enqueue the message into the target topic. Resolve the pattern named
    // after the topic (when one exists) so injected messages carry the same
    // pattern identity — name, template/role metadata, attachment config,
    // live_injection, custom topic_path — as router-matched messages (mirrors
    // MessageRouter, #542).
    let pattern = target_tm.pattern_for_topic(topic_name);
    let (pattern_name, attachment_config, live_injection, topic_path_override) = match &pattern {
        Some(p) => {
            if let Some(ref template) = p.template {
                inbound.metadata.insert(
                    "template".to_string(),
                    serde_json::Value::String(template.clone()),
                );
            }
            if let Some(ref role) = p.role {
                inbound
                    .metadata
                    .insert("role".to_string(), serde_json::Value::String(role.clone()));
            }
            (
                p.name.clone(),
                p.attachments.clone(),
                p.live_injection(),
                p.topic_path
                    .as_ref()
                    .map(|tp| crate::topic_path::resolve_topic_path(tp, target_tm.data_root()))
                    // Mirror MessageRouter: an agent topic without a pinned
                    // topic_path has its home under the agent root
                    // (`<agents-workspace>/<agent>/<topic>`, or the agent root
                    // for the self-named topic) and works in that home's
                    // `.data`. Only the self-named topic is resolvable here
                    // (that is what `pattern_for_topic` matches); other agent
                    // topics already carry a registered path from their first
                    // routed message.
                    .or_else(|| {
                        (target_tm.channel_name() == AGENTS_CHANNEL).then(|| {
                            crate::topic_path::topic_work_dir(
                                target_tm.workspace_dir(),
                                &p.name,
                                topic_name,
                            )
                        })
                    }),
            )
        }
        None => (String::new(), None, true, None),
    };
    let pattern_match = PatternMatch {
        pattern_name,
        channel: target_tm.channel_name().to_string(),
        matches: HashMap::new(),
    };

    target_tm
        .enqueue(
            inbound,
            topic_name.to_string(),
            pattern_match,
            attachment_config,
            live_injection,
            topic_path_override,
        )
        .await;

    tracing::info!(
        target_channel = %target_tm.channel_name(),
        target_topic = %topic_name,
        attachment_count = validated_attachments.len(),
        require_reply = req.require_reply,
        "Message sent to agent topic"
    );

    // Name what actually happened. Without `topic`, the name was matched
    // against topic names first, so it landed on the agent's own topic only
    // when it resolved to the agents channel; name the channel otherwise,
    // rather than claiming an ownership the code never checked. A `topic`
    // equal to the agent names that agent's own topic, so it follows the
    // same branch as the `topic`-less form.
    let sent_to = match req.topic {
        Some(topic) if topic != req.agent => format!("topic '{topic}'"),
        _ if target_tm.channel_name() == AGENTS_CHANNEL => {
            format!("agent '{}' (its own topic)", req.agent)
        }
        _ => format!(
            "topic '{topic_name}' of channel '{}'",
            target_tm.channel_name()
        ),
    };
    let attachment_info = if validated_attachments.is_empty() {
        String::new()
    } else {
        format!(" with {} attachment(s)", validated_attachments.len())
    };
    let reply_info = if req.require_reply {
        " (reply requested)"
    } else {
        ""
    };

    Ok(format!(
        "Message sent to {sent_to}{attachment_info}{reply_info}. The target topic will process it."
    ))
}

/// Find the manager that serves the addressee.
///
/// A topic's name *is* its identity, so the only question is which manager runs
/// it — and it is answered by looking, never by trusting a caller-supplied
/// channel:
///
/// 1. a manager running its queue ([`TopicManagers::serving`]);
/// 2. otherwise the channel the topic's own state records, which is what keeps
///    a topic idle since the last restart reachable instead of it having to
///    wait for its next inbound message;
/// 3. otherwise the topic's own home, for one that exists but has never been
///    routed to ([`agent_topic`]) — `/spawn` makes a topic, and the task that
///    follows has to be able to reach it;
/// 4. otherwise the name is refused: no topic owns it.
///
/// A send never creates the topic it addresses. A topic is made by the thing
/// that routes to it — `/spawn` for an agent, a channel rule for a channel
/// topic — and one created here would be a name no rule owns, under a home
/// that collides with the names that do (state dirs are keyed by topic name
/// process-wide).
///
/// A `topic` equal to the `agent` names that agent's own topic: an agent's
/// own topic is named after it (`AGENTS_CHANNEL`), so `agent` and `topic`
/// carrying the same name is the default topic, not a named one — and it is
/// refused like any other name nobody owns.
async fn resolve(
    managers: &TopicManagers,
    agent: &str,
    topic: Option<&str>,
) -> Result<Arc<TopicManager>> {
    let topic_name = topic.unwrap_or(agent);
    if let Some(tm) = managers.serving(topic_name).await {
        return Ok(tm);
    }
    if let Some(channel) = recorded_channel(managers, agent, topic_name).await {
        return managers.by_channel(&channel).ok_or_else(|| {
            anyhow!("Topic '{topic_name}' belongs to channel '{channel}', which is not running")
        });
    }
    // A topic that exists but has never been routed to: `/spawn` made it a
    // moment ago, and nothing has sent it a message yet, so it has neither a
    // queue nor a routing record. It is an agent's own topic, so the agents
    // channel serves it.
    if let Some(tm) = agent_topic(managers, topic_name) {
        return Ok(tm);
    }
    // No topic owns this name — refuse it. Creating one here would hand out a
    // name no rule owns: the agent's own topic is made by `/spawn`, a channel
    // topic by the rule that routes to it.
    bail!(
        "No topic named '{topic_name}' exists: no topic by that name is running or registered in \
         a state dir, or the directory it names is gone. Create it first — `/spawn` or \
         `jyc_spawn_subagent` for an agent, the channel rule that routes to it for a channel \
         topic, or the dashboard's create-topic call — then send."
    );
}

/// The agents channel's manager, when `topic_name` is an agent's own topic
/// that exists but has never been routed to.
///
/// `/spawn` registers the child's state and pins its directory, but a topic
/// only gets a queue — and with it the routing record the worker writes on the
/// first message — once something sends to it. Between those two moments the
/// topic is there and unreachable, which is exactly the window a delegation
/// lives in: spawn, then send the task.
///
/// What tells it apart from a name nobody owns is its home. An agent-keyed
/// topic owns `<agents dir>/<name>/` and the agents channel serves it; a
/// pipe-routed one owns `<agents dir>/<agent>/<topic>/` and belongs to the
/// channel that routed it, and an ad-hoc pin's home is named after the path
/// rather than the topic — neither is ours to answer for.
///
/// A closed topic keeps its registration but loses its directory, so the
/// directory is what says the topic is still there.
fn agent_topic(managers: &TopicManagers, topic_name: &str) -> Option<Arc<TopicManager>> {
    let agents = managers.by_channel(AGENTS_CHANNEL)?;
    let state_dir = jyc_types::state_dir::registered_state(topic_name)?;
    if !state_dir.is_dir() {
        return None;
    }
    // The state dir of an agent-keyed topic: `<agents dir>/<name>/.jyc`.
    if state_dir != agents.workspace_dir().join(topic_name).join(".jyc") {
        return None;
    }
    Some(agents)
}

/// Refuse a name that is path syntax: the topic's home is built by joining
/// these names into the agents dir, and a resolution that probed outside it
/// would read a record the caller never meant to reach.
///
/// Path syntax only. A topic the channel layer named — a sender address
/// (`john.doe@example.com`, the default `pipe` topic), a subject line, emoji —
/// is a legitimate name and must stay addressable, so the stricter
/// `spawn_common::invalid_name` rule is applied where a name is *created*, not
/// where one is addressed.
///
/// Those four shapes are also the containment argument: joined as a path
/// component, an empty name, `.` or `..` resolves against the agents dir's
/// parent, and a separator makes the join absolute. A Windows prefix-only name
/// (`C:`, `c:foo`) would clear the base as well, but no channel can produce one
/// — `sanitize_for_filesystem` replaces `:` — and it cannot be a real directory
/// there, so the alphabet is left alone rather than widened for a non-case.
fn invalid_address_name(name: &str) -> Option<&'static str> {
    if name.is_empty() || name == "." || name == ".." {
        return Some("the name is not a usable path component");
    }
    if name.contains('/') || name.contains('\\') {
        return Some("the name must not contain a path separator");
    }
    None
}

/// The channel a topic's own state records it was routed by, written into
/// `.jyc/topic-meta.json` on its first message.
///
/// Which directory to look in is asked in two steps. The state registry answers
/// for the topics it was populated with at startup, and it only walks one level
/// under the agents dir — a sub-topic's home is two (`<agents dir>/<agent>/
/// <topic>`), and startup only registers the ones it sees. So the home is also
/// probed directly from the two names — the caller refuses path syntax in them
/// before they are joined, so the home computed here stays under the agents dir
/// (a `topic-path` record can still point elsewhere once a topic owns one, but
/// then the registry answers first) — which covers a sub-topic idle since the
/// last restart.
///
/// `None` for a topic that has never been routed to. Note the recorded name is
/// a *hub process* manager name: a topic created through the deprecated pipe
/// form records a channel this process does not run, and resolution refuses
/// rather than guessing.
async fn recorded_channel(
    managers: &TopicManagers,
    agent: &str,
    topic_name: &str,
) -> Option<String> {
    let state_dir = match jyc_types::state_dir::registered_state(topic_name) {
        Some(state_dir) => state_dir,
        None => {
            let agents = managers.by_channel(AGENTS_CHANNEL)?;
            // `topic_work_dir` answers with the directory the topic *works* in
            // (`<home>/.data` under the current layout), so the state dir is
            // its home's — `topic_home` maps it back, and treats the older
            // layouts (where the working dir was the home) as their own.
            let work_dir = topic_work_dir(agents.workspace_dir(), agent, topic_name);
            topic_home(&work_dir).join(".jyc")
        }
    };
    let meta_path = state_dir.join("topic-meta.json");
    let raw = match tokio::fs::read_to_string(&meta_path).await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::debug!(
                error = %e,
                path = %meta_path.display(),
                topic = %topic_name,
                "No topic-meta.json to read a routing channel from"
            );
            return None;
        }
    };
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(meta) => meta.get("channel")?.as_str().map(str::to_string),
        Err(e) => {
            tracing::debug!(
                error = %e,
                topic = %topic_name,
                "topic-meta.json is not readable JSON; treating the routing channel as unknown"
            );
            None
        }
    }
}

/// Check that each attachment exists inside `working_dir`, returning the
/// filenames that may be sent.
fn validate_attachments(filenames: &[String], working_dir: &Path) -> Result<Vec<String>> {
    let mut valid = Vec::with_capacity(filenames.len());
    for filename in filenames {
        let file_path = working_dir.join(filename);
        if !file_path.exists() {
            bail!("Attachment not found: '{filename}'");
        }
        // Security: ensure within working directory.
        if let Ok(canonical) = file_path.canonicalize() {
            let working_canonical = working_dir
                .canonicalize()
                .unwrap_or_else(|_| working_dir.to_path_buf());
            if !canonical.starts_with(&working_canonical) {
                bail!("Attachment '{filename}' is outside the working directory");
            }
        }
        valid.push(filename.clone());
    }
    Ok(valid)
}

/// Read one validated attachment into the message.
fn build_attachment(filename: &str, working_dir: &Path) -> MessageAttachment {
    let file_path = working_dir.join(filename);
    let size = std::fs::metadata(&file_path)
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    MessageAttachment {
        filename: filename.to_string(),
        content_type: "application/octet-stream".to_string(),
        size,
        content: std::fs::read(&file_path).ok(),
        saved_path: None,
        staged_path: None,
    }
}

/// `/send` command — send a message to another agent, or to one of its topics.
///
/// The command form of `jyc_send_to_topic`: same implementation
/// ([`send_to_topic`]), arguments instead of JSON.
pub struct SendCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl SendCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }
}

/// The arguments a `/send` line carries, once parsed.
#[derive(Debug, PartialEq)]
struct SendArgs {
    agent: String,
    topic: Option<String>,
    message: String,
    attachments: Vec<String>,
    recipient: Option<String>,
    require_reply: bool,
}

/// Usage line, shared by every refusal so the syntax is stated once — read off
/// `/send`'s own row instead of kept as a copy beside it.
fn usage() -> String {
    super::builtin::usage_of("/send")
}

/// Parse `/send` arguments: the first bare word is the agent, the flags follow
/// it, and from the first message word on everything — dashes included — is
/// message text. `--` ends the flags explicitly, for a message that starts
/// with one.
///
/// The shape is the one `/send`'s row declares: one positional, then text. Which
/// word is which therefore comes from the declaration, so a message can never be
/// read as a flag — and a flag can never be read as message text.
fn parse(args: &[String]) -> Result<SendArgs, String> {
    let mut parsed = spec::parse_command("/send", SEND_FLAGS, args)
        .map_err(|error| format!("{error}. {}", usage()))?;
    let agent = parsed
        .positionals
        .first()
        .cloned()
        .ok_or_else(|| format!("Missing agent. {}", usage()))?;
    let message = unquote(parsed.body.take().unwrap_or_default());
    if message.trim().is_empty() {
        return Err(format!("Missing message. {}", usage()));
    }
    Ok(SendArgs {
        agent,
        topic: parsed.value(SendFlag::Topic).map(str::to_string),
        message,
        // Every `-a`, in the order given: attaching several files is the point
        // of the flag.
        attachments: parsed
            .flags
            .iter()
            .filter(|(flag, _)| *flag == SendFlag::Attach)
            .filter_map(|(_, value)| value.clone())
            .collect(),
        recipient: parsed.value(SendFlag::Recipient).map(str::to_string),
        require_reply: parsed.has(SendFlag::Reply),
    })
}

/// Drop one pair of matching quotes around the whole message, so
/// `/send joe "/close --force"` arrives as `/close --force` — the same thing
/// the unquoted form now sends. Only a pair around the entire message counts;
/// inner quotes are left alone.
fn unquote(message: String) -> String {
    let bytes = message.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return message[1..message.len() - 1].to_string();
        }
    }
    message
}

#[async_trait]
impl CommandHandler for SendCommandHandler {
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let parsed = match parse(&context.args) {
            Ok(parsed) => parsed,
            Err(message) => return Ok(refusal(message)),
        };
        let Some(managers) = self.topic_manager.managers() else {
            return Ok(refusal(
                "No topic managers available: this topic is not wired to the channels it could send to"
                    .to_string(),
            ));
        };

        let source = SendSource {
            channel: (!context.channel.is_empty()).then_some(context.channel.as_str()),
            topic: (!context.topic_name.is_empty()).then_some(context.topic_name.as_str()),
            working_dir: &context.topic_path,
        };
        let req = SendRequest {
            agent: &parsed.agent,
            topic: parsed.topic.as_deref(),
            message: &parsed.message,
            attachments: &parsed.attachments,
            recipient: parsed.recipient.as_deref(),
            require_reply: parsed.require_reply,
        };

        match send_to_topic(&managers, source, req).await {
            Ok(message) => Ok(CommandResult {
                success: true,
                message,
                error: None,
                append_body: None,
            }),
            Err(e) => Ok(refusal(e.to_string())),
        }
    }
}

/// A refusal: nothing was sent, and why (see [`CommandResult::refusal`] for why
/// the text goes in both fields).
fn refusal(message: String) -> CommandResult {
    CommandResult::refusal(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_storage::MessageStorage;
    use crate::metrics::MetricsCollector;
    use crate::static_agent::StaticAgentService;
    use arc_swap::ArcSwap;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn parse_reads_the_agent_then_the_message() {
        let parsed = parse(&args(&["invoice", "hello", "there"])).unwrap();
        assert_eq!(parsed.agent, "invoice");
        assert_eq!(parsed.message, "hello there");
        assert_eq!(parsed.topic, None);
        assert!(!parsed.require_reply);
    }

    #[test]
    fn parse_reads_flags_before_the_message() {
        let parsed = parse(&args(&[
            "--reply",
            "invoice",
            "-t",
            "probe-served-topic",
            "-a",
            "report.pdf",
            "hello",
        ]))
        .unwrap();
        assert_eq!(parsed.agent, "invoice");
        assert_eq!(parsed.topic.as_deref(), Some("probe-served-topic"));
        assert_eq!(parsed.attachments, vec!["report.pdf".to_string()]);
        assert_eq!(parsed.message, "hello");
        assert!(parsed.require_reply);
    }

    #[test]
    fn parse_keeps_dashes_after_the_first_message_word() {
        let parsed = parse(&args(&["ci-watch-917", "/close", "--force"])).unwrap();
        assert_eq!(parsed.agent, "ci-watch-917");
        assert_eq!(parsed.message, "/close --force");
    }

    #[test]
    fn parse_takes_double_dash_as_the_start_of_the_message() {
        let parsed = parse(&args(&["invoice", "--", "--force", "hi"])).unwrap();
        assert_eq!(parsed.message, "--force hi");
        assert!(
            parse(&args(&["invoice", "--"]))
                .unwrap_err()
                .contains("Missing message")
        );
    }

    #[test]
    fn parse_drops_surrounding_quotes_from_the_message() {
        // One token, and the two tokens a real command line hands over.
        for words in [
            vec!["invoice", "\"/close --force\""],
            vec!["invoice", "\"/close", "--force\""],
        ] {
            let parsed = parse(&args(&words)).unwrap();
            assert_eq!(parsed.message, "/close --force", "{words:?}");
        }
        let parsed = parse(&args(&["invoice", "'/close --force'"])).unwrap();
        assert_eq!(parsed.message, "/close --force");
        // An unpaired quote is not a pair, so it stays part of the message.
        let parsed = parse(&args(&["invoice", "\"hello"])).unwrap();
        assert_eq!(parsed.message, "\"hello");
    }

    #[test]
    fn parse_keeps_a_lone_dash_as_message_text() {
        let parsed = parse(&args(&["invoice", "-"])).unwrap();
        assert_eq!(parsed.message, "-");
    }

    #[test]
    fn parse_refuses_a_missing_addressee_or_message() {
        assert!(parse(&args(&[])).unwrap_err().contains("Missing agent"));
        assert!(
            parse(&args(&["invoice"]))
                .unwrap_err()
                .contains("Missing message")
        );
        assert!(
            parse(&args(&["invoice", "--topic"]))
                .unwrap_err()
                .contains("needs a value")
        );
        assert!(
            parse(&args(&["invoice", "--nope", "hi"]))
                .unwrap_err()
                .contains("unknown flag --nope")
        );
    }

    fn source<'a>(working_dir: &'a Path) -> SendSource<'a> {
        SendSource {
            channel: Some("agents"),
            topic: Some("parent"),
            working_dir,
        }
    }

    fn request<'a>(agent: &'a str, topic: Option<&'a str>, message: &'a str) -> SendRequest<'a> {
        SendRequest {
            agent,
            topic,
            message,
            attachments: &[],
            recipient: None,
            require_reply: false,
        }
    }

    /// Registers a topic's state dir for the duration of a test.
    ///
    /// The registry is process-global, so a name registered here would leak
    /// into whichever test runs next — tests in this file therefore use names
    /// no other test can pick, and this guard removes the entry on the way out
    /// (including on panic).
    struct RegisteredState(String);

    impl Drop for RegisteredState {
        fn drop(&mut self) {
            jyc_types::state_dir::unregister(&self.0);
        }
    }

    fn register_state(topic: &str, state_dir: &Path) -> RegisteredState {
        jyc_types::state_dir::register(topic, state_dir);
        RegisteredState(topic.to_string())
    }

    /// Writes the routing record a topic's first message leaves behind.
    fn write_topic_meta(state_dir: &Path, channel: &str) {
        std::fs::create_dir_all(state_dir).unwrap();
        std::fs::write(
            state_dir.join("topic-meta.json"),
            serde_json::json!({ "channel": channel, "channel_uid": "seed" }).to_string(),
        )
        .unwrap();
    }

    /// A routed message is what makes a topic exist: give `tm` one.
    async fn seed_topic(tm: &Arc<TopicManager>, channel: &str, topic: &str) {
        tm.enqueue(
            jyc_types::InboundMessage {
                id: "seed".to_string(),
                channel: channel.to_string(),
                channel_uid: "seed".to_string(),
                sender: "user".to_string(),
                sender_address: "user@example.com".to_string(),
                recipients: vec![],
                topic: topic.to_string(),
                content: jyc_types::MessageContent {
                    text: Some("seed".to_string()),
                    html: None,
                    markdown: None,
                },
                timestamp: chrono::Utc::now(),
                references: None,
                reply_to_id: None,
                external_id: None,
                attachments: vec![],
                metadata: HashMap::new(),
                matched_pattern: None,
            },
            topic.to_string(),
            PatternMatch {
                pattern_name: "invoice".to_string(),
                channel: channel.to_string(),
                matches: HashMap::new(),
            },
            None,
            false,
            None,
        )
        .await;
    }

    /// An agent's own topic is served by the agents channel, whether or not it
    /// has been active before — as long as it exists: a send reaches topics
    /// that exist, it does not make them.
    #[tokio::test]
    async fn sends_to_an_agents_channel_topic_that_exists() {
        let tmp = tempdir().unwrap();
        let agents = make_tm(&tmp.path().join("agents-ws"), AGENTS_CHANNEL);
        let managers = TopicManagers::new();
        managers.set(vec![agents.clone()]);
        let _cleanup = RegisteredState("probe-agent-default".to_string());

        seed_topic(&agents, AGENTS_CHANNEL, "probe-agent-default").await;

        let sent = send_to_topic(
            &managers,
            source(tmp.path()),
            request("probe-agent-default", None, "hello"),
        )
        .await
        .unwrap();

        assert!(sent.contains("agent 'probe-agent-default'"), "{sent}");
        assert!(agents.serves_topic("probe-agent-default").await);
    }

    /// A topic `/spawn` just made: its state is registered and its home is an
    /// agent's own home under the agents dir, but nothing has routed a message
    /// to it yet — no queue, no routing record. It is a topic, so the task
    /// that follows the spawn reaches it.
    #[tokio::test]
    async fn sends_to_a_spawned_topic_that_has_never_been_routed_to() {
        let tmp = tempdir().unwrap();
        let agents = make_tm(tmp.path(), AGENTS_CHANNEL);
        let managers = TopicManagers::new();
        managers.set(vec![agents.clone()]);

        // What `/spawn` leaves behind: the child's state registered under its
        // name, in the home it owns under the agents dir.
        let home = tmp.path().join("probe-spawned-child");
        std::fs::create_dir_all(home.join(".jyc")).unwrap();
        let _cleanup = register_state("probe-spawned-child", &home.join(".jyc"));

        let sent = send_to_topic(
            &managers,
            source(tmp.path()),
            request("probe-spawned-child", None, "hello"),
        )
        .await
        .unwrap();

        assert!(sent.contains("agent 'probe-spawned-child'"), "{sent}");
    }

    /// A closed child keeps its registration but loses its directory, so it is
    /// refused like a name nobody owns.
    #[tokio::test]
    async fn refuses_a_closed_child_whose_home_is_gone() {
        let tmp = tempdir().unwrap();
        let managers = TopicManagers::new();
        managers.set(vec![make_tm(tmp.path(), AGENTS_CHANNEL)]);

        let home = tmp.path().join("probe-closed-child");
        std::fs::create_dir_all(home.join(".jyc")).unwrap();
        let _cleanup = register_state("probe-closed-child", &home.join(".jyc"));
        // The close took the home with it.
        std::fs::remove_dir_all(&home).unwrap();

        let err = send_to_topic(
            &managers,
            source(tmp.path()),
            request("probe-closed-child", None, "hello"),
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("No topic named 'probe-closed-child'"),
            "{err}"
        );
    }

    /// A name no topic owns is refused, and nothing is created for it: the
    /// send says the topic is missing and names the way to make it.
    #[tokio::test]
    async fn refuses_an_agent_no_topic_owns() {
        let tmp = tempdir().unwrap();
        let managers = TopicManagers::new();
        managers.set(vec![make_tm(tmp.path(), AGENTS_CHANNEL)]);

        let err = send_to_topic(
            &managers,
            source(tmp.path()),
            request("probe-agent-missing", None, "hello"),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("probe-agent-missing"), "{err}");
        assert!(err.to_string().contains("Create it first"), "{err}");
        let agents = managers.by_channel(AGENTS_CHANNEL).unwrap();
        assert!(!agents.serves_topic("probe-agent-missing").await);
    }

    /// An agent's own topic is named after the agent, so a `topic` equal to
    /// the `agent` names that same default topic — and it is refused like any
    /// other name nobody owns.
    #[tokio::test]
    async fn refuses_the_agent_own_topic_when_nobody_owns_it() {
        let tmp = tempdir().unwrap();
        let managers = TopicManagers::new();
        managers.set(vec![make_tm(tmp.path(), AGENTS_CHANNEL)]);

        let err = send_to_topic(
            &managers,
            source(tmp.path()),
            request(
                "probe-agent-same-name",
                Some("probe-agent-same-name"),
                "hello",
            ),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("probe-agent-same-name"), "{err}");
        assert!(err.to_string().contains("Create it first"), "{err}");
        let agents = managers.by_channel(AGENTS_CHANNEL).unwrap();
        assert!(!agents.serves_topic("probe-agent-same-name").await);
    }

    /// An agent's own topic is named after the agent — but a topic by that
    /// name may already be served by another channel, and then it is reached
    /// through that topic, not through a second one of the same name.
    #[tokio::test]
    async fn a_same_name_topic_served_elsewhere_is_reached_through_it() {
        let tmp = tempdir().unwrap();
        let agents = make_tm(&tmp.path().join("agents-ws"), AGENTS_CHANNEL);
        let other = make_tm(&tmp.path().join("mail-bot-ws"), "mail-bot");
        let managers = TopicManagers::new();
        managers.set(vec![agents.clone(), other.clone()]);
        let _cleanup = RegisteredState("probe-own-named".to_string());

        seed_topic(&other, "mail-bot", "probe-own-named").await;

        let sent = send_to_topic(
            &managers,
            source(tmp.path()),
            request("probe-own-named", Some("probe-own-named"), "hi"),
        )
        .await
        .unwrap();

        assert!(
            sent.contains("topic 'probe-own-named' of channel 'mail-bot'"),
            "{sent}"
        );
        assert!(other.serves_topic("probe-own-named").await);
        assert!(
            !agents.serves_topic("probe-own-named").await,
            "the agents channel must not be handed a topic that belongs elsewhere"
        );
    }

    /// The same name, idle since the last restart: its state records the
    /// channel that routed to it, and that channel serves it.
    #[tokio::test]
    async fn sends_to_the_channel_a_same_name_topic_state_records() {
        let tmp = tempdir().unwrap();
        let agents = make_tm(&tmp.path().join("agents-ws"), AGENTS_CHANNEL);
        let mail_bot = make_tm(&tmp.path().join("mail-bot-ws"), "mail-bot");
        let managers = TopicManagers::new();
        managers.set(vec![agents.clone(), mail_bot.clone()]);

        let topic = "probe-recorded-own";
        let state_dir = tmp.path().join("state").join(topic);
        write_topic_meta(&state_dir, "mail-bot");
        let _cleanup = register_state(topic, &state_dir);
        assert!(
            managers.serving(topic).await.is_none(),
            "nothing is running this topic's queue"
        );

        let sent = send_to_topic(
            &managers,
            source(tmp.path()),
            request(topic, Some(topic), "hi"),
        )
        .await
        .unwrap();

        assert!(
            sent.contains("topic 'probe-recorded-own' of channel 'mail-bot'"),
            "{sent}"
        );
        assert!(mail_bot.serves_topic(topic).await);
        assert!(
            !agents.serves_topic(topic).await,
            "the agents channel must not be handed a topic that belongs elsewhere"
        );
    }

    /// A named topic goes to whichever manager already serves it — the agent
    /// argument does not name the channel.
    #[tokio::test]
    async fn sends_to_the_manager_serving_a_named_topic() {
        let tmp = tempdir().unwrap();
        let agents = make_tm(&tmp.path().join("agents-ws"), AGENTS_CHANNEL);
        let other = make_tm(&tmp.path().join("mail-bot-ws"), "mail-bot");
        let managers = TopicManagers::new();
        managers.set(vec![agents.clone(), other.clone()]);
        let _cleanup = RegisteredState("probe-served-topic".to_string());

        // A routed message is what makes a topic exist: give `other` one.
        seed_topic(&other, "mail-bot", "probe-served-topic").await;
        assert_eq!(
            managers
                .serving("probe-served-topic")
                .await
                .unwrap()
                .channel_name(),
            "mail-bot"
        );

        let sent = send_to_topic(
            &managers,
            source(tmp.path()),
            request("invoice", Some("probe-served-topic"), "here is the PDF"),
        )
        .await
        .unwrap();

        assert!(sent.contains("topic 'probe-served-topic'"), "{sent}");
        assert!(other.serves_topic("probe-served-topic").await);
        assert!(
            !agents.serves_topic("probe-served-topic").await,
            "the agents channel must not be handed a topic that belongs elsewhere"
        );
    }

    /// A name no topic anywhere owns, and a named topic at that: refused rather
    /// than re-created under a name that would collide process-wide. A `topic`
    /// equal to the `agent` is not a named topic — that is the agent's own
    /// topic, and it is refused the same way.
    #[tokio::test]
    async fn refuses_a_named_topic_nobody_owns() {
        let tmp = tempdir().unwrap();
        let managers = TopicManagers::new();
        managers.set(vec![make_tm(tmp.path(), AGENTS_CHANNEL)]);

        let err = send_to_topic(
            &managers,
            source(tmp.path()),
            request("invoice", Some("unserved-probe-topic"), "hi"),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("unserved-probe-topic"), "{err}");
        let agents = managers.by_channel(AGENTS_CHANNEL).unwrap();
        assert!(!agents.serves_topic("unserved-probe-topic").await);
    }

    /// A topic idle since the last restart is still addressable: its own state
    /// records which channel routed to it, so the message goes to that channel
    /// without the topic having to be running a queue first.
    #[tokio::test]
    async fn sends_to_the_channel_a_topic_state_records() {
        let tmp = tempdir().unwrap();
        let agents = make_tm(&tmp.path().join("agents-ws"), AGENTS_CHANNEL);
        let mail_bot = make_tm(&tmp.path().join("mail-bot-ws"), "mail-bot");
        let managers = TopicManagers::new();
        managers.set(vec![agents.clone(), mail_bot.clone()]);

        let topic = "probe-recorded-topic";
        let state_dir = tmp.path().join("state").join(topic);
        write_topic_meta(&state_dir, "mail-bot");
        let _cleanup = register_state(topic, &state_dir);
        assert!(
            managers.serving(topic).await.is_none(),
            "nothing is running this topic's queue"
        );

        let sent = send_to_topic(
            &managers,
            source(tmp.path()),
            request("invoice", Some(topic), "hi"),
        )
        .await
        .unwrap();

        assert!(sent.contains("topic 'probe-recorded-topic'"), "{sent}");
        assert!(mail_bot.serves_topic("probe-recorded-topic").await);
        assert!(
            !agents.serves_topic(topic).await,
            "the agents channel must not be handed a topic that belongs elsewhere"
        );
    }

    /// The probe that follows the registry. A sub-topic's home is two
    /// directories down, which the startup scan never walks, so its state is
    /// never registered — resolution has to reach it from the names alone. Note
    /// the topic works in `<home>/.data`, so its state dir is one level up from
    /// what `topic_work_dir` answers with.
    #[tokio::test]
    async fn sends_to_a_sub_topic_whose_state_is_not_registered() {
        let tmp = tempdir().unwrap();
        let agents_ws = tmp.path().join("agents-ws");
        let agents = make_tm(&agents_ws, AGENTS_CHANNEL);
        let mail_bot = make_tm(&tmp.path().join("mail-bot-ws"), "mail-bot");
        let managers = TopicManagers::new();
        managers.set(vec![agents.clone(), mail_bot.clone()]);

        let agent = "probe-sub-agent";
        let topic = "probe-sub-topic";
        let home = agents_ws.join(agent).join(topic);
        std::fs::create_dir_all(home.join(".data")).unwrap();
        write_topic_meta(&home.join(".jyc"), "mail-bot");
        let _cleanup = RegisteredState(topic.to_string());
        assert!(jyc_types::state_dir::registered_state(topic).is_none());

        let sent = send_to_topic(
            &managers,
            source(tmp.path()),
            request(agent, Some(topic), "hi"),
        )
        .await
        .unwrap();

        assert!(sent.contains("topic 'probe-sub-topic'"), "{sent}");
        assert!(mail_bot.serves_topic("probe-sub-topic").await);
        assert!(!agents.serves_topic(topic).await);
    }

    /// Names are path components under the agents dir, and resolution probes that
    /// path, so path syntax is refused rather than followed.
    #[tokio::test]
    async fn refuses_a_name_that_is_path_syntax() {
        let tmp = tempdir().unwrap();
        let managers = TopicManagers::new();
        managers.set(vec![make_tm(tmp.path(), AGENTS_CHANNEL)]);

        for bad in ["../escape", "sub/dir", "win\\dir", ".", ".."] {
            // The name reaches the guard as an agent and as the topic slot; a
            // `--topic sub/dir` is the likelier carrier of the two.
            for req in [
                request(bad, None, "hi"),
                request("invoice", Some(bad), "hi"),
            ] {
                let err = send_to_topic(&managers, source(tmp.path()), req)
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("Invalid address"), "{bad}: {err}");
            }
        }
    }

    /// Topic names are made by the channel layer, not by `/spawn`: a sender
    /// address is a normal name, and the topic behind one has to stay
    /// addressable — the reply path for its agent depends on it.
    #[tokio::test]
    async fn sends_to_a_topic_named_like_a_sender_address() {
        let tmp = tempdir().unwrap();
        let agents = make_tm(&tmp.path().join("agents-ws"), AGENTS_CHANNEL);
        let mail_bot = make_tm(&tmp.path().join("mail-bot-ws"), "mail-bot");
        let managers = TopicManagers::new();
        managers.set(vec![agents.clone(), mail_bot.clone()]);

        let topic = "john.doe@example.com";
        let state_dir = tmp.path().join("state").join("sender-address");
        write_topic_meta(&state_dir, "mail-bot");
        let _cleanup = register_state(topic, &state_dir);

        let sent = send_to_topic(
            &managers,
            source(tmp.path()),
            request("invoice", Some(topic), "hi"),
        )
        .await
        .unwrap();

        assert!(sent.contains(topic), "{sent}");
        assert!(mail_bot.serves_topic(topic).await);
        assert!(!agents.serves_topic(topic).await);
    }

    /// The record names a channel this process does not run: refuse, rather
    /// than quietly landing the message somewhere else.
    #[tokio::test]
    async fn refuses_when_the_recorded_channel_is_not_running() {
        let tmp = tempdir().unwrap();
        let agents = make_tm(tmp.path(), AGENTS_CHANNEL);
        let managers = TopicManagers::new();
        managers.set(vec![agents.clone()]);

        let topic = "probe-absent-channel-topic";
        let state_dir = tmp.path().join("state").join(topic);
        write_topic_meta(&state_dir, "feishu-not-running");
        let _cleanup = register_state(topic, &state_dir);

        let err = send_to_topic(
            &managers,
            source(tmp.path()),
            request("invoice", Some(topic), "hi"),
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("feishu-not-running"), "{err}");
        assert!(!agents.serves_topic(topic).await);
    }

    /// The wiring `/send` depends on: a manager handed the registry resolves
    /// through it, and a worker clone taken *before* the wiring still does —
    /// the back-pointer is one shared cell, not a copy.
    #[tokio::test]
    async fn the_registry_wiring_survives_a_worker_clone() {
        let tmp = tempdir().unwrap();
        let tm = make_tm(tmp.path(), AGENTS_CHANNEL);
        let worker = tm.worker_clone();
        assert!(tm.managers().is_none(), "unwired to start");

        let managers = Arc::new(TopicManagers::new());
        managers.set(vec![tm.clone()]);
        tm.set_managers(&managers);

        assert_eq!(tm.managers().unwrap().all().len(), 1);
        assert!(
            worker.managers().is_some(),
            "a clone made before the wiring missed it"
        );
    }

    #[tokio::test]
    async fn refuses_an_empty_message_before_resolving_anything() {
        let tmp = tempdir().unwrap();
        let managers = TopicManagers::new();
        managers.set(vec![make_tm(tmp.path(), AGENTS_CHANNEL)]);

        let err = send_to_topic(
            &managers,
            source(tmp.path()),
            request("invoice", None, "  "),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Message cannot be empty"), "{err}");
    }

    #[tokio::test]
    async fn refuses_a_missing_or_escaping_attachment() {
        let tmp = tempdir().unwrap();
        let agents = make_tm(tmp.path(), AGENTS_CHANNEL);
        let managers = TopicManagers::new();
        managers.set(vec![agents.clone()]);
        // A send reaches topics that exist and refuses the ones that do not,
        // so the topic has to be there for the attachment check to be what
        // refuses this message.
        seed_topic(&agents, AGENTS_CHANNEL, "invoice").await;
        // The topic works in a directory of its own, so `..` names a file that
        // is really there and really outside it.
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let out = tmp.path().join("outside.pdf");
        std::fs::write(&out, b"x").unwrap();

        let attached = vec!["missing.pdf".to_string()];
        let err = send_to_topic(
            &managers,
            source(&work),
            SendRequest {
                attachments: &attached,
                ..request("invoice", None, "hi")
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Attachment not found"), "{err}");

        let escaping = vec!["../outside.pdf".to_string()];
        let err = send_to_topic(
            &managers,
            source(&work),
            SendRequest {
                attachments: &escaping,
                ..request("invoice", None, "hi")
            },
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("outside the working directory"),
            "{err}"
        );
    }

    /// A manager for `channel`, holding queues only — enough to answer
    /// "is this topic served here".
    fn make_tm(workspace: &Path, channel: &str) -> Arc<TopicManager> {
        let storage = Arc::new(MessageStorage::new(workspace));
        let cancel = CancellationToken::new();
        let metrics_cancel = CancellationToken::new();
        let (metrics, _stats, _metrics_task) = MetricsCollector::new(metrics_cancel).start();
        let config = Arc::new(ArcSwap::from_pointee(
            jyc_types::load_config_from_str(
                r#"
[general]
[channels.test]
type = "email"
[channels.test.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.test.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
"#,
            )
            .unwrap(),
        ));

        Arc::new(TopicManager::new_with_options(
            1,
            10,
            storage,
            Arc::new(NoopOutbound),
            Arc::new(StaticAgentService::new("ok")),
            cancel,
            false,
            workspace.join("templates"),
            config,
            channel.to_string(),
            "websocket".to_string(),
            workspace.to_path_buf(),
            workspace.to_path_buf(),
            metrics,
            None,
        ))
    }

    /// Minimal outbound adapter: these tests never send a reply.
    struct NoopOutbound;

    #[async_trait]
    impl jyc_types::OutboundAdapter for NoopOutbound {
        fn channel_type(&self) -> &str {
            "test"
        }
        async fn connect(&self) -> Result<()> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<()> {
            Ok(())
        }
        fn clean_body(&self, raw_body: &str) -> String {
            raw_body.to_string()
        }
        async fn send_reply(
            &self,
            _original: &jyc_types::InboundMessage,
            _reply_text: &str,
            _topic_path: &Path,
            _message_dir: &str,
            _attachments: Option<&[jyc_types::OutboundAttachment]>,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "noop".to_string(),
            })
        }
        async fn send_message(
            &self,
            _recipient: &str,
            _subject: &str,
            _body: &str,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "noop".to_string(),
            })
        }
    }
}
