use super::email::email_pipe_with_topic;
use super::*;

use jyc_types::ChannelPattern;

fn pattern_with(pipe: jyc_types::PipeTarget) -> ChannelPattern {
    ChannelPattern {
        name: format!("p-{}", pipe.topic.clone().unwrap_or_default()),
        channel: "feishu_bot".to_string(),
        enabled: true,
        pipe: Some(pipe),
        ..Default::default()
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
