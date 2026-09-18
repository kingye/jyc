//! Regression tests for the message-before-question ordering guarantee and
//! the embedded `<ask_user>` recovery shim.
//!
//! Two ways a model reaches a blocking question:
//!  1. Native tool call — narration text plus a structured `ask_user` call
//!     in one response. The narration must be delivered BEFORE the tool
//!     blocks, or the user sees the question card without the context.
//!  2. Embedded XML — the reply text ends with a literal `<ask_user ...>`
//!     tag (weak function-calling). The tag must be recovered as a real
//!     question (prose first, then block), never shipped raw to the user.
//! Malformed tags are stripped from the delivered reply.

use super::event_test_helpers::scripted::ScriptedProvider;
use super::event_test_helpers::test_config;
use super::*;
use crate::tools::mcp_bridge::register_mcp_tools;
use crate::types::StreamEvent;
use jyc_core::question::QuestionHub;
use jyc_types::channel::{
    InboundMessage, OutboundAdapter, OutboundAttachment, QuestionAnswer, QuestionRequest,
    SendResult,
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
}

/// Outbound adapter that captures replies and questions in delivery order.
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

fn registry_with_reply_tool() -> crate::tools::registry::ToolRegistry {
    let mut registry = crate::tools::builtin::create_builtin_registry();
    register_mcp_tools(&mut registry);
    registry
}

/// Drive the loop while polling the hub; answer the pending question as
/// soon as it appears, then let the loop run to completion.
async fn run_and_answer(
    config: AgentLoopConfig<'_>,
    hub: Arc<QuestionHub>,
    topic: &str,
) -> AgentLoopResult {
    let answerer = async {
        loop {
            if let Some(id) = hub.pending_for(topic) {
                return hub.respond(&id, QuestionAnswer::Choice("按方案".to_string()));
            }
            tokio::task::yield_now().await;
        }
    };
    tokio::pin!(let run = run(config););
    tokio::select! {
        finished = &mut run => panic!("run finished before the question was answered: {finished:?}"),
        answered = answerer => {
            assert!(answered, "hub.respond must find the pending question");
            run.await.expect("agent loop should run to completion")
        }
    }
}

/// The exact field failure: reply text ends with a literal `<ask_user>`
/// tag. The prose must be delivered first, the question must execute
/// (blocking), the answer must reach the model, and the raw tag must
/// never appear in any delivery.
#[tokio::test]
async fn embedded_tag_recovers_question_with_message_first() {
    let prose = "## 方案\n\n改动 1、2、3。";
    let tag = "<ask_user question=\"开工吗？\" options=\"按方案, 再想想\" timeout_seconds=\"60\">";
    let round1 = format!("{prose}\n\n{tag}");
    let provider = ScriptedProvider {
        rounds: vec![
            vec![StreamEvent::TextDelta(round1), StreamEvent::Done],
            vec![
                StreamEvent::TextDelta("收到，按方案开工。".to_string()),
                StreamEvent::Done,
            ],
        ],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let tools = registry_with_reply_tool();
    let cancel = tokio_util::sync::CancellationToken::new();
    let log = DeliveryLog::default();
    let outbound = Arc::new(CapturingOutbound { log: log.clone() });
    let hub = Arc::new(QuestionHub::new());

    let result = run_and_answer(
        AgentLoopConfig {
            outbound: Some(outbound),
            current_channel: Some("mock".to_string()),
            question_hub: Some(hub.clone()),
            ..test_config(&provider, &tools, tmp.path(), cancel, "embedded-ask")
        },
        hub,
        "embedded-ask",
    )
    .await;

    // The answer unblocked the question and the loop continued.
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(
        result.reply_auto_delivered,
        "the post-answer conclusion must auto-deliver"
    );
    assert_eq!(
        result.reply_text_from_tool.as_deref(),
        Some("收到，按方案开工。\n\n— auto-delivered")
    );
    // The answer reached the transcript as the embedded question's result.
    let tool_msgs: Vec<String> = result
        .raw_context
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()).map(String::from))
        .collect();
    assert!(
        tool_msgs.iter().any(|c| c == "按方案"),
        "the answer must be the embedded ask's tool result, got: {tool_msgs:?}"
    );

    // Ordering: prose reply strictly before the question; no raw tag
    // anywhere in the deliveries.
    let log = log.snapshot();
    assert_eq!(log[0].0, "reply", "message must come first: {log:?}");
    assert_eq!(log[0].1, prose);
    assert!(
        log.iter().any(|(kind, _)| *kind == "question"),
        "the question must execute: {log:?}"
    );
    let question_pos = log.iter().position(|(k, _)| *k == "question").unwrap();
    assert!(question_pos > 0, "question must follow the prose: {log:?}");
    for (kind, text) in &log {
        assert!(
            !text.contains("<ask_user"),
            "raw tag leaked into a {kind} delivery: {text}"
        );
    }
}

/// Native path: one response carrying both narration text and a
/// structured `ask_user` call. Same ordering guarantee — narration first,
/// then the blocking question.
#[tokio::test]
async fn native_ask_delivers_narration_before_question() {
    let provider = ScriptedProvider {
        rounds: vec![
            vec![
                StreamEvent::TextDelta("方案如下，请定夺。".to_string()),
                StreamEvent::ToolUseStart {
                    id: "ask-1".to_string(),
                    name: "ask_user".to_string(),
                },
                StreamEvent::ToolInputDelta(
                    "{\"question\":\"开工吗？\",\"options\":[\"按方案\",\"再想想\"],\"timeout_seconds\":60}"
                        .to_string(),
                ),
                StreamEvent::ToolUseEnd,
                StreamEvent::Done,
            ],
            vec![
                StreamEvent::TextDelta("收到，按方案开工。".to_string()),
                StreamEvent::Done,
            ],
        ],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let tools = registry_with_reply_tool();
    let cancel = tokio_util::sync::CancellationToken::new();
    let log = DeliveryLog::default();
    let outbound = Arc::new(CapturingOutbound { log: log.clone() });
    let hub = Arc::new(QuestionHub::new());

    let result = run_and_answer(
        AgentLoopConfig {
            outbound: Some(outbound),
            current_channel: Some("mock".to_string()),
            question_hub: Some(hub.clone()),
            ..test_config(&provider, &tools, tmp.path(), cancel, "native-ask")
        },
        hub,
        "native-ask",
    )
    .await;

    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(
        result.reply_auto_delivered,
        "the post-answer conclusion must auto-deliver"
    );
    let log = log.snapshot();
    assert_eq!(
        log[0],
        ("reply", "方案如下，请定夺。".to_string()),
        "narration must be delivered before the question: {log:?}"
    );
    let question_pos = log.iter().position(|(k, _)| *k == "question").unwrap();
    assert!(
        question_pos > 0,
        "question must follow the narration: {log:?}"
    );
}

/// Native path with a stray XML tag mixed into the narration: the tag
/// must not ship in the delivered narration, and the question must still
/// execute exactly once (via the native call).
#[tokio::test]
async fn native_ask_strips_embedded_tag_from_narration() {
    let provider = ScriptedProvider {
        rounds: vec![
            vec![
                StreamEvent::TextDelta(
                    "方案如下。<ask_user question=\"泄漏的 tag\" options=\"x, y\">"
                        .to_string(),
                ),
                StreamEvent::ToolUseStart {
                    id: "ask-1".to_string(),
                    name: "ask_user".to_string(),
                },
                StreamEvent::ToolInputDelta(
                    "{\"question\":\"开工吗？\",\"options\":[\"按方案\",\"再想想\"],\"timeout_seconds\":60}"
                        .to_string(),
                ),
                StreamEvent::ToolUseEnd,
                StreamEvent::Done,
            ],
            vec![
                StreamEvent::TextDelta("收到，按方案开工。".to_string()),
                StreamEvent::Done,
            ],
        ],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let tools = registry_with_reply_tool();
    let cancel = tokio_util::sync::CancellationToken::new();
    let log = DeliveryLog::default();
    let outbound = Arc::new(CapturingOutbound { log: log.clone() });
    let hub = Arc::new(QuestionHub::new());

    let result = run_and_answer(
        AgentLoopConfig {
            outbound: Some(outbound),
            current_channel: Some("mock".to_string()),
            question_hub: Some(hub.clone()),
            ..test_config(&provider, &tools, tmp.path(), cancel, "native-mixed")
        },
        hub,
        "native-mixed",
    )
    .await;

    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(result.reply_auto_delivered);
    let log = log.snapshot();
    assert_eq!(
        log[0],
        ("reply", "方案如下。".to_string()),
        "narration must be delivered with the stray tag stripped: {log:?}"
    );
    assert_eq!(
        log.iter().filter(|(k, _)| *k == "question").count(),
        1,
        "exactly one question must execute: {log:?}"
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
    let hub = Arc::new(QuestionHub::new());

    let result = run(AgentLoopConfig {
        outbound: Some(outbound),
        current_channel: Some("mock".to_string()),
        question_hub: Some(hub.clone()),
        ..test_config(&provider, &tools, tmp.path(), cancel, "malformed-ask")
    })
    .await
    .expect("agent loop should run to completion");

    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        result.reply_text_from_tool.as_deref(),
        Some("结论先行。\n\n— auto-delivered"),
        "malformed tag must be stripped from the delivered reply, got: {:?}",
        result.reply_text_from_tool
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
