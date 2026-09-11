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
/// no tool calls. Used to drive the no-reply path repeatedly.
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

#[tokio::test]
async fn no_reply_emits_event_and_reminds_once_then_exits() {
    let provider = EmptyResponseProvider {
        calls: AtomicUsize::new(0),
    };
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    let tools = crate::tools::builtin::create_builtin_registry();
    let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
    let mut rx = bus.subscribe().await.unwrap();
    let cancel = CancellationToken::new();

    let result = run(super::AgentLoopConfig {
        provider: &provider,
        small_provider: None,
        tools: &tools,
        system_prompt: "test",
        user_blocks: vec![ContentBlock::Text {
            text: "hello".to_string(),
        }],
        working_dir: &working_dir,
        topic_path: &working_dir,
        cancel: cancel.clone(),
        topic_name: "no-reply-test",
        event_bus: Some(&bus),
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
        model_label: "empty-test-1",
        context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
        reply_target: None,
    })
    .await
    .expect("agent loop should run to completion");

    // First call = initial turn; second call = after system-reminder.
    // No third call — the reminder is single-shot.
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "expected exactly one reminder (initial turn + reminder)"
    );

    assert_eq!(result.text, "", "no text should be produced");
    assert!(
        !result.reply_sent_by_tool,
        "reply_sent_by_tool must be false"
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
        2,
        "expected exactly 2 no_reply events (initial + after reminder), got {}",
        no_reply_events.len()
    );
}

/// Mirror of `no_reply_emits_event_and_reminds_once_then_exits` with
/// `jyc_reply_message` registered. Pre-fix the no-reply gate required
/// `!reply_tool_available`, so a registered reply tool let the loop
/// exit silently on the first empty response — common with thinking
/// models that issue tool calls and then return nothing. Post-fix the
/// reminder is injected once regardless of reply-tool availability.
#[tokio::test]
async fn no_reply_reminds_once_when_reply_tool_available() {
    let provider = EmptyResponseProvider {
        calls: AtomicUsize::new(0),
    };
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    let mut tools = crate::tools::builtin::create_builtin_registry();
    crate::tools::mcp_bridge::register_mcp_tools(&mut tools);
    let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
    let mut rx = bus.subscribe().await.unwrap();
    let cancel = CancellationToken::new();

    let result = run(super::AgentLoopConfig {
        provider: &provider,
        small_provider: None,
        tools: &tools,
        system_prompt: "test",
        user_blocks: vec![ContentBlock::Text {
            text: "hello".to_string(),
        }],
        working_dir: &working_dir,
        topic_path: &working_dir,
        cancel: cancel.clone(),
        topic_name: "no-reply-with-tool-test",
        event_bus: Some(&bus),
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
        model_label: "empty-with-tool-test-1",
        context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
        reply_target: None,
    })
    .await
    .expect("agent loop should run to completion");

    // Same shape as the unavailable-tool case: one reminder nudge,
    // then exit. Without the fix the loop would have made only 1 call.
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "expected exactly one reminder (initial turn + reminder), got {}",
        provider.calls.load(Ordering::SeqCst)
    );

    assert_eq!(result.text, "", "no text should be produced");
    assert!(
        !result.reply_sent_by_tool,
        "reply_sent_by_tool must be false"
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
        2,
        "expected exactly 2 no_reply events (initial + after reminder), got {}",
        no_reply_events.len()
    );
}
