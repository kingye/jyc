//! Tests for live injection: the messages that arrive while a turn is
//! running join the conversation instead of waiting for the turn to end.
//!
//! Two layers are covered: the drain itself (a batch becomes user
//! messages, in order, and lands in the chat log) and the drain point
//! (the loop picks the batch up before it builds the next call, so the
//! model sees it there).

use super::*;
use crate::provider::{EventStream, Provider};
use crate::types::{Message, StreamEvent, ToolDefinition};
use async_trait::async_trait;
use futures::stream;
use jyc_types::{InboundMessage, MessageContent, PatternMatch, QueueItem};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// A queue item carrying a plain text body.
fn queue_item(text: &str) -> QueueItem {
    QueueItem {
        topic_name: "inject-test".to_string(),
        message: InboundMessage {
            id: format!("id-{text}"),
            channel: "test".to_string(),
            channel_uid: "test".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "inject-test".to_string(),
            content: MessageContent {
                text: Some(text.to_string()),
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
        pattern_match: PatternMatch {
            pattern_name: "test".to_string(),
            channel: "test".to_string(),
            matches: HashMap::new(),
        },
        attachment_config: None,
        template: None,
        live_injection: true,
        topic_path_override: None,
    }
}

/// A queue item that came from another topic, without asking for an answer.
fn sourced_item(text: &str, channel: &str, topic: &str) -> QueueItem {
    let mut item = queue_item(text);
    item.message
        .metadata
        .insert("source_channel".to_string(), serde_json::json!(channel));
    item.message
        .metadata
        .insert("source_topic".to_string(), serde_json::json!(topic));
    item
}

/// A queue item that came from another topic and asked for an answer.
fn reply_request_item(text: &str, channel: &str, topic: &str) -> QueueItem {
    let mut item = sourced_item(text, channel, topic);
    item.message
        .metadata
        .insert("require_reply".to_string(), serde_json::json!(true));
    item
}

/// A tool that records where the turn sent, the way `jyc_send_to_topic`
/// does — the record the guard reads to tell an answered request from an
/// unanswered one.
struct RecordingSendTool {
    target: (String, String),
}

#[async_trait]
impl crate::tools::Tool for RecordingSendTool {
    fn name(&self) -> &str {
        "record_send"
    }

    fn description(&self) -> &str {
        "records a send to a topic"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        ctx: &crate::tools::ToolContext<'_>,
    ) -> anyhow::Result<crate::tools::ToolOutput> {
        ctx.outbound_targets
            .lock()
            .expect("outbound_targets poisoned")
            .push(self.target.clone());
        Ok(crate::tools::ToolOutput::success("sent"))
    }
}

/// Provider that records the raw messages of every call and, on the
/// first call, hands a message to the injection queue — standing in for
/// a user writing while the model is still working.
struct InjectingProvider {
    calls: AtomicUsize,
    seen_raw: Mutex<Vec<Vec<serde_json::Value>>>,
    inject: Mutex<Option<mpsc::Sender<QueueItem>>>,
    /// The item to hand over, when it is not the plain default one.
    inject_item: Mutex<Option<QueueItem>>,
    /// The tool the first call asks for.
    tool_name: Mutex<Option<String>>,
    /// The call that fails, when one should.
    fail_on_call: Mutex<Option<usize>>,
}

#[async_trait]
impl Provider for InjectingProvider {
    fn name(&self) -> &str {
        "inject-test"
    }
    fn model(&self) -> &str {
        "inject-test-1"
    }

    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
    ) -> anyhow::Result<EventStream> {
        unimplemented!("complete() unused in injection tests")
    }

    async fn complete_raw(
        &self,
        raw_messages: &[serde_json::Value],
        _tools: &[ToolDefinition],
        _system: &str,
    ) -> anyhow::Result<EventStream> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen_raw
            .lock()
            .expect("seen_raw lock")
            .push(raw_messages.to_vec());
        if *self.fail_on_call.lock().expect("fail_on_call lock") == Some(call) {
            anyhow::bail!("the provider failed on call {call}");
        }
        if call == 0 {
            // The user writes while the first call is in flight.
            let sender = self.inject.lock().expect("inject lock").take();
            if let Some(tx) = sender {
                let item = self
                    .inject_item
                    .lock()
                    .expect("inject item lock")
                    .take()
                    .unwrap_or_else(|| queue_item("extra info"));
                let _ = tx.send(item).await;
            }
            // A tool call keeps the loop going for another iteration. The
            // tool does not exist — the failure is the point: it moves the
            // loop to its next iteration without touching the disk.
            let events: Vec<anyhow::Result<StreamEvent>> = vec![
                Ok(StreamEvent::ToolUseStart {
                    id: "call-1".to_string(),
                    name: self
                        .tool_name
                        .lock()
                        .expect("tool name lock")
                        .clone()
                        .unwrap_or_else(|| "no_such_tool".to_string()),
                }),
                Ok(StreamEvent::ToolInputDelta("{}".to_string())),
                Ok(StreamEvent::ToolUseEnd),
                Ok(StreamEvent::Done),
            ];
            return Ok(Box::pin(stream::iter(events)));
        }
        // The second call ends the turn with a text reply. It carries the
        // `Done` marker like every provider stream does: a stream that
        // ends without it is a truncated one, and the loop fails the turn
        // instead of shipping half an answer.
        let events: Vec<anyhow::Result<StreamEvent>> = vec![
            Ok(StreamEvent::TextDelta("done".to_string())),
            Ok(StreamEvent::Done),
        ];
        Ok(Box::pin(stream::iter(events)))
    }

    fn format_user_message(&self, blocks: &[ContentBlock]) -> serde_json::Value {
        let text: String = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        serde_json::json!({"role": "user", "content": text})
    }

    fn format_tool_result(
        &self,
        tool_call_id: &str,
        content: &str,
        _is_error: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": content,
        })
    }

    fn build_raw_assistant_message(
        &self,
        text: &str,
        _reasoning: &str,
        _tool_calls: &[(String, String, String)],
    ) -> serde_json::Value {
        serde_json::json!({"role": "assistant", "content": text})
    }
}

/// The drain folds a whole batch in, in arrival order, and skips bodies
/// that carry nothing.
#[tokio::test]
async fn drain_folds_the_whole_batch_in_order() {
    let tmp = TempDir::new().unwrap();
    let topic_path = tmp.path().to_path_buf();
    let topic = "inject-drain-batch";
    jyc_types::state_dir::register(topic, &topic_path.join(".jyc"));

    let (tx, mut rx) = mpsc::channel::<QueueItem>(8);
    tx.send(queue_item("first")).await.unwrap();
    tx.send(queue_item("  ")).await.unwrap();
    tx.send(queue_item("second")).await.unwrap();

    let provider = InjectingProvider {
        calls: AtomicUsize::new(0),
        seen_raw: Mutex::new(Vec::new()),
        inject: Mutex::new(None),
        inject_item: Mutex::new(None),
        tool_name: Mutex::new(None),
        fail_on_call: Mutex::new(None),
    };
    let mut history: Vec<Message> = Vec::new();
    let mut raw_context: Vec<serde_json::Value> = Vec::new();

    let (count, requests) = super::drain_injections(
        &mut rx,
        &mut history,
        &mut raw_context,
        &provider,
        None,
        topic,
        &topic_path,
    );
    assert!(
        requests.is_empty(),
        "none of these messages asked for a reply"
    );

    assert_eq!(count, 2, "the blank body is not a message");
    let user_texts: Vec<String> = history
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| match &m.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => String::new(),
        })
        .collect();
    assert_eq!(
        user_texts,
        vec!["first".to_string(), "second".to_string()],
        "both messages join the conversation, in arrival order"
    );
    assert_eq!(raw_context.len(), 2, "one raw message per folded message");
    assert_eq!(raw_context[0]["content"], "first");
    assert_eq!(raw_context[1]["content"], "second");

    // The transcript shows what the user said.
    let log = std::fs::read_to_string(
        std::fs::read_dir(topic_path.join(".jyc"))
            .expect("state dir")
            .map(|e| e.expect("dir entry").path())
            .find(|p| p.to_string_lossy().ends_with(".jsonl"))
            .expect("chat log file"),
    )
    .expect("chat log");
    assert!(log.contains("first"), "the chat log has the first message");
    assert!(
        log.contains("second"),
        "the chat log has the second message"
    );
}

/// A drain with nothing queued changes nothing.
#[tokio::test]
async fn drain_of_an_empty_queue_is_a_no_op() {
    let tmp = TempDir::new().unwrap();
    let topic_path = tmp.path().to_path_buf();
    let topic = "inject-drain-empty";
    jyc_types::state_dir::register(topic, &topic_path.join(".jyc"));

    let (_tx, mut rx) = mpsc::channel::<QueueItem>(8);
    let provider = InjectingProvider {
        calls: AtomicUsize::new(0),
        seen_raw: Mutex::new(Vec::new()),
        inject: Mutex::new(None),
        inject_item: Mutex::new(None),
        tool_name: Mutex::new(None),
        fail_on_call: Mutex::new(None),
    };
    let mut history: Vec<Message> = Vec::new();
    let mut raw_context: Vec<serde_json::Value> = Vec::new();

    let (count, requests) = super::drain_injections(
        &mut rx,
        &mut history,
        &mut raw_context,
        &provider,
        None,
        topic,
        &topic_path,
    );
    assert!(
        requests.is_empty(),
        "none of these messages asked for a reply"
    );

    assert_eq!(count, 0);
    assert!(history.is_empty());
    assert!(raw_context.is_empty());
}

/// An outbound adapter that strips a quoted tail, the way the email
/// channel strips the quoted history of a reply.
struct QuoteStrippingOutbound;

#[async_trait::async_trait]
impl jyc_types::channel::OutboundAdapter for QuoteStrippingOutbound {
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
        raw_body
            .split("\n> ")
            .next()
            .unwrap_or(raw_body)
            .trim()
            .to_string()
    }
    async fn send_reply(
        &self,
        _original: &InboundMessage,
        _reply_text: &str,
        _topic_path: &std::path::Path,
        _message_dir: &str,
        _attachments: Option<&[jyc_types::channel::OutboundAttachment]>,
    ) -> anyhow::Result<jyc_types::channel::SendResult> {
        Ok(jyc_types::channel::SendResult {
            message_id: "test".to_string(),
        })
    }
    async fn send_message(
        &self,
        _recipient: &str,
        _subject: &str,
        _body: &str,
    ) -> anyhow::Result<jyc_types::channel::SendResult> {
        Ok(jyc_types::channel::SendResult {
            message_id: "test".to_string(),
        })
    }
}

/// The body the model reads is cleaned the way the turn's own body is;
/// the transcript keeps the raw words.
#[tokio::test]
async fn the_model_reads_the_cleaned_body_and_the_log_the_raw_one() {
    let tmp = TempDir::new().unwrap();
    let topic_path = tmp.path().to_path_buf();
    let topic = "inject-drain-clean";
    jyc_types::state_dir::register(topic, &topic_path.join(".jyc"));

    let (tx, mut rx) = mpsc::channel::<QueueItem>(8);
    tx.send(queue_item("extra info\n> earlier message"))
        .await
        .unwrap();

    let provider = InjectingProvider {
        calls: AtomicUsize::new(0),
        seen_raw: Mutex::new(Vec::new()),
        inject: Mutex::new(None),
        inject_item: Mutex::new(None),
        tool_name: Mutex::new(None),
        fail_on_call: Mutex::new(None),
    };
    let mut history: Vec<Message> = Vec::new();
    let mut raw_context: Vec<serde_json::Value> = Vec::new();

    let (count, requests) = super::drain_injections(
        &mut rx,
        &mut history,
        &mut raw_context,
        &provider,
        Some(&QuoteStrippingOutbound),
        topic,
        &topic_path,
    );
    assert!(
        requests.is_empty(),
        "none of these messages asked for a reply"
    );

    assert_eq!(count, 1);
    assert_eq!(
        raw_context[0]["content"], "extra info",
        "the model reads the body without the quoted history"
    );
    let log = std::fs::read_to_string(
        std::fs::read_dir(topic_path.join(".jyc"))
            .expect("state dir")
            .map(|e| e.expect("dir entry").path())
            .find(|p| p.to_string_lossy().ends_with(".jsonl"))
            .expect("chat log file"),
    )
    .expect("chat log");
    assert!(
        log.contains("earlier message"),
        "the transcript keeps the raw words: {log}"
    );
}

/// The loop picks the injected message up before it builds the next
/// call: the model sees it in that call, without a new turn.
#[tokio::test]
async fn an_injected_message_is_in_the_next_call() {
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    jyc_types::state_dir::register("inject-loop-call", &working_dir.join(".jyc"));

    let (tx, mut rx) = mpsc::channel::<QueueItem>(8);
    let provider = InjectingProvider {
        calls: AtomicUsize::new(0),
        seen_raw: Mutex::new(Vec::new()),
        inject: Mutex::new(Some(tx)),
        inject_item: Mutex::new(None),
        tool_name: Mutex::new(None),
        fail_on_call: Mutex::new(None),
    };
    let tools = crate::tools::builtin::create_builtin_registry();

    let config = AgentLoopConfig {
        provider: &provider,
        small_provider: None,
        tools: &tools,
        system_prompt: "test",
        user_blocks: vec![ContentBlock::Text {
            text: "hello".to_string(),
        }],
        working_dir: &working_dir,
        topic_path: &working_dir,
        cancel: CancellationToken::new(),
        topic_name: "inject-loop-call",
        event_bus: None,
        prior_history: vec![],
        prior_raw_context: vec![],
        injection_rx: Some(&mut rx),
        max_iterations: Some(5),
        sse_read_timeout: std::time::Duration::from_secs(60),
        additional_read_roots: vec![],
        additional_write_roots: vec![],
        pattern_inject_images: false,
        outbound: None,
        topic_managers: None,
        current_channel: None,
        outbounds: None,
        context_window: None,
        thinking_enabled: false,
        pricing: None,
        billing_mode: Default::default(),
        billing_dir: None,
        model_label: "inject-test",
        context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
        reply_target: None,
    };

    run(config)
        .await
        .expect("agent loop should run to completion");

    let seen = provider.seen_raw.lock().expect("seen_raw lock").clone();
    assert!(
        seen.len() >= 2,
        "the loop should have called the model twice, saw {} calls",
        seen.len()
    );
    let second_call = &seen[1];
    let texts: Vec<String> = second_call
        .iter()
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()).map(String::from))
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("extra info")),
        "the injected message is in the second call: {texts:?}"
    );
}

/// A message that asked for a reply says so where the model reads it, and
/// the request is collected: the turn owes that topic an answer.
#[tokio::test]
async fn a_drained_reply_request_is_announced_and_collected() {
    let tmp = TempDir::new().unwrap();
    let topic_path = tmp.path().to_path_buf();
    let topic = "inject-drain-reply-request";
    jyc_types::state_dir::register(topic, &topic_path.join(".jyc"));

    let (tx, mut rx) = mpsc::channel::<QueueItem>(8);
    tx.send(reply_request_item("please answer", "agents", "jyc"))
        .await
        .unwrap();

    let provider = InjectingProvider {
        calls: AtomicUsize::new(0),
        seen_raw: Mutex::new(Vec::new()),
        inject: Mutex::new(None),
        inject_item: Mutex::new(None),
        tool_name: Mutex::new(None),
        fail_on_call: Mutex::new(None),
    };
    let mut history: Vec<Message> = Vec::new();
    let mut raw_context: Vec<serde_json::Value> = Vec::new();

    let (count, requests) = super::drain_injections(
        &mut rx,
        &mut history,
        &mut raw_context,
        &provider,
        None,
        topic,
        &topic_path,
    );

    assert_eq!(count, 1);
    assert_eq!(
        requests.len(),
        1,
        "the turn owes an answer to the requester"
    );
    assert_eq!(requests[0].topic, "jyc");
    assert_eq!(requests[0].channel, "agents");

    let text = match &history[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => String::new(),
    };
    assert!(
        text.contains("Reply requested"),
        "the model is told the message wants an answer: {text}"
    );
    assert!(
        text.contains("please answer"),
        "the body still comes through: {text}"
    );
}

/// A message from another topic that asked for nothing names its source
/// and owes no answer.
#[tokio::test]
async fn a_drained_message_names_its_source_without_owing_an_answer() {
    let tmp = TempDir::new().unwrap();
    let topic_path = tmp.path().to_path_buf();
    let topic = "inject-drain-sourced";
    jyc_types::state_dir::register(topic, &topic_path.join(".jyc"));

    let (tx, mut rx) = mpsc::channel::<QueueItem>(8);
    tx.send(sourced_item("fyi", "agents", "jyc")).await.unwrap();

    let provider = InjectingProvider {
        calls: AtomicUsize::new(0),
        seen_raw: Mutex::new(Vec::new()),
        inject: Mutex::new(None),
        inject_item: Mutex::new(None),
        tool_name: Mutex::new(None),
        fail_on_call: Mutex::new(None),
    };
    let mut history: Vec<Message> = Vec::new();
    let mut raw_context: Vec<serde_json::Value> = Vec::new();

    let (count, requests) = super::drain_injections(
        &mut rx,
        &mut history,
        &mut raw_context,
        &provider,
        None,
        topic,
        &topic_path,
    );

    assert_eq!(count, 1);
    assert!(
        requests.is_empty(),
        "nobody asked for an answer, so none is owed"
    );

    let text = match &history[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => String::new(),
    };
    assert!(
        text.contains("**Source:**"),
        "the model is told where the message came from: {text}"
    );
    assert!(text.contains("fyi"), "the body still comes through: {text}");
}

/// A reply request that arrives while the turn is running is owed an
/// answer: the turn hands it to the guard, which forwards one if the turn
/// forgets to answer.
#[tokio::test]
async fn a_reply_request_that_arrives_mid_turn_is_owed_an_answer() {
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    jyc_types::state_dir::register("inject-loop-reply", &working_dir.join(".jyc"));

    let (tx, mut rx) = mpsc::channel::<QueueItem>(8);
    let provider = InjectingProvider {
        calls: AtomicUsize::new(0),
        seen_raw: Mutex::new(Vec::new()),
        inject: Mutex::new(Some(tx)),
        inject_item: Mutex::new(Some(reply_request_item("please answer", "agents", "jyc"))),
        tool_name: Mutex::new(None),
        fail_on_call: Mutex::new(None),
    };
    let tools = crate::tools::builtin::create_builtin_registry();

    let config = AgentLoopConfig {
        provider: &provider,
        small_provider: None,
        tools: &tools,
        system_prompt: "test",
        user_blocks: vec![ContentBlock::Text {
            text: "hello".to_string(),
        }],
        working_dir: &working_dir,
        topic_path: &working_dir,
        cancel: CancellationToken::new(),
        topic_name: "inject-loop-reply",
        event_bus: None,
        prior_history: vec![],
        prior_raw_context: vec![],
        injection_rx: Some(&mut rx),
        max_iterations: Some(5),
        sse_read_timeout: std::time::Duration::from_secs(60),
        additional_read_roots: vec![],
        additional_write_roots: vec![],
        pattern_inject_images: false,
        outbound: None,
        topic_managers: None,
        current_channel: None,
        outbounds: None,
        context_window: None,
        thinking_enabled: false,
        pricing: None,
        billing_mode: Default::default(),
        billing_dir: None,
        model_label: "inject-test",
        context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
        reply_target: None,
    };

    let result = run(config)
        .await
        .expect("agent loop should run to completion");

    assert_eq!(
        result.pending_replies.len(),
        1,
        "the turn owes the requester an answer"
    );
    assert_eq!(result.pending_replies[0].source.topic, "jyc");
    assert_eq!(result.pending_replies[0].source.channel, "agents");
    assert_eq!(
        result.pending_replies[0].sent_before, 0,
        "the arrived request carries the mark of zero: nothing had been sent before it was folded in"
    );

    // And the model was told, in the call that carried the message.
    let seen = provider.seen_raw.lock().expect("seen_raw lock").clone();
    let second_call = &seen[1];
    let texts: Vec<String> = second_call
        .iter()
        .filter_map(|m| m.get("content").and_then(|c| c.as_str()).map(String::from))
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("Reply requested")),
        "the injected request says it wants an answer: {texts:?}"
    );
}

/// A turn that dies hands back the requests it still owed.
///
/// The request arrived mid-turn; the turn died before it could answer it.
/// The loop has to hand it back with the error — the caller cannot see it
/// any other way, and the requester waits forever if nobody does.
#[tokio::test]
async fn a_turn_that_dies_hands_back_the_requests_it_owed() {
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    jyc_types::state_dir::register("inject-loop-dies", &working_dir.join(".jyc"));

    let (tx, mut rx) = mpsc::channel::<QueueItem>(8);
    let provider = InjectingProvider {
        calls: AtomicUsize::new(0),
        seen_raw: Mutex::new(Vec::new()),
        inject: Mutex::new(Some(tx)),
        inject_item: Mutex::new(Some(reply_request_item("please answer", "agents", "jyc"))),
        tool_name: Mutex::new(None),
        fail_on_call: Mutex::new(Some(1)),
    };
    let tools = crate::tools::builtin::create_builtin_registry();

    let config = AgentLoopConfig {
        provider: &provider,
        small_provider: None,
        tools: &tools,
        system_prompt: "test",
        user_blocks: vec![ContentBlock::Text {
            text: "hello".to_string(),
        }],
        working_dir: &working_dir,
        topic_path: &working_dir,
        cancel: CancellationToken::new(),
        topic_name: "inject-loop-dies",
        event_bus: None,
        prior_history: vec![],
        prior_raw_context: vec![],
        injection_rx: Some(&mut rx),
        max_iterations: Some(5),
        sse_read_timeout: std::time::Duration::from_secs(60),
        additional_read_roots: vec![],
        additional_write_roots: vec![],
        pattern_inject_images: false,
        outbound: None,
        topic_managers: None,
        current_channel: None,
        outbounds: None,
        context_window: None,
        thinking_enabled: false,
        pricing: None,
        billing_mode: Default::default(),
        billing_dir: None,
        model_label: "inject-test",
        context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
        reply_target: None,
    };

    let error = run(config).await.expect_err("the turn should die");

    assert_eq!(
        error.pending_replies.len(),
        1,
        "the request that arrived mid-turn is handed back with the error"
    );
    assert_eq!(error.pending_replies[0].source.topic, "jyc");
    assert_eq!(error.pending_replies[0].source.channel, "agents");
    assert!(
        error.error.to_string().contains("the provider failed"),
        "the error says why the turn died: {error}"
    );
}

/// A send the turn made *before* the request was folded in is no answer
/// to it.
///
/// The request arrived while the turn was running, and the turn sent to
/// the asking topic before the request was folded into the conversation:
/// the model had not been shown it yet, so that send was no answer to it.
/// The turn's record of where it sent is cumulative, so without a mark the
/// request would look answered and the requester would hear nothing.
#[tokio::test]
async fn a_send_from_before_the_request_was_folded_in_is_not_its_answer() {
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    jyc_types::state_dir::register("inject-loop-mark", &working_dir.join(".jyc"));

    let (tx, mut rx) = mpsc::channel::<QueueItem>(8);
    let provider = InjectingProvider {
        calls: AtomicUsize::new(0),
        seen_raw: Mutex::new(Vec::new()),
        inject: Mutex::new(Some(tx)),
        inject_item: Mutex::new(Some(reply_request_item("please answer", "agents", "jyc"))),
        tool_name: Mutex::new(Some("record_send".to_string())),
        fail_on_call: Mutex::new(None),
    };
    let mut tools = crate::tools::builtin::create_builtin_registry();
    tools.register(Box::new(RecordingSendTool {
        target: ("agents".to_string(), "jyc".to_string()),
    }));

    let config = AgentLoopConfig {
        provider: &provider,
        small_provider: None,
        tools: &tools,
        system_prompt: "test",
        user_blocks: vec![ContentBlock::Text {
            text: "hello".to_string(),
        }],
        working_dir: &working_dir,
        topic_path: &working_dir,
        cancel: CancellationToken::new(),
        topic_name: "inject-loop-mark",
        event_bus: None,
        prior_history: vec![],
        prior_raw_context: vec![],
        injection_rx: Some(&mut rx),
        max_iterations: Some(5),
        sse_read_timeout: std::time::Duration::from_secs(60),
        additional_read_roots: vec![],
        additional_write_roots: vec![],
        pattern_inject_images: false,
        outbound: None,
        topic_managers: None,
        current_channel: None,
        outbounds: None,
        context_window: None,
        thinking_enabled: false,
        pricing: None,
        billing_mode: Default::default(),
        billing_dir: None,
        model_label: "inject-test",
        context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
        reply_target: None,
    };

    let result = run(config)
        .await
        .expect("agent loop should run to completion");

    // The turn did send to the asking topic — before the request was
    // folded in, so before the model was shown it.
    assert_eq!(
        result.outbound_targets,
        [("agents".to_string(), "jyc".to_string())],
        "the turn sent to the asking topic before the request was folded in"
    );
    assert_eq!(
        result.pending_replies.len(),
        1,
        "the turn owes the requester an answer"
    );
    assert_eq!(
        result.pending_replies[0].sent_before, 1,
        "the request carries the mark of the send that came before it"
    );

    // And the guard still owes the answer: that send is not the answer.
    let owed = crate::service::reply_guard::unanswered_requests(
        &result.pending_replies,
        &result.outbound_targets,
    );
    assert_eq!(
        owed.len(),
        1,
        "a send from before the request was folded in is not its answer"
    );
    assert_eq!(owed[0].topic, "jyc");
}
