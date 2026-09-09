use super::email::email_pipe_with_topic;
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
    let v =
        serde_json::json!({"attachments": [{"filename": "x"}, {"filename": "y", "path": "/p"}]});
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
