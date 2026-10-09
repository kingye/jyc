use super::email::email_pipe_with_topic;

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
