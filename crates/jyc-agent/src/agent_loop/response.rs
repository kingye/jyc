//! Response collection from the LLM stream.
//!
//! Extracted from the monolithic `agent_loop.rs`.

use anyhow::Result;
use chrono::Utc;
use futures::StreamExt;

use jyc_core::topic_event::TopicEvent;
use jyc_core::topic_event_bus::TopicEventBusRef;

use crate::types::{ContentBlock, Message, Role, StreamEvent};

use super::{ToolCall, publish_event};

/// Collected response from streaming.
#[derive(Debug, Default)]
pub(crate) struct CollectedResponse {
    pub(crate) text: String,
    pub(crate) reasoning_content: String,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    /// Per-call prompt-cache **read** tokens. For Anthropic, this is
    /// `cache_read_input_tokens`; for every other vendor, the single
    /// `cached_tokens` / `prompt_cache_hit_tokens` field. `0` when
    /// the provider didn't surface cache hits for this call.
    pub(crate) cache_hit_tokens: u64,
    /// Per-call prompt-cache **creation** (write) tokens. Anthropic
    /// and OpenAI (GPT-5.6+) report writes separately from reads;
    /// for every other provider this is `0`.
    pub(crate) cache_creation_tokens: u64,
    /// Per-call reasoning (thinking) tokens — the hidden chain-of-thought
    /// share of the output. Already included in `output_tokens` (and
    /// billed as such); informational only. `0` when the provider
    /// doesn't break it out.
    pub(crate) reasoning_tokens: u64,
}

impl CollectedResponse {
    /// Convert to a Message for internal logic (reply detection, text extraction).
    pub(crate) fn to_message(&self) -> Message {
        let mut content = Vec::new();

        if !self.text.is_empty() {
            content.push(ContentBlock::Text {
                text: self.text.clone(),
            });
        }

        for tc in &self.tool_calls {
            let input: serde_json::Value = serde_json::from_str(&tc.arguments)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            content.push(ContentBlock::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input,
            });
        }

        Message {
            role: Role::Assistant,
            content,
        }
    }

    /// Build the raw provider JSON for this assistant response.
    pub(crate) fn to_raw_message(
        &self,
        provider: &dyn crate::provider::Provider,
    ) -> serde_json::Value {
        let tool_calls: Vec<(String, String, String)> = self
            .tool_calls
            .iter()
            .map(|tc| (tc.id.clone(), tc.name.clone(), tc.arguments.clone()))
            .collect();
        provider.build_raw_assistant_message(&self.text, &self.reasoning_content, &tool_calls)
    }
}

const THINKING_PUBLISH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Collect a streaming response into a complete response.
pub(crate) async fn collect_response(
    stream: crate::provider::EventStream,
    sse_read_timeout: std::time::Duration,
    event_bus: Option<&TopicEventBusRef>,
    topic_name: &str,
    thinking_enabled: bool,
) -> Result<CollectedResponse> {
    let mut response = CollectedResponse::default();
    let mut current_tool_id: Option<String> = None;
    let mut current_tool_name: Option<String> = None;
    let mut current_tool_args = String::new();
    let mut saw_done = false;

    // Throttle Thinking events so we don't flood the event bus.
    let mut last_thinking_publish: Option<std::time::Instant> = None;

    tokio::pin!(stream);

    loop {
        let event = match tokio::time::timeout(sse_read_timeout, stream.next()).await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "SSE stream timed out: no events for {}s",
                    sse_read_timeout.as_secs()
                ));
            }
        };
        match event? {
            StreamEvent::TextDelta(text) => {
                response.text.push_str(&text);
            }
            StreamEvent::ReasoningDelta(text) => {
                response.reasoning_content.push_str(&text);

                // Publish a throttled Thinking event for the dashboard chat pane.
                // Skipped entirely when the user has run `/thinking hide`.
                if thinking_enabled {
                    let now = std::time::Instant::now();
                    let should_publish = match last_thinking_publish {
                        None => true,
                        Some(t) => now.duration_since(t) >= THINKING_PUBLISH_INTERVAL,
                    };
                    if should_publish {
                        last_thinking_publish = Some(now);
                        let text = response.reasoning_content.clone();
                        publish_event(
                            event_bus,
                            TopicEvent::Thinking {
                                topic_name: topic_name.to_string(),
                                text,
                                full_length: response.reasoning_content.len(),
                                timestamp: Utc::now(),
                            },
                        )
                        .await;
                    }
                }
            }
            StreamEvent::ToolUseStart { id, name } => {
                // Flush previous tool call if one is in progress.
                // This handles providers that send multiple tool calls in a
                // single response — the next ToolUseStart arrives before the
                // previous ToolUseEnd, so we must save the previous call now.
                if let (Some(prev_id), Some(prev_name)) =
                    (current_tool_id.take(), current_tool_name.take())
                {
                    response.tool_calls.push(ToolCall {
                        id: prev_id,
                        name: prev_name,
                        arguments: std::mem::take(&mut current_tool_args),
                    });
                }
                current_tool_id = Some(id);
                current_tool_name = Some(name);
            }
            StreamEvent::ToolInputDelta(delta) => {
                current_tool_args.push_str(&delta);
            }
            StreamEvent::ToolUseEnd => {
                if let (Some(id), Some(name)) = (current_tool_id.take(), current_tool_name.take()) {
                    response.tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: std::mem::take(&mut current_tool_args),
                    });
                }
            }
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_hit_tokens,
                cache_creation_tokens,
                reasoning_tokens,
            } => {
                response.input_tokens = input_tokens;
                response.output_tokens += output_tokens;
                response.cache_hit_tokens = cache_hit_tokens;
                response.cache_creation_tokens = cache_creation_tokens;
                response.reasoning_tokens += reasoning_tokens;
            }
            StreamEvent::Done => {
                saw_done = true;
                break;
            }
            StreamEvent::Error(msg) => {
                return Err(anyhow::anyhow!("LLM error: {}", msg));
            }
        }
    }

    // A stream that reaches EOF without the provider's `Done` marker was
    // truncated mid-response (gateway cut, connection drop, proxy timeout).
    // Accepting the partial text as complete lets the text-only
    // auto-delivery fallback ship a broken fragment to the user (#786).
    // Fail instead: the message matches the transient retry patterns
    // ("stream ended"), so `complete_with_retry` re-issues the call.
    if !saw_done {
        return Err(anyhow::anyhow!(
            "SSE stream ended without Done marker (truncated mid-stream)"
        ));
    }

    // Final thinking flush: the throttled publishes inside the stream loop
    // may have skipped the tail of the reasoning stream — emit one last
    // snapshot with the full text so live consumers don't lose the ending.
    if thinking_enabled && !response.reasoning_content.is_empty() {
        publish_event(
            event_bus,
            TopicEvent::Thinking {
                topic_name: topic_name.to_string(),
                text: response.reasoning_content.clone(),
                full_length: response.reasoning_content.len(),
                timestamp: Utc::now(),
            },
        )
        .await;
    }

    // Safety net: flush any pending tool call that was started (ToolUseStart)
    // but never ended (ToolUseEnd). Providers that omit `finish_reason` on
    // the last chunk, or that stream the entire tool call in a single chunk
    // without a subsequent end marker, would otherwise drop the accumulated
    // arguments silently.
    if let (Some(id), Some(name)) = (current_tool_id.take(), current_tool_name.take()) {
        response.tool_calls.push(ToolCall {
            id,
            name,
            arguments: std::mem::take(&mut current_tool_args),
        });
    }

    Ok(response)
}

/// Markers of raw tool-call syntax leaking into the text channel. Models
/// with weak function-calling (via OpenAI-compat adapters) sometimes emit
/// these instead of structured `tool_calls`; left unchecked, the text-only
/// auto-delivery fallback would ship the syntax to the user as a "reply".
///
/// Detection is start-anchored (after leading whitespace): real leaks BEGIN
/// with the raw syntax, while legit replies only ever QUOTE it inside prose
/// or code blocks — rejecting those would break meta-discussion (#786
/// review). Cut-mid-leak fragments with a prose preamble are still caught
/// by the truncated-stream check above (no `Done` marker). A response that
/// matches with no parsed tool calls is a provider format failure, so the
/// iteration is retried rather than delivered.
const LEAKED_TOOL_CALL_MARKERS: &[&str] = &[
    "<call tool=",
    "<parameter name=",
    "<argument key=",
    "</call>",
    "<tool_call>",
    "<antml:invoke",
    // Gateway-wrapped leak: `<response tools="<call tool=...>">`.
    "<response tools=",
];

/// Whether `text` looks like leaked tool-call syntax rather than a
/// user-facing reply. Only meaningful when the response parsed no
/// structured tool calls.
pub(crate) fn looks_like_leaked_tool_call(text: &str) -> bool {
    let text = text.trim_start();
    LEAKED_TOOL_CALL_MARKERS.iter().any(|m| text.starts_with(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn event_stream(events: Vec<StreamEvent>) -> crate::provider::EventStream {
        Box::pin(stream::iter(events.into_iter().map(Ok)))
    }

    #[tokio::test]
    async fn done_stream_collects_text_and_tool_calls() {
        let events = vec![
            StreamEvent::TextDelta("hello ".to_string()),
            StreamEvent::ToolUseStart {
                id: "1".to_string(),
                name: "bash".to_string(),
            },
            StreamEvent::ToolInputDelta("{\"a\":1}".to_string()),
            StreamEvent::ToolUseEnd,
            StreamEvent::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_hit_tokens: 0,
                cache_creation_tokens: 0,
                reasoning_tokens: 0,
            },
            StreamEvent::Done,
        ];
        let r = collect_response(
            event_stream(events),
            std::time::Duration::from_secs(5),
            None,
            "topic",
            false,
        )
        .await
        .expect("stream with Done marker must collect");
        assert_eq!(r.text, "hello ");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "bash");
    }

    #[tokio::test]
    async fn eof_without_done_marker_is_rejected() {
        // A stream that ends (EOF) before `Done` was truncated
        // mid-response; the partial text must not be accepted as complete
        // (#786).
        let events = vec![StreamEvent::TextDelta("half a sentence ".to_string())];
        let err = collect_response(
            event_stream(events),
            std::time::Duration::from_secs(5),
            None,
            "topic",
            false,
        )
        .await
        .expect_err("EOF without Done must fail");
        assert!(
            format!("{err:#}").contains("stream ended"),
            "error should match the transient retry patterns, got: {err:#}"
        );
    }

    #[test]
    fn leaked_tool_call_detection() {
        // Real leaks BEGIN with the raw syntax (after optional whitespace).
        assert!(looks_like_leaked_tool_call(
            "<response tools=\"<call tool=\"bash\""
        ));
        assert!(looks_like_leaked_tool_call(
            "  \n\t<call tool=\"bash\" index=\"1\">"
        ));
        assert!(looks_like_leaked_tool_call("</call> trailing"));
        // Legit replies may QUOTE the syntax mid-prose — the gate must not
        // reject meta-discussion (start-anchored detection).
        assert!(!looks_like_leaked_tool_call(
            "看日志：\n<call tool=\"bash\" index=\"1\">"
        ));
        assert!(!looks_like_leaked_tool_call(
            "比如 `<parameter name=\"command\">` 这种写法。"
        ));
        assert!(!looks_like_leaked_tool_call("普通回复，没有工具调用语法。"));
        assert!(!looks_like_leaked_tool_call(""));
    }
}
