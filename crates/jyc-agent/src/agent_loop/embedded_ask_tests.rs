//! Regression tests for the message-before-question ordering guarantee, the
//! turn ending at the question, and the embedded `<ask_user>` recovery shim.
//!
//! Two ways a model asks a question:
//!  1. Native tool call — narration text plus a structured `ask_user` call in
//!     one response. The narration must be delivered BEFORE the question, or
//!     the user sees the question box without its context.
//!  2. Embedded XML — the reply text ends with a literal `<ask_user ...>` tag
//!     (weak function-calling). The tag must be recovered as a real question
//!     (prose first, then the question), never shipped raw to the user.
//! Malformed tags are stripped from the delivered reply.
//!
//! Neither path waits for the answer, on any channel: the questions go out, the
//! turn ends, and the answer arrives as the user's next message. So every test
//! here asserts the provider was called exactly once — a second call means the
//! loop went on after the questions were already in front of the user.

use super::event_test_helpers::scripted::ScriptedProvider;
use super::event_test_helpers::test_config;
use super::*;
use crate::types::StreamEvent;
use jyc_types::channel::{
    InboundMessage, OutboundAdapter, OutboundAttachment, QuestionRequest, SendResult,
};
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

/// Order-preserving log of everything the outbound adapter saw.
#[derive(Default, Clone)]
struct DeliveryLog {
    entries: Arc<Mutex<Vec<(&'static str, String)>>>,
}

impl DeliveryLog {
    fn record(&self, kind: &'static str, text: String) {
        self.entries.lock().unwrap().push((kind, text));
    }

    fn snapshot(&self) -> Vec<(&'static str, String)> {
        self.entries.lock().unwrap().clone()
    }

    fn kinds(&self, kind: &str) -> Vec<String> {
        self.snapshot()
            .into_iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, text)| text)
            .collect()
    }
}

/// Outbound adapter with a question box: captures replies and questions in
/// delivery order.
struct CapturingOutbound {
    log: DeliveryLog,
}

#[async_trait::async_trait]
impl OutboundAdapter for CapturingOutbound {
    fn channel_type(&self) -> &str {
        "mock"
    }

    async fn connect(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn disconnect(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn clean_body(&self, body: &str) -> String {
        body.to_string()
    }

    async fn send_reply(
        &self,
        _original: &InboundMessage,
        reply_text: &str,
        _topic_path: &Path,
        _message_dir: &str,
        _attachments: Option<&[OutboundAttachment]>,
    ) -> anyhow::Result<SendResult> {
        self.log.record("reply", reply_text.to_string());
        Ok(SendResult {
            message_id: "mock-reply".to_string(),
        })
    }

    async fn send_message(
        &self,
        _recipient: &str,
        _subject: &str,
        _body: &str,
    ) -> anyhow::Result<SendResult> {
        Ok(SendResult {
            message_id: "mock-msg".to_string(),
        })
    }

    async fn send_question(&self, request: &QuestionRequest) -> Result<()> {
        self.log.record("question", request.question.clone());
        Ok(())
    }
}

/// The message a turn is answering. `origin` stamps a piped-in turn: the
/// channel the user actually wrote on, before the pipe re-targeted it.
fn reply_target(origin: Option<&str>) -> crate::tools::ReplyTarget {
    let mut metadata = serde_json::Map::new();
    if let Some(origin) = origin {
        metadata.insert(
            jyc_types::ORIGIN_CHANNEL_METADATA_KEY.to_string(),
            serde_json::Value::String(origin.to_string()),
        );
    }
    crate::tools::ReplyTarget {
        original: InboundMessage {
            id: "test".to_string(),
            channel: "mock".to_string(),
            channel_uid: "1".to_string(),
            sender: "user".to_string(),
            sender_address: "user@test".to_string(),
            recipients: vec![],
            topic: "test".to_string(),
            content: Default::default(),
            timestamp: chrono::Utc::now(),
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: metadata.into_iter().collect(),
            matched_pattern: None,
        },
        message_dir: "2026-09-18_00-00-00".to_string(),
    }
}

fn registry_with_reply_tool() -> crate::tools::registry::ToolRegistry {
    crate::tools::builtin::create_builtin_registry()
}

/// One round only: narration, then the embedded tag. The prose must be
/// delivered first, the tag must execute as a real question, the turn must end
/// there, and the raw tag must never appear in any delivery.
#[tokio::test]
async fn embedded_tag_recovers_question_and_ends_the_turn() {
    let prose = "## 方案\n\n改动 1、2、3。";
    let tag = "<ask_user question=\"开工吗？\" options=\"按方案, 再想想\">";
    let provider = ScriptedProvider {
        rounds: vec![vec![
            StreamEvent::TextDelta(format!("{prose}\n\n{tag}")),
            StreamEvent::Done,
        ]],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let tools = registry_with_reply_tool();
    let cancel = tokio_util::sync::CancellationToken::new();
    let log = DeliveryLog::default();
    let outbound = Arc::new(CapturingOutbound { log: log.clone() });

    let result = run(AgentLoopConfig {
        outbound: Some(outbound),
        reply_target: Some(reply_target(None)),
        current_channel: Some("mock".to_string()),
        ..test_config(&provider, &tools, tmp.path(), cancel, "embedded-ask")
    })
    .await
    .expect("agent loop should run to completion");

    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "the turn ends with the question; nothing is said after it"
    );
    assert!(
        result.text.is_empty(),
        "no reply text after the question: {:?}",
        result.text
    );

    let log = log.snapshot();
    assert_eq!(log[0].0, "reply", "message must come first: {log:?}");
    assert_eq!(log[0].1, prose);
    let question_pos = log
        .iter()
        .position(|(kind, _)| *kind == "question")
        .expect("the question must execute");
    assert!(question_pos > 0, "question must follow the prose: {log:?}");
    for (kind, text) in &log {
        assert!(
            !text.contains("<ask_user"),
            "raw tag leaked into a {kind} delivery: {text}"
        );
    }
}

/// Native path: one response carrying both narration text and a structured
/// `ask_user` call. Narration first, then the question — and on a channel with
/// a question box the questions go out as one frame each and never as a
/// duplicated plain-text message.
#[tokio::test]
async fn native_ask_delivers_narration_before_question() {
    let provider = ScriptedProvider {
        rounds: vec![vec![
            StreamEvent::TextDelta("方案如下，请定夺。".to_string()),
            StreamEvent::ToolUseStart {
                id: "ask-1".to_string(),
                name: "ask_user".to_string(),
            },
            StreamEvent::ToolInputDelta(
                "{\"question\":\"开工吗？\",\"options\":[\"按方案\",\"再想想\"]}".to_string(),
            ),
            StreamEvent::ToolUseEnd,
            StreamEvent::Done,
        ]],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let tools = registry_with_reply_tool();
    let cancel = tokio_util::sync::CancellationToken::new();
    let log = DeliveryLog::default();
    let outbound = Arc::new(CapturingOutbound { log: log.clone() });

    run(AgentLoopConfig {
        outbound: Some(outbound),
        reply_target: Some(reply_target(None)),
        current_channel: Some("mock".to_string()),
        ..test_config(&provider, &tools, tmp.path(), cancel, "native-ask")
    })
    .await
    .expect("agent loop should run to completion");

    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let log = log.snapshot();
    assert_eq!(
        log[0],
        ("reply", "方案如下，请定夺。".to_string()),
        "narration must be delivered before the question: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|(k, _)| *k == "question").count(),
        1,
        "the question is rendered once: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|(k, _)| *k == "reply").count(),
        1,
        "a boxed question is not also sent as plain text: {log:?}"
    );
}

/// Native path with a stray XML tag mixed into the narration: the tag must not
/// ship in the delivered narration, and the question must still execute exactly
/// once (via the native call).
#[tokio::test]
async fn native_ask_strips_embedded_tag_from_narration() {
    let provider = ScriptedProvider {
        rounds: vec![vec![
            StreamEvent::TextDelta(
                "方案如下。<ask_user question=\"泄漏的 tag\" options=\"x, y\">".to_string(),
            ),
            StreamEvent::ToolUseStart {
                id: "ask-1".to_string(),
                name: "ask_user".to_string(),
            },
            StreamEvent::ToolInputDelta(
                "{\"question\":\"开工吗？\",\"options\":[\"按方案\",\"再想想\"]}".to_string(),
            ),
            StreamEvent::ToolUseEnd,
            StreamEvent::Done,
        ]],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let tools = registry_with_reply_tool();
    let cancel = tokio_util::sync::CancellationToken::new();
    let log = DeliveryLog::default();
    let outbound = Arc::new(CapturingOutbound { log: log.clone() });

    run(AgentLoopConfig {
        outbound: Some(outbound),
        reply_target: Some(reply_target(None)),
        current_channel: Some("mock".to_string()),
        ..test_config(&provider, &tools, tmp.path(), cancel, "native-mixed")
    })
    .await
    .expect("agent loop should run to completion");

    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        log.kinds("reply"),
        vec!["方案如下。".to_string()],
        "narration must be delivered with the stray tag stripped"
    );
    assert_eq!(
        log.kinds("question"),
        vec!["开工吗？".to_string()],
        "exactly one question must execute"
    );
}

/// A malformed tag (missing required attributes) cannot be executed, but
/// it must still never reach the user: the reply ships the prose alone.
#[tokio::test]
async fn malformed_tag_is_stripped_from_delivery() {
    let provider = ScriptedProvider {
        rounds: vec![vec![
            StreamEvent::TextDelta("结论先行。\n\n<ask_user options=\"a, b\">".to_string()),
            StreamEvent::Done,
        ]],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let tools = registry_with_reply_tool();
    let cancel = tokio_util::sync::CancellationToken::new();
    let log = DeliveryLog::default();
    let outbound = Arc::new(CapturingOutbound { log: log.clone() });

    let result = run(AgentLoopConfig {
        outbound: Some(outbound),
        reply_target: Some(reply_target(None)),
        current_channel: Some("mock".to_string()),
        ..test_config(&provider, &tools, tmp.path(), cancel, "malformed-ask")
    })
    .await
    .expect("agent loop should run to completion");

    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(result.reply_delivered);
    assert_eq!(
        result.text, "结论先行。",
        "malformed tag must be stripped from the delivered reply, got: {:?}",
        result.text
    );
    let log = log.snapshot();
    assert!(
        log.iter()
            .all(|(kind, text)| { *kind == "reply" && !text.contains("<ask_user") }),
        "no delivery may contain raw tag syntax: {log:?}"
    );
    assert!(
        log.iter().all(|(kind, _)| *kind != "question"),
        "a malformed tag must not execute a question: {log:?}"
    );
}

/// The field failure this covers: a turn piped in from a channel with no
/// question box (feishu) settled with an error, and the user read that error
/// instead of a question. The questions must go out as an ordinary message on
/// the channel the user is actually reading, and the turn must end there
/// instead of going on to say something else.
#[tokio::test]
async fn piped_ask_without_a_box_is_delivered_as_plain_text() {
    let provider = ScriptedProvider {
        rounds: vec![vec![
            StreamEvent::TextDelta("两个问题，回个编号就行。".to_string()),
            StreamEvent::ToolUseStart {
                id: "ask-1".to_string(),
                name: "ask_user".to_string(),
            },
            StreamEvent::ToolInputDelta(
                "{\"question\":\"开工吗？\",\"options\":[\"按方案\",\"再想想\"]}".to_string(),
            ),
            StreamEvent::ToolUseEnd,
            StreamEvent::Done,
        ]],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let tools = registry_with_reply_tool();
    let cancel = tokio_util::sync::CancellationToken::new();
    let log = DeliveryLog::default();
    let outbound = Arc::new(CapturingOutbound { log: log.clone() });

    let result = run(AgentLoopConfig {
        outbound: Some(outbound),
        // Piped in from a channel with no question box, running on the hub.
        reply_target: Some(reply_target(Some("feishu_bot"))),
        current_channel: Some("agents".to_string()),
        ..test_config(&provider, &tools, tmp.path(), cancel, "piped-ask")
    })
    .await
    .expect("the loop runs to completion");

    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "the turn ends with the questions instead of going on"
    );
    assert!(
        !result.reply_delivered,
        "nothing was delivered after the questions"
    );

    let replies = log.kinds("reply");
    let log = log.snapshot();
    assert!(
        log.iter().all(|(kind, _)| *kind != "question"),
        "the hub channel never sees a question this user cannot answer: {log:?}"
    );
    assert_eq!(replies.len(), 2, "narration then the questions: {log:?}");
    assert_eq!(replies[0], "两个问题，回个编号就行。");
    assert!(
        replies[1].contains("Q1: 开工吗？")
            && replies[1].contains("1) 按方案")
            && replies[1].contains("2) 再想想"),
        "the questions must reach the user, answerable by number: {}",
        replies[1]
    );
    assert!(
        log.iter().all(|(_, text)| !text.contains("[ERROR]")),
        "no internal tool error may ship to the user: {log:?}"
    );
}

/// The same fallback through the other path that runs `ask_user`: a weak model
/// wrote the question as an XML tag, and the turn came from a channel with no
/// question box. The recovery shim executes the tool on its own, so it has to
/// hand the queued questions over for delivery and stop — otherwise the user is
/// promised a message that is never sent, or gets a second one after it.
#[tokio::test]
async fn embedded_tag_from_a_channel_without_a_box_is_delivered_as_plain_text() {
    let tag = "<ask_user question=\"开工吗？\" options=\"按方案, 再想想\">";
    let provider = ScriptedProvider {
        rounds: vec![vec![
            StreamEvent::TextDelta(format!("两个问题，回个编号就行。\n\n{tag}")),
            StreamEvent::Done,
        ]],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let tools = registry_with_reply_tool();
    let cancel = tokio_util::sync::CancellationToken::new();
    let log = DeliveryLog::default();
    let outbound = Arc::new(CapturingOutbound { log: log.clone() });

    run(AgentLoopConfig {
        outbound: Some(outbound),
        reply_target: Some(reply_target(Some("feishu_bot"))),
        current_channel: Some("agents".to_string()),
        ..test_config(&provider, &tools, tmp.path(), cancel, "embedded-piped-ask")
    })
    .await
    .expect("the loop runs to completion");

    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "the turn ends with the questions"
    );
    let replies = log.kinds("reply");
    let log = log.snapshot();
    assert!(
        log.iter().all(|(kind, _)| *kind != "question"),
        "no question may be pushed where the user cannot answer one: {log:?}"
    );
    assert_eq!(
        replies.len(),
        2,
        "the prose and the questions, then nothing: {log:?}"
    );
    let asked = &replies[1];
    assert!(asked.contains("Q1: 开工吗"), "{asked}");
    assert!(asked.contains("1) 按方案"), "answerable by number: {asked}");
    assert!(
        !asked.contains("<ask_user"),
        "the raw tag must not ship: {asked}"
    );
}

/// An adapter whose channel is down: `send_question` fails on the trait default
/// (so the questions fall back to plain text) and every reply fails too.
struct BrokenChannel;

#[async_trait::async_trait]
impl OutboundAdapter for BrokenChannel {
    fn channel_type(&self) -> &str {
        "mock"
    }

    async fn connect(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn disconnect(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn clean_body(&self, body: &str) -> String {
        body.to_string()
    }

    async fn send_reply(
        &self,
        _original: &InboundMessage,
        _reply_text: &str,
        _topic_path: &Path,
        _message_dir: &str,
        _attachments: Option<&[OutboundAttachment]>,
    ) -> anyhow::Result<SendResult> {
        Err(anyhow::anyhow!("channel is down"))
    }

    async fn send_message(
        &self,
        _recipient: &str,
        _subject: &str,
        _body: &str,
    ) -> anyhow::Result<SendResult> {
        Err(anyhow::anyhow!("channel is down"))
    }
}

fn ask_round(question: &str, options: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseStart {
            id: "ask-1".to_string(),
            name: "ask_user".to_string(),
        },
        StreamEvent::ToolInputDelta(format!(
            "{{\"question\":\"{question}\",\"options\":[{options}]}}"
        )),
        StreamEvent::ToolUseEnd,
        StreamEvent::Done,
    ]
}

/// The failure this covers: the questions could not be delivered, and the turn
/// ended anyway with the model told not to ask again — the block went to the
/// file relay, whose watcher dies with the turn, so nothing would ever send it
/// and only a `warn` log remained. Undelivered questions now become the turn's
/// reply text, which the worker delivers with the ordinary reply machinery.
#[tokio::test]
async fn undelivered_questions_become_the_turns_reply() {
    let provider = ScriptedProvider {
        rounds: vec![ask_round("开工吗？", "\"按方案\", \"再想想\"")],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let tools = registry_with_reply_tool();

    let result = run(AgentLoopConfig {
        outbound: Some(Arc::new(BrokenChannel)),
        reply_target: Some(reply_target(None)),
        current_channel: Some("mock".to_string()),
        ..test_config(
            &provider,
            &tools,
            tmp.path(),
            tokio_util::sync::CancellationToken::new(),
            "undelivered-ask",
        )
    })
    .await
    .expect("a dead channel must not fail the loop");

    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "the model is not asked to say it again"
    );
    assert!(
        !result.reply_delivered,
        "nothing reached the channel directly"
    );
    assert!(
        result.text.contains("Q1: 开工吗？") && result.text.contains("2) 再想想"),
        "the worker must be handed something it can deliver: {:?}",
        result.text
    );
}

/// A turn the user cancelled is a failed turn whatever it delivered first.
/// `ask_user` stops the loop where it stands, and the completion event the
/// dashboard clears its "thinking" state on must still report the cancel rather
/// than a clean finish.
#[tokio::test]
async fn cancel_in_the_question_batch_still_reports_failure() {
    use jyc_core::topic_event_bus::SimpleThreadEventBus;

    // The question first, then a `bash sleep 5` in the same batch: the cancel
    // lands while that tool runs, so the turn is stopped by the user right where
    // `ask_user` had already ended it.
    let provider = ScriptedProvider {
        rounds: vec![{
            let mut round = ask_round("开工吗？", "\"按方案\"");
            round.pop();
            round.extend(vec![
                StreamEvent::ToolUseStart {
                    id: "bash-1".to_string(),
                    name: "bash".to_string(),
                },
                StreamEvent::ToolInputDelta("{\"command\":\"sleep 5\"}".to_string()),
                StreamEvent::ToolUseEnd,
                StreamEvent::Done,
            ]);
            round
        }],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let tools = registry_with_reply_tool();
    let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(256));
    let mut rx = bus.subscribe().await.unwrap();
    let log = DeliveryLog::default();
    let cancel = tokio_util::sync::CancellationToken::new();
    let fire = cancel.clone();
    let waiter = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        fire.cancel();
    });

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run(AgentLoopConfig {
            outbound: Some(Arc::new(CapturingOutbound { log: log.clone() })),
            reply_target: Some(reply_target(None)),
            current_channel: Some("mock".to_string()),
            event_bus: Some(&bus),
            ..test_config(&provider, &tools, tmp.path(), cancel, "cancel-ask")
        }),
    )
    .await
    .expect("the cancelled bash tool must not be waited out")
    .expect("cancellation must not surface as an error");
    let _ = waiter.await;

    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(
        result.text.is_empty(),
        "a cancelled turn delivers nothing further: {:?}",
        result.text
    );

    let mut success = None;
    while let Ok(Some(event)) =
        tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
    {
        if let TopicEvent::ProcessingCompleted { success: s, .. } = event {
            success = Some(s);
        }
    }
    assert_eq!(
        success,
        Some(false),
        "a cancelled turn must not report a clean finish"
    );
}
