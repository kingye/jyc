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
    /// is the only vendor that reports writes separately from
    /// reads; for every other provider this is `0`.
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
            StreamEvent::Done => break,
            StreamEvent::Error(msg) => {
                return Err(anyhow::anyhow!("LLM error: {}", msg));
            }
        }
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
