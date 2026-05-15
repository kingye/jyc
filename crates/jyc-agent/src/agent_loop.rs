//! The core agentic loop.
//!
//! Sends messages to the LLM, detects tool calls, executes them,
//! and loops until the LLM responds with only text (no tool calls).

use anyhow::Result;
use chrono::Utc;
use futures::StreamExt;
use std::path::Path;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing;

use jyc_core::thread_event::ThreadEvent;
use jyc_core::thread_event_bus::ThreadEventBusRef;

use crate::provider::Provider;
use crate::tools::{ToolContext, ToolOutput, registry::ToolRegistry};
use crate::types::{AgentLoopResult, ContentBlock, Message, Role, StreamEvent};

/// Maximum number of tool-call iterations before giving up.
const MAX_ITERATIONS: usize = 50;

/// Configuration for the agent loop.
pub struct AgentLoopConfig<'a> {
    pub provider: &'a dyn Provider,
    pub tools: &'a ToolRegistry,
    pub system_prompt: &'a str,
    pub user_message: &'a str,
    pub working_dir: &'a Path,
    pub cancel: CancellationToken,
    /// Thread name (for event publishing).
    pub thread_name: &'a str,
    /// Optional event bus for dashboard propagation.
    pub event_bus: Option<&'a ThreadEventBusRef>,
    /// Prior conversation history (from chat_history).
    pub prior_history: Vec<Message>,
}

/// Run the agent loop to completion.
///
/// Returns the final text response and metadata about tool usage.
pub async fn run(config: AgentLoopConfig<'_>) -> Result<AgentLoopResult> {
    let AgentLoopConfig {
        provider, tools, system_prompt, user_message,
        working_dir, cancel, thread_name, event_bus, prior_history,
    } = config;

    // Build history: prior context + current message
    let mut history: Vec<Message> = prior_history;
    history.push(Message::user(user_message));

    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut reply_sent_by_tool = false;
    let mut reply_text_from_tool: Option<String> = None;
    let start_time = Instant::now();

    // Publish ProcessingStarted
    publish_event(event_bus, ThreadEvent::ProcessingStarted {
        thread_name: thread_name.to_string(),
        message_id: "agent-loop".to_string(),
        timestamp: Utc::now(),
    }).await;

    for iteration in 0..MAX_ITERATIONS {
        if cancel.is_cancelled() {
            tracing::info!(iteration, "Agent loop cancelled");
            break;
        }

        tracing::debug!(
            iteration,
            history_len = history.len(),
            "Agent loop iteration"
        );

        // 1. Send to LLM
        let stream = provider
            .complete(&history, &tools.definitions(), system_prompt)
            .await?;

        // 2. Collect the response
        let response = collect_response(stream).await?;

        total_input_tokens += response.input_tokens;
        total_output_tokens += response.output_tokens;

        // 3. Check for empty response (likely an API error we didn't catch)
        if response.text.is_empty() && response.tool_calls.is_empty() && response.input_tokens == 0 {
            tracing::warn!(
                iteration,
                "LLM returned empty response (no text, no tools, 0 tokens) — possible API error"
            );
        }

        // 4. Add assistant message to history
        history.push(response.to_message());

        // 4. If no tool calls, we're done
        if response.tool_calls.is_empty() {
            tracing::info!(
                iteration,
                text_len = response.text.len(),
                "Agent loop complete (text-only response)"
            );

            let duration = start_time.elapsed();
            publish_event(event_bus, ThreadEvent::ProcessingCompleted {
                thread_name: thread_name.to_string(),
                message_id: "agent-loop".to_string(),
                success: true,
                duration_secs: duration.as_secs(),
                timestamp: Utc::now(),
            }).await;

            return Ok(AgentLoopResult {
                text: response.text,
                reply_sent_by_tool,
                reply_text_from_tool,
                input_tokens: total_input_tokens,
                output_tokens: total_output_tokens,
                history,
            });
        }

        // 5. Execute tool calls
        tracing::info!(
            iteration,
            tool_count = response.tool_calls.len(),
            tools = ?response.tool_calls.iter().map(|tc| tc.name.as_str()).collect::<Vec<_>>(),
            "Executing tool calls"
        );

        let ctx = ToolContext { working_dir };

        for tool_call in &response.tool_calls {
            if cancel.is_cancelled() {
                tracing::info!("Cancelled during tool execution");
                break;
            }

            let input: serde_json::Value = serde_json::from_str(&tool_call.arguments)
                .unwrap_or(serde_json::Value::Object(Default::default()));

            // Publish ToolStarted
            let input_preview = truncate_str(&tool_call.arguments, 200);
            publish_event(event_bus, ThreadEvent::ToolStarted {
                thread_name: thread_name.to_string(),
                tool_name: tool_call.name.clone(),
                input: Some(input_preview),
                timestamp: Utc::now(),
            }).await;

            let tool_start = Instant::now();

            let output = match tools.execute(&tool_call.name, input.clone(), &ctx).await {
                Ok(output) => output,
                Err(e) => {
                    tracing::warn!(tool = %tool_call.name, error = %e, "Tool execution failed");
                    ToolOutput::error(format!("Tool error: {e}"))
                }
            };

            let tool_duration = tool_start.elapsed();

            // Publish ToolCompleted
            publish_event(event_bus, ThreadEvent::ToolCompleted {
                thread_name: thread_name.to_string(),
                tool_name: tool_call.name.clone(),
                success: !output.is_error,
                duration_secs: tool_duration.as_secs(),
                output: if output.is_error { Some(truncate_str(&output.content, 200)) } else { None },
                timestamp: Utc::now(),
            }).await;

            tracing::debug!(
                tool = %tool_call.name,
                is_error = output.is_error,
                output_len = output.content.len(),
                duration_ms = tool_duration.as_millis(),
                "Tool executed"
            );

            // Check if this was the reply_message tool
            if tool_call.name.contains("reply_message") || tool_call.name.contains("jyc_reply") {
                if !output.is_error {
                    reply_sent_by_tool = true;
                    // Extract the message text from the tool input
                    if let Some(msg) = input.get("message").and_then(|m| m.as_str()) {
                        reply_text_from_tool = Some(msg.to_string());
                    }
                }
            }

            // Add tool result to history
            history.push(Message::tool_result(
                &tool_call.id,
                &output.content,
                output.is_error,
            ));
        }

        // If reply was sent by tool, we can stop early
        if reply_sent_by_tool {
            tracing::info!(iteration, "Reply sent by MCP tool, stopping loop");

            let duration = start_time.elapsed();
            publish_event(event_bus, ThreadEvent::ProcessingCompleted {
                thread_name: thread_name.to_string(),
                message_id: "agent-loop".to_string(),
                success: true,
                duration_secs: duration.as_secs(),
                timestamp: Utc::now(),
            }).await;

            return Ok(AgentLoopResult {
                text: response.text,
                reply_sent_by_tool: true,
                reply_text_from_tool,
                input_tokens: total_input_tokens,
                output_tokens: total_output_tokens,
                history,
            });
        }

        // Publish progress (only when continuing the loop — not after completion)
        let elapsed = start_time.elapsed();
        publish_event(event_bus, ThreadEvent::ProcessingProgress {
            thread_name: thread_name.to_string(),
            elapsed_secs: elapsed.as_secs(),
            activity: "tool execution".to_string(),
            progress: Some(format!("iteration {}, {} tokens used", iteration + 1, total_input_tokens)),
            parts_count: iteration + 1,
            output_length: total_output_tokens as usize,
            timestamp: Utc::now(),
        }).await;

        // 6. Loop back to LLM with tool results
    }

    tracing::warn!("Agent loop reached maximum iterations ({})", MAX_ITERATIONS);

    let duration = start_time.elapsed();
    publish_event(event_bus, ThreadEvent::ProcessingCompleted {
        thread_name: thread_name.to_string(),
        message_id: "agent-loop".to_string(),
        success: false,
        duration_secs: duration.as_secs(),
        timestamp: Utc::now(),
    }).await;

    Ok(AgentLoopResult {
        text: String::new(),
        reply_sent_by_tool,
        reply_text_from_tool,
        input_tokens: total_input_tokens,
        output_tokens: total_output_tokens,
        history,
    })
}

/// Publish an event to the event bus (if available).
async fn publish_event(event_bus: Option<&ThreadEventBusRef>, event: ThreadEvent) {
    if let Some(bus) = event_bus {
        let _ = bus.publish(event).await;
    }
}

/// Truncate a string to a maximum length.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..s.floor_char_boundary(max_len)])
    }
}

/// A collected tool call from the LLM response.
#[derive(Debug, Clone)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Collected response from streaming.
#[derive(Debug, Default)]
struct CollectedResponse {
    text: String,
    tool_calls: Vec<ToolCall>,
    input_tokens: u64,
    output_tokens: u64,
}

impl CollectedResponse {
    /// Convert to a Message for conversation history.
    fn to_message(&self) -> Message {
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
}

/// Collect a streaming response into a complete response.
async fn collect_response(
    stream: crate::provider::EventStream,
) -> Result<CollectedResponse> {
    let mut response = CollectedResponse::default();
    let mut current_tool_id: Option<String> = None;
    let mut current_tool_name: Option<String> = None;
    let mut current_tool_args = String::new();

    tokio::pin!(stream);

    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::TextDelta(text) => {
                response.text.push_str(&text);
            }
            StreamEvent::ToolUseStart { id, name } => {
                current_tool_id = Some(id);
                current_tool_name = Some(name);
                current_tool_args.clear();
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
            StreamEvent::Usage { input_tokens, output_tokens } => {
                response.input_tokens = input_tokens;
                response.output_tokens += output_tokens;
            }
            StreamEvent::Done => break,
            StreamEvent::Error(msg) => {
                return Err(anyhow::anyhow!("LLM error: {}", msg));
            }
        }
    }

    Ok(response)
}
