use super::event_test_helpers::drain_events;
use super::*;
use crate::provider::{EventStream, Provider};
use crate::types::{Message, StreamEvent, ToolDefinition};
use async_trait::async_trait;
use futures::stream;
use jyc_core::topic_event_bus::{SimpleThreadEventBus, TopicEventBusRef};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

/// Mock provider that returns one final completion per call: empty text,
/// no tool calls. Used to drive the no-reply path.
struct EmptyResponseProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for EmptyResponseProvider {
    fn name(&self) -> &str {
        "empty-test"
    }
    fn model(&self) -> &str {
        "empty-test-1"
    }

    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
    ) -> anyhow::Result<EventStream> {
        unimplemented!("complete() unused in no-reply tests")
    }

    async fn complete_raw(
        &self,
        _raw_messages: &[serde_json::Value],
        _tools: &[ToolDefinition],
        _system: &str,
    ) -> anyhow::Result<EventStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events: Vec<anyhow::Result<StreamEvent>> = vec![Ok(StreamEvent::Done)];
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

/// Mock provider returning fixed text once, then empty responses.
struct TextThenEmptyProvider {
    text: String,
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for TextThenEmptyProvider {
    fn name(&self) -> &str {
        "text-test"
    }
    fn model(&self) -> &str {
        "text-test-1"
    }

    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
    ) -> anyhow::Result<EventStream> {
        unimplemented!("complete() unused in no-reply tests")
    }

    async fn complete_raw(
        &self,
        _raw_messages: &[serde_json::Value],
        _tools: &[ToolDefinition],
        _system: &str,
    ) -> anyhow::Result<EventStream> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let text = if call == 0 {
            self.text.clone()
        } else {
            String::new()
        };
        let events: Vec<anyhow::Result<StreamEvent>> = vec![
            Ok(StreamEvent::TextDelta(text.clone())),
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

fn base_config<'a>(
    provider: &'a dyn Provider,
    tools: &'a crate::tools::registry::ToolRegistry,
    working_dir: &'a std::path::Path,
    topic_name: &'a str,
    bus: &'a TopicEventBusRef,
) -> AgentLoopConfig<'a> {
    AgentLoopConfig {
        provider,
        small_provider: None,
        tools,
        system_prompt: "test",
        user_blocks: vec![ContentBlock::Text {
            text: "hello".to_string(),
        }],
        working_dir,
        topic_path: working_dir,
        cancel: CancellationToken::new(),
        topic_name,
        event_bus: Some(bus),
        prior_history: vec![],
        prior_raw_context: vec![],
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
        auto_reset_threshold: 0.95,
        thinking_enabled: false,
        pricing: None,
        billing_mode: Default::default(),
        billing_dir: None,
        model_label: "test-model",
        context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
        reply_target: None,
        question_hub: None,
    }
}

#[tokio::test]
async fn no_reply_emits_event_and_exits_without_reminder() {
    let provider = EmptyResponseProvider {
        calls: AtomicUsize::new(0),
    };
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    jyc_types::state_dir::register("no-reply-test", &working_dir.join(".jyc"));
    let tools = crate::tools::builtin::create_builtin_registry();
    let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
    let mut rx = bus.subscribe().await.unwrap();

    let result = run(base_config(
        &provider,
        &tools,
        &working_dir,
        "no-reply-test",
        &bus,
    ))
    .await
    .expect("agent loop should run to completion");

    // No reminder nudge: the loop exits on the first empty response.
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "expected a single LLM call with no reminder"
    );

    assert_eq!(result.text, "", "no text should be produced");
    assert!(
        !result.reply_delivered,
        "nothing was delivered (no text produced)"
    );

    let events = drain_events(&mut rx).await;
    let no_reply_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            TopicEvent::SessionStatus { status_type, .. } if status_type == "no_reply" => Some(()),
            _ => None,
        })
        .collect();
    assert_eq!(
        no_reply_events.len(),
        1,
        "expected exactly 1 no_reply event, got {}",
        no_reply_events.len()
    );
}

/// The final assistant text is the reply: with no live reply target it is
/// queued through the `.jyc/reply.md` + `reply-sent.flag` file relay.
#[tokio::test]
async fn final_text_is_queued_via_file_relay() {
    let provider = TextThenEmptyProvider {
        text: "Here is the final answer.".to_string(),
        calls: AtomicUsize::new(0),
    };
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    jyc_types::state_dir::register("relay-delivery-test", &working_dir.join(".jyc"));
    let tools = crate::tools::builtin::create_builtin_registry();
    let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));

    let result = run(base_config(
        &provider,
        &tools,
        &working_dir,
        "relay-delivery-test",
        &bus,
    ))
    .await
    .expect("agent loop should run to completion");

    assert_eq!(result.text, "Here is the final answer.");
    assert!(result.reply_delivered, "final text must be delivered");

    let jyc_dir = jyc_types::state_dir::jyc_dir("relay-delivery-test", &working_dir);
    let reply_md = std::fs::read_to_string(jyc_dir.join("reply.md")).unwrap();
    assert_eq!(reply_md, "Here is the final answer.");
    assert!(jyc_dir.join("reply-sent.flag").exists());
}
