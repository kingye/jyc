//! The reply guard's wiring.
//!
//! `reply_guard.rs` covers the decisions; these cover the decisions
//! reaching the requester: a request the turn did not answer is answered by
//! the turn's own last words, and a request the turn did answer is left
//! alone.

use super::reply_guard::{ReplyRequest, ReplySource, TurnEnding};
use super::tests::app_config_with_channels;
use super::*;
use crate::tools::{Tool, ToolContext};
use jyc_core::message_storage::MessageStorage;
use jyc_core::metrics::MetricsCollector;
use jyc_core::static_agent::StaticAgentService;
use jyc_core::topic_manager::TopicManager;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

/// Minimal outbound adapter: these tests never send a reply.
struct NoopOutbound;

#[async_trait]
impl jyc_types::OutboundAdapter for NoopOutbound {
    fn channel_type(&self) -> &str {
        "test"
    }
    async fn connect(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn disconnect(&self) -> anyhow::Result<()> {
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
    ) -> anyhow::Result<jyc_types::SendResult> {
        Ok(jyc_types::SendResult {
            message_id: "noop".to_string(),
            chat_seq: 0,
        })
    }
}

/// A topic manager for the `agents` channel, with no worker behind it: the
/// tests only care about what lands in a queue.
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
        jyc_types::PatternMatch {
            pattern_name: "guard".to_string(),
            channel: channel.to_string(),
            matches: HashMap::new(),
        },
        None,
        false,
        None,
    )
    .await;
}

/// A service that can reach the managers the guard forwards through.
///
/// The data dir is the caller's temporary directory: a fixed path is shared
/// state between test runs.
fn service_with_managers(managers: Arc<TopicManagers>, data_dir: &Path) -> JycAgentService {
    let service = JycAgentService::new(
        app_config_with_channels(None, HashMap::new()),
        data_dir.to_path_buf(),
        vec![],
        None,
        None,
        None,
        None,
        "agents".to_string(),
    );
    service.set_topic_managers(managers);
    service
}

/// A reply request from the topic "guard-source" on the agents channel.
///
/// Nothing was sent before it arrived, so its mark is zero.
fn source_request() -> ReplyRequest {
    ReplyRequest {
        source: ReplySource {
            channel: "agents".to_string(),
            topic: "guard-source".to_string(),
        },
        sent_before: 0,
    }
}

/// The requests a turn owes are the ones its message carried plus the ones
/// that arrived while it ran.
///
/// This is the join the service does at the end of a turn: the message the
/// turn handled asked for an answer, and a second request arrived while the
/// turn was running. Both requesters hear something.
#[tokio::test]
async fn the_guard_answers_the_request_the_message_carried_and_the_one_that_arrived() {
    let tmp = tempdir().unwrap();
    let agents = make_tm(tmp.path(), "agents");
    let managers = Arc::new(TopicManagers::new());
    managers.set(vec![agents.clone()]);
    seed_topic(&agents, "agents", "guard-source").await;
    seed_topic(&agents, "agents", "guard-source-2").await;

    let service = service_with_managers(managers.clone(), tmp.path());
    let mut metadata = HashMap::new();
    metadata.insert("require_reply".to_string(), json!(true));
    metadata.insert("source_channel".to_string(), json!("agents"));
    metadata.insert("source_topic".to_string(), json!("guard-source"));
    let message = jyc_types::InboundMessage {
        id: "m1".to_string(),
        channel: "agents".to_string(),
        channel_uid: "m1".to_string(),
        sender: "user".to_string(),
        sender_address: "user@example.com".to_string(),
        recipients: vec![],
        topic: "guard-target".to_string(),
        content: jyc_types::MessageContent {
            text: Some("do work".to_string()),
            html: None,
            markdown: None,
        },
        timestamp: chrono::Utc::now(),
        references: None,
        reply_to_id: None,
        external_id: None,
        attachments: vec![],
        metadata,
        matched_pattern: None,
    };
    let arrived = [ReplyRequest {
        source: ReplySource {
            channel: "agents".to_string(),
            topic: "guard-source-2".to_string(),
        },
        sent_before: 0,
    }];

    let forwarded = service
        .guard_and_forward(
            &message,
            "guard-target",
            tmp.path(),
            TurnEnding::Finished("all done"),
            &arrived,
            &[],
        )
        .await;

    assert_eq!(forwarded.len(), 2, "both requesters get an answer");
    assert_eq!(forwarded[0].topic, "guard-source");
    assert_eq!(forwarded[1].topic, "guard-source-2");
}

/// The requester hears the turn's last words when the turn did not answer.
#[tokio::test]
async fn the_guard_answers_a_request_the_turn_left_unanswered() {
    let tmp = tempdir().unwrap();
    let agents = make_tm(tmp.path(), "agents");
    let managers = Arc::new(TopicManagers::new());
    managers.set(vec![agents.clone()]);
    seed_topic(&agents, "agents", "guard-source").await;

    let service = service_with_managers(managers.clone(), tmp.path());
    let forwarded = service
        .forward_unanswered_replies(
            Some(&managers),
            &[source_request()],
            "guard-target",
            tmp.path(),
            TurnEnding::Finished("all done"),
            &[],
        )
        .await;

    assert_eq!(forwarded.len(), 1, "the requester gets an answer");
    assert_eq!(forwarded[0].topic, "guard-source");
    assert_eq!(forwarded[0].channel, "agents");
}

/// A turn that already answered the request is not answered twice.
#[tokio::test]
async fn the_guard_leaves_an_answered_request_alone() {
    let tmp = tempdir().unwrap();
    let agents = make_tm(tmp.path(), "agents");
    let managers = Arc::new(TopicManagers::new());
    managers.set(vec![agents.clone()]);
    seed_topic(&agents, "agents", "guard-source").await;

    let service = service_with_managers(managers.clone(), tmp.path());
    let forwarded = service
        .forward_unanswered_replies(
            Some(&managers),
            &[source_request()],
            "guard-target",
            tmp.path(),
            TurnEnding::Finished("all done"),
            &[("agents".to_string(), "guard-source".to_string())],
        )
        .await;

    assert!(forwarded.is_empty(), "the turn answered it already");
}

/// Nothing was asked, so nothing is forwarded.
#[tokio::test]
async fn the_guard_answers_nothing_when_nothing_was_asked() {
    let tmp = tempdir().unwrap();
    let agents = make_tm(tmp.path(), "agents");
    let managers = Arc::new(TopicManagers::new());
    managers.set(vec![agents.clone()]);

    let service = service_with_managers(managers.clone(), tmp.path());
    let forwarded = service
        .forward_unanswered_replies(
            Some(&managers),
            &[],
            "guard-target",
            tmp.path(),
            TurnEnding::Finished("all done"),
            &[],
        )
        .await;

    assert!(forwarded.is_empty());
}

/// A delivery is recorded, so the guard can tell an answered request from
/// an unanswered one.
#[tokio::test]
async fn a_delivery_is_recorded_against_the_topic_that_took_it() {
    let tmp = tempdir().unwrap();
    let agents = make_tm(tmp.path(), "agents");
    let managers = Arc::new(TopicManagers::new());
    managers.set(vec![agents.clone()]);
    seed_topic(&agents, "agents", "guard-source").await;

    let mut ctx = ToolContext::new(tmp.path());
    ctx.topic_managers = Some(managers.clone());
    ctx.current_channel = Some("agents".to_string());
    ctx.current_topic = Some("guard-target".to_string());

    let tool = crate::tools::builtin::send_to_topic::SendToThreadTool;
    let output = tool
        .execute(
            json!({"agent": "guard-source", "message": "here is the answer"}),
            &ctx,
        )
        .await
        .expect("the delivery succeeds");

    assert!(!output.content.is_empty());
    let recorded = ctx.outbound_targets.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![("agents".to_string(), "guard-source".to_string())],
        "the turn sent to the source topic, so it answered it"
    );
}

/// A delivery that failed leaves no record: the guard has to answer the
/// request itself.
#[tokio::test]
async fn a_failed_delivery_is_not_recorded() {
    let tmp = tempdir().unwrap();
    let agents = make_tm(tmp.path(), "agents");
    let managers = Arc::new(TopicManagers::new());
    managers.set(vec![agents.clone()]);

    let mut ctx = ToolContext::new(tmp.path());
    ctx.topic_managers = Some(managers.clone());
    ctx.current_channel = Some("agents".to_string());
    ctx.current_topic = Some("guard-target".to_string());

    let tool = crate::tools::builtin::send_to_topic::SendToThreadTool;
    let failed = tool
        .execute(
            json!({"agent": "guard-nowhere", "message": "nobody is there"}),
            &ctx,
        )
        .await
        .expect("the call itself succeeds: a refused delivery is an error output");

    // A refused delivery is reported the way every tool reports a failure:
    // as an output the model reads, not as a raised error.
    assert!(
        failed.is_error,
        "no topic owns that name, so the delivery failed: {:?}",
        failed.content
    );
    assert!(
        ctx.outbound_targets.lock().unwrap().is_empty(),
        "a delivery that did not happen is not a delivery"
    );
}
