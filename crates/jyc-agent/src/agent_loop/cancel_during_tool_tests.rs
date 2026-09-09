use super::*;
use crate::provider::{EventStream, Provider};
use crate::tools::builtin::create_builtin_registry;
use crate::tools::registry::ToolRegistry;
use crate::types::{ContentBlock, Message, StreamEvent, ToolDefinition};
use async_trait::async_trait;
use futures::stream;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Mock provider that emits one `bash sleep 60` tool call, then Done.
///
/// Used to verify that a pre-cancelled token aborts the in-flight
/// bash command via `tokio::process::Child::drop` instead of waiting
/// the full 60 seconds.
struct BashSleepProvider;

#[async_trait]
impl Provider for BashSleepProvider {
    fn name(&self) -> &str {
        "bash-sleep-test"
    }
    fn model(&self) -> &str {
        "bash-sleep-test-1"
    }

    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
    ) -> anyhow::Result<EventStream> {
        unimplemented!("complete() unused in cancel tests")
    }

    async fn complete_raw(
        &self,
        _raw_messages: &[serde_json::Value],
        _tools: &[ToolDefinition],
        _system: &str,
    ) -> anyhow::Result<EventStream> {
        let events: Vec<anyhow::Result<StreamEvent>> = vec![
            Ok(StreamEvent::ToolUseStart {
                id: "1".to_string(),
                name: "bash".to_string(),
            }),
            Ok(StreamEvent::ToolInputDelta(
                r#"{"command":"sleep 60"}"#.to_string(),
            )),
            Ok(StreamEvent::ToolUseEnd),
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

/// Mock provider whose `complete_raw` never resolves — simulates an
/// LLM call still in flight so a mid-call `/cancel` can be exercised.
/// Formatting methods delegate to `BashSleepProvider`.
struct HangingProvider;

#[async_trait]
impl Provider for HangingProvider {
    fn name(&self) -> &str {
        "hanging-test"
    }
    fn model(&self) -> &str {
        "hanging-test-1"
    }

    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
    ) -> anyhow::Result<EventStream> {
        unimplemented!("complete() unused in cancel tests")
    }

    async fn complete_raw(
        &self,
        _raw_messages: &[serde_json::Value],
        _tools: &[ToolDefinition],
        _system: &str,
    ) -> anyhow::Result<EventStream> {
        // Never resolves: the caller's `tokio::select!` must win via cancel.
        std::future::pending::<()>().await;
        unreachable!()
    }

    fn format_user_message(&self, blocks: &[ContentBlock]) -> serde_json::Value {
        BashSleepProvider.format_user_message(blocks)
    }

    fn format_tool_result(
        &self,
        tool_call_id: &str,
        content: &str,
        is_error: bool,
    ) -> serde_json::Value {
        BashSleepProvider.format_tool_result(tool_call_id, content, is_error)
    }

    fn build_raw_assistant_message(
        &self,
        text: &str,
        reasoning: &str,
        tool_calls: &[(String, String, String)],
    ) -> serde_json::Value {
        BashSleepProvider.build_raw_assistant_message(text, reasoning, tool_calls)
    }
}

/// A token cancelled while an LLM call is in flight exits the loop
/// *without* propagating an error, and still publishes
/// `ProcessingCompleted { success: false }`.
///
/// That event is the only signal the inspect server uses to clear its
/// per-topic `is_processing` flag; skipping it left the dashboard
/// stuck at "AI thinking..." forever after a `/cancel`.
#[tokio::test]
async fn cancel_during_llm_call_publishes_processing_completed() {
    use jyc_core::topic_event_bus::{SimpleThreadEventBus, TopicEventBusRef};
    use std::sync::Arc;

    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    let provider = HangingProvider;
    let tools: ToolRegistry = create_builtin_registry();
    let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(256));
    let mut rx = bus.subscribe().await.unwrap();

    let cancel = CancellationToken::new();
    // Fire the cancel after the loop is parked inside `complete_raw`,
    // so it lands in the LLM-call select! rather than the
    // top-of-loop check.
    let cancel_fire = cancel.clone();
    let fire_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel_fire.cancel();
    });

    let timeout_result = tokio::time::timeout(
        Duration::from_secs(5),
        run(AgentLoopConfig {
            provider: &provider,
            small_provider: None,
            tools: &tools,
            system_prompt: "test",
            user_blocks: vec![ContentBlock::Text {
                text: "test".to_string(),
            }],
            working_dir: &working_dir,
            topic_path: &working_dir,
            cancel: cancel.clone(),
            topic_name: "cancel-during-llm",
            event_bus: Some(&bus),
            prior_history: vec![],
            prior_raw_context: vec![],
            max_iterations: Some(10),
            sse_read_timeout: Duration::from_secs(60),
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
            model_label: "",
            context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
            reply_target: None,
        }),
    )
    .await;
    let _ = fire_task.await;

    let result = timeout_result
        .expect("agent loop must exit within 5s")
        .expect("cancellation must not surface as an error");
    assert_eq!(result.text, "", "no reply text after cancellation");
    assert!(!result.reply_sent_by_tool);

    // Drain the bus: a completion event with success=false must be there.
    let mut saw_completed = false;
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        if let TopicEvent::ProcessingCompleted { success, .. } = event {
            assert!(!success, "cancelled round must report success=false");
            saw_completed = true;
        }
    }
    assert!(
        saw_completed,
        "cancel during LLM call must publish ProcessingCompleted"
    );
}

/// A token cancelled mid-tool-execution aborts the in-flight bash tool.
///
/// `bash sleep 60` would otherwise run for 60 s. The cancel races
/// `tools.execute()` via `tokio::select!` and drops the in-flight
/// future — which kills the spawned child via
/// `tokio::process::Child::drop`. We expect the agent to return
/// within a few seconds with no reply text.
///
/// The cancel fires from a separate tokio task after a small delay so
/// it lands during the bash tool's execution (rather than at the
/// line-173 top-of-loop check, which fires before any tool runs).
#[tokio::test]
async fn cancel_during_long_running_tool_returns_quickly() {
    let tmp = TempDir::new().unwrap();
    let working_dir = tmp.path().to_path_buf();
    let provider = BashSleepProvider;
    let mut tools: ToolRegistry = create_builtin_registry();
    // Sanity-check: bash must be registered so the tool call can resolve.
    assert!(tools.has_tool("bash"));

    let cancel = CancellationToken::new();
    // Fire the cancel from a separate task so it lands during the
    // bash tool's execution, not at the line-173 top-of-loop check.
    let cancel_fire = cancel.clone();
    let fire_task = tokio::spawn(async move {
        // Give the LLM call + bash spawn enough time to land inside
        // the select! race. 200 ms is comfortable on slow CI.
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel_fire.cancel();
    });

    let start = Instant::now();
    let timeout_result = tokio::time::timeout(
        Duration::from_secs(5),
        run(AgentLoopConfig {
            provider: &provider,
            small_provider: None,
            tools: &tools,
            system_prompt: "test",
            user_blocks: vec![ContentBlock::Text {
                text: "test".to_string(),
            }],
            working_dir: &working_dir,
            topic_path: &working_dir,
            cancel: cancel.clone(),
            topic_name: "cancel-during-tool",
            event_bus: None,
            prior_history: vec![],
            prior_raw_context: vec![],
            max_iterations: Some(10),
            sse_read_timeout: Duration::from_secs(60),
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
            model_label: "",
            context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
            reply_target: None,
        }),
    )
    .await;
    let elapsed = start.elapsed();

    // Wait for the fire task to complete (it always will — cancel is
    // idempotent and the spawn was infallible).
    let _ = fire_task.await;

    // The agent must have returned within the 5 s timeout.
    let result = timeout_result.expect("agent loop must exit within 5s");

    // The agent loop exits via cancellation mid-tool, so the result is
    // Ok (ProcessingCompleted with success=false). The key contract:
    // it did NOT wait for the 60 s bash timeout.
    assert!(
        elapsed < Duration::from_secs(3),
        "agent loop should exit promptly on cancel, elapsed={:?}",
        elapsed
    );
    let result = result.expect("agent loop should return Ok after cancellation");
    assert_eq!(result.text, "", "no reply text after cancellation");
    assert!(!result.reply_sent_by_tool);
    // Keep `tools` and `provider` alive across the borrow at the call
    // site; both are dropped at end of scope.
    let _ = (&mut tools, &provider);
}
