use super::event_test_helpers::drain_events;
use super::event_test_helpers::scripted::ScriptedProvider;
use super::event_test_helpers::test_config;
use super::*;
use crate::tools::mcp_bridge::register_mcp_tools;
use crate::types::StreamEvent;
use jyc_core::topic_event_bus::{SimpleThreadEventBus, TopicEventBusRef};
use jyc_types::channel::{InboundMessage, OutboundAdapter, OutboundAttachment, SendResult};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

/// Registry matching production: builtin tools plus the MCP bridge
/// (which registers `jyc_reply_message`).
fn registry_with_reply_tool() -> ToolRegistry {
    let mut registry = crate::tools::builtin::create_builtin_registry();
    register_mcp_tools(&mut registry);
    registry
}

/// A text-only finish with the reply tool registered must be
/// auto-delivered immediately — no injected reminder, no nudge turn:
/// the loop executes `jyc_reply_message` synthetically with the text
/// plus the subtle trace, and the result counts as sent by the tool.
#[tokio::test]
async fn text_only_finish_auto_delivers_without_reminder() {
    let provider = ScriptedProvider {
        rounds: vec![vec![
            StreamEvent::TextDelta("I'll check the docs".to_string()),
            StreamEvent::Done,
        ]],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    let tools = registry_with_reply_tool();
    let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
    let mut rx = bus.subscribe().await.unwrap();
    let cancel = CancellationToken::new();

    let result = run(super::AgentLoopConfig {
        event_bus: Some(&bus),
        model_label: "scripted-test-auto",
        ..test_config(
            &provider,
            &tools,
            &working_dir,
            cancel.clone(),
            "reply-tool-auto",
        )
    })
    .await
    .expect("agent loop should run to completion");

    // Single LLM call: text-only finish → immediate synthetic delivery.
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(
        result.reply_sent_by_tool,
        "auto-delivered reply must count as sent by the tool"
    );
    assert!(
        result.reply_auto_delivered,
        "auto-delivered reply must be flagged for metrics"
    );
    assert_eq!(
        result.reply_text_from_tool.as_deref(),
        Some("I'll check the docs\n\n— auto-delivered"),
        "auto-delivered text must be the model text plus the subtle trace, got: {:?}",
        result.reply_text_from_tool
    );

    // No system reminder may be injected: that is the confusion source
    // this change removes.
    let reminders: Vec<_> = result
        .raw_context
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .filter(|c| c.contains("[System reminder]"))
        .collect();
    assert!(
        reminders.is_empty(),
        "no reminder may be injected on a text-only finish, got: {:?}",
        reminders
    );

    // No nudge/no_reply status events: the reply was delivered, not flagged.
    let events = drain_events(&mut rx).await;
    let nudges: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            TopicEvent::SessionStatus { status_type, .. }
                if status_type == "reply_tool_missing" || status_type == "no_reply" =>
            {
                Some(status_type.clone())
            }
            _ => None,
        })
        .collect();
    assert!(
        nudges.is_empty(),
        "no nudge/no_reply status events expected, got: {:?}",
        nudges
    );
}

/// A `silent: true` reply call closes the turn cleanly: counts as
/// reply-handled (no fallback warning, no reply text), delivers
/// nothing, and stops the loop.
#[tokio::test]
async fn silent_reply_closes_turn_without_delivery() {
    let provider = ScriptedProvider {
        rounds: vec![vec![
            StreamEvent::ToolUseStart {
                id: "call_1".to_string(),
                name: "jyc_reply_message".to_string(),
            },
            StreamEvent::ToolInputDelta(r#"{"silent":true}"#.to_string()),
            StreamEvent::ToolUseEnd,
            StreamEvent::Done,
        ]],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    let tools = registry_with_reply_tool();
    let cancel = CancellationToken::new();

    let result = run(super::AgentLoopConfig {
        model_label: "scripted-test-silent",
        ..test_config(
            &provider,
            &tools,
            &working_dir,
            cancel.clone(),
            "silent-reply",
        )
    })
    .await
    .expect("agent loop should run to completion");

    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(result.reply_sent_by_tool, "silent reply counts as handled");
    assert!(
        result.reply_text_from_tool.is_none(),
        "silent reply carries no text"
    );
    assert!(result.text.is_empty(), "no fallback text expected");
    // No signal files: the worker must skip post-loop delivery entirely.
    assert!(!working_dir.join(".jyc/reply.md").exists());
    assert!(!working_dir.join(".jyc/reply-sent.flag").exists());
}

/// A text-only finish must be auto-delivered in the agent's name: the
/// loop executes `jyc_reply_message` synthetically (writing the signal
/// files) and the result counts as sent by the tool, with a subtle
/// auto-delivery trace appended.
#[tokio::test]
async fn persistent_text_only_is_auto_delivered_via_reply_tool() {
    let provider = ScriptedProvider {
        rounds: vec![vec![
            StreamEvent::TextDelta("thinking out loud".to_string()),
            StreamEvent::Done,
        ]],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    let tools = registry_with_reply_tool();
    let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
    let cancel = CancellationToken::new();

    let result = run(super::AgentLoopConfig {
        event_bus: Some(&bus),
        model_label: "scripted-test-1",
        ..test_config(
            &provider,
            &tools,
            &working_dir,
            cancel.clone(),
            "reply-tool-persist",
        )
    })
    .await
    .expect("agent loop should run to completion");

    // Single LLM call: the text-only finish is auto-delivered via a
    // synthetic `jyc_reply_message` execution.
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert!(
        result.reply_sent_by_tool,
        "auto-delivered reply must count as sent by the tool"
    );
    assert!(
        result.reply_auto_delivered,
        "auto-delivered reply must be flagged for metrics"
    );
    assert_eq!(
        result.reply_text_from_tool.as_deref(),
        Some("thinking out loud\n\n— auto-delivered"),
        "auto-delivered text must be the last model text plus the subtle trace, got: {:?}",
        result.reply_text_from_tool
    );
    // The synthetic execution wrote the signal files (file-relay path,
    // since the test provides no outbound adapter / reply target).
    assert!(
        working_dir.join(".jyc/reply.md").exists(),
        "reply.md must be written by the synthetic auto-delivery"
    );
    assert!(
        working_dir.join(".jyc/reply-sent.flag").exists(),
        "reply-sent.flag must be written by the synthetic auto-delivery"
    );
}

/// Minimal outbound adapter that reports successful direct deliveries.
struct MockOutbound;

#[async_trait::async_trait]
impl OutboundAdapter for MockOutbound {
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
}

/// A synthetic auto-delivery that reaches a live outbound adapter must
/// publish `ReplySent`: the dashboard chat pane renders live replies only
/// from `chat_message` events fanned out of `ReplySent` (the raw
/// per-channel `reply` broadcast is ignored), so a delivered-but-eventless
/// reply shows in the logs but never in the chat pane.
#[tokio::test]
async fn synthetic_auto_delivery_publishes_reply_sent() {
    let provider = ScriptedProvider {
        rounds: vec![vec![
            StreamEvent::TextDelta("thinking out loud".to_string()),
            StreamEvent::Done,
        ]],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    let tools = registry_with_reply_tool();
    let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
    let mut rx = bus.subscribe().await.unwrap();
    let cancel = CancellationToken::new();
    let mock: Arc<dyn OutboundAdapter> = Arc::new(MockOutbound);
    let original = InboundMessage {
        id: "test".to_string(),
        channel: "test".to_string(),
        channel_uid: "1".to_string(),
        sender: "user".to_string(),
        sender_address: "user@test".to_string(),
        recipients: vec![],
        topic: "Test".to_string(),
        content: Default::default(),
        timestamp: chrono::Utc::now(),
        references: None,
        reply_to_id: None,
        external_id: None,
        attachments: vec![],
        metadata: Default::default(),
        matched_pattern: None,
    };

    let result = run(super::AgentLoopConfig {
        event_bus: Some(&bus),
        outbound: Some(mock),
        model_label: "scripted-test-2",
        reply_target: Some(crate::tools::ReplyTarget {
            original,
            message_dir: "2026-08-23_00-00-00".to_string(),
        }),
        ..test_config(
            &provider,
            &tools,
            &working_dir,
            cancel.clone(),
            "reply-sent-direct",
        )
    })
    .await
    .expect("agent loop should run to completion");

    assert!(
        result.reply_auto_delivered,
        "auto-delivered reply must be flagged for metrics"
    );
    let events = drain_events(&mut rx).await;
    let reply_sent: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            TopicEvent::ReplySent { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reply_sent,
        vec!["thinking out loud\n\n— auto-delivered"],
        "synchronous synthetic delivery must publish ReplySent with the delivered text"
    );
}

/// A FAILED `jyc_reply_message` call (empty message) followed by a
/// text-only finish must trigger the failure-aware reminder quoting the
/// concrete tool error, and the recovery turn must again be restricted
/// to the reply tool alone.
#[tokio::test]
async fn failed_reply_then_text_only_gets_failure_reminder() {
    let provider = ScriptedProvider {
        rounds: vec![
            // Round 0: broken reply call — empty message, tool errors.
            vec![
                StreamEvent::ToolUseStart {
                    id: "call_1".to_string(),
                    name: "jyc_reply_message".to_string(),
                },
                StreamEvent::ToolInputDelta(r#"{"message":"","stop_after":true}"#.to_string()),
                StreamEvent::ToolUseEnd,
                StreamEvent::Done,
            ],
            // Round 1: model gives up and finishes text-only.
            vec![
                StreamEvent::TextDelta("let me just say it in text".to_string()),
                StreamEvent::Done,
            ],
            // Round 2 (restricted): corrected reply call.
            vec![
                StreamEvent::ToolUseStart {
                    id: "call_2".to_string(),
                    name: "jyc_reply_message".to_string(),
                },
                StreamEvent::ToolInputDelta(
                    r#"{"message":"fixed answer","stop_after":true}"#.to_string(),
                ),
                StreamEvent::ToolUseEnd,
                StreamEvent::Done,
            ],
        ],
        calls: AtomicUsize::new(0),
        seen_tools: Default::default(),
    };
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    let tools = registry_with_reply_tool();
    let cancel = CancellationToken::new();

    let result = run(super::AgentLoopConfig {
        max_iterations: Some(6),
        model_label: "scripted-test-failure",
        ..test_config(
            &provider,
            &tools,
            &working_dir,
            cancel.clone(),
            "reply-failure",
        )
    })
    .await
    .expect("agent loop should run to completion");

    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    assert!(result.reply_sent_by_tool, "corrected reply must win");
    assert_eq!(result.reply_text_from_tool.as_deref(), Some("fixed answer"));

    // The injected reminder must be the failure-aware variant, quoting
    // the concrete tool error (empty message).
    let reminders: Vec<_> = result
        .raw_context
        .iter()
        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
        .filter(|c| c.contains("[System reminder]"))
        .collect();
    assert_eq!(reminders.len(), 1, "exactly one reminder expected");
    assert!(
        reminders[0].contains("FAILED and the reply was NOT delivered"),
        "expected REMINDER_REPLY_FAILED, got: {}",
        reminders[0]
    );
    assert!(
        reminders[0].contains("Message cannot be empty"),
        "reminder must quote the tool error, got: {}",
        reminders[0]
    );

    // The recovery turn after the failure reminder must also be
    // tool-restricted (round index 2).
    let seen = provider.seen_tools.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[2], vec!["jyc_reply_message".to_string()]);
}
