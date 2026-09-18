use jyc_core::topic_event::TopicEvent;

/// Baseline [`super::AgentLoopConfig`] for agent-loop integration tests:
/// everything a scripted run needs, with the fields tests actually vary
/// (`topic_name`, `event_bus`, `outbound`, `model_label`, `max_iterations`,
/// `reply_target`, `question_hub`, ...) overridden at the call site via
/// struct-update syntax.
pub(super) fn test_config<'a>(
    provider: &'a scripted::ScriptedProvider,
    tools: &'a crate::tools::registry::ToolRegistry,
    working_dir: &'a std::path::Path,
    cancel: tokio_util::sync::CancellationToken,
    topic_name: &'a str,
) -> super::AgentLoopConfig<'a> {
    super::AgentLoopConfig {
        provider,
        small_provider: None,
        tools,
        system_prompt: "test",
        user_blocks: vec![super::ContentBlock::Text {
            text: "hello".to_string(),
        }],
        working_dir,
        topic_path: working_dir,
        cancel,
        topic_name,
        event_bus: None,
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
        model_label: "scripted-test",
        context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
        reply_target: None,
        question_hub: None,
    }
}

/// Scripted LLM provider for agent-loop integration tests: replays a
/// fixed list of `StreamEvent` rounds, one per `complete_raw` call.
pub(super) mod scripted {
    use crate::provider::{EventStream, Provider};
    use crate::types::{ContentBlock, Message, StreamEvent, ToolDefinition};
    use async_trait::async_trait;
    use futures::stream;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock provider that replays a scripted list of responses, one per
    /// `complete_raw` call. Used to drive text-only → reply-tool and
    /// text-only → text-only sequences.
    pub(crate) struct ScriptedProvider {
        /// Per-call round of raw `StreamEvent`s (no `Result` wrapper:
        /// `anyhow::Error` is not `Clone`, and the wrapper is only needed
        /// at stream-construction time).
        pub rounds: Vec<Vec<StreamEvent>>,
        pub calls: AtomicUsize,
        /// Tool names offered on each `complete_raw` call, in call order —
        /// lets tests assert the reply-recovery turn restricts the tool
        /// list to `jyc_reply_message` alone.
        pub seen_tools: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedProvider {
        /// Record the offered tool names and replay the next scripted round.
        fn next_stream(&self, tools: &[ToolDefinition]) -> EventStream {
            self.seen_tools
                .lock()
                .unwrap()
                .push(tools.iter().map(|t| t.name.clone()).collect());
            let i = self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<anyhow::Result<StreamEvent>> = match self.rounds.get(i) {
                Some(round) => round.iter().cloned().map(Ok).collect(),
                None => vec![Ok(StreamEvent::Done)],
            };
            Box::pin(stream::iter(events))
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted-test"
        }
        fn model(&self) -> &str {
            "scripted-test-1"
        }

        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<EventStream> {
            unimplemented!("complete() unused in scripted tests")
        }

        async fn complete_raw(
            &self,
            _raw_messages: &[serde_json::Value],
            tools: &[ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<EventStream> {
            Ok(self.next_stream(tools))
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
}

/// Drain a receiver synchronously to a Vec, with a small grace timeout
/// so any in-flight publishes complete.
pub(super) async fn drain_events(
    rx: &mut tokio::sync::mpsc::Receiver<TopicEvent>,
) -> Vec<TopicEvent> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
            Ok(Some(e)) => out.push(e),
            Ok(None) => break, // sender closed
            Err(_) => break,   // timeout — no more events
        }
    }
    out
}
