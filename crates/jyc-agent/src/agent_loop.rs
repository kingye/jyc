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

/// Default maximum number of tool-call iterations before giving up.
/// Can be overridden via AgentLoopConfig.max_iterations.
const DEFAULT_MAX_ITERATIONS: usize = 100;

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
    /// Prior conversation history (internal format, for logic).
    pub prior_history: Vec<Message>,
    /// Prior raw context (provider-formatted JSON, for API calls).
    pub prior_raw_context: Vec<serde_json::Value>,
    /// Maximum loop iterations. Defaults to DEFAULT_MAX_ITERATIONS.
    pub max_iterations: Option<usize>,
}

/// Run the agent loop to completion.
///
/// Returns the final text response and metadata about tool usage.
pub async fn run(config: AgentLoopConfig<'_>) -> Result<AgentLoopResult> {
    let AgentLoopConfig {
        provider, tools, system_prompt, user_message,
        working_dir, cancel, thread_name, event_bus, prior_history, prior_raw_context,
        max_iterations,
    } = config;

    let max_iter = max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS);

    // Build internal history: prior context + current message
    let mut history: Vec<Message> = prior_history;
    history.push(Message::user(user_message));

    // Build raw context: prior raw + current user message
    let mut raw_context: Vec<serde_json::Value> = prior_raw_context;
    raw_context.push(provider.format_user_message(user_message));

    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut reply_sent_by_tool = false;
    let mut reply_text_from_tool: Option<String> = None;
    let start_time = Instant::now();

    // Cycle tracking: when iter_in_cycle reaches max_iter, send a progress reply,
    // reset the counter, and continue. No upper bound on cycles.
    let mut iter_in_cycle: usize = 0;
    let mut cycle_count: usize = 0;
    let mut total_iterations: usize = 0;

    // Publish ProcessingStarted
    publish_event(event_bus, ThreadEvent::ProcessingStarted {
        thread_name: thread_name.to_string(),
        message_id: "agent-loop".to_string(),
        timestamp: Utc::now(),
    }).await;

    loop {
        if cancel.is_cancelled() {
            tracing::info!(total_iterations, "Agent loop cancelled");
            break;
        }

        // Check for cycle boundary: send progress reply and reset counter
        if iter_in_cycle >= max_iter {
            cycle_count += 1;
            tracing::info!(
                cycle = cycle_count,
                total_iterations,
                input_tokens = total_input_tokens,
                "Cycle boundary reached, sending progress reply and continuing"
            );

            // Generate progress summary via separate LLM call
            let progress_text = generate_progress_summary(
                provider,
                &raw_context,
                cycle_count,
                total_iterations,
            ).await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Failed to generate progress summary, using fallback");
                format!(
                    "Still working on this task. Cycle {}, ~{} iterations completed. Will continue.",
                    cycle_count, total_iterations
                )
            });

            // Synthetically execute reply_message tool
            let synthetic_call_id = format!("progress-cycle-{}", cycle_count);
            let synthetic_args = serde_json::json!({"message": &progress_text}).to_string();

            // Publish ToolStarted for the progress reply
            publish_event(event_bus, ThreadEvent::ToolStarted {
                thread_name: thread_name.to_string(),
                tool_name: "jyc_reply_reply_message".to_string(),
                input: Some(truncate_str(&synthetic_args, 200)),
                timestamp: Utc::now(),
            }).await;

            let tool_start = Instant::now();
            let ctx = ToolContext { working_dir };
            let synthetic_input: serde_json::Value = serde_json::from_str(&synthetic_args)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            let synthetic_output = match tools.execute("jyc_reply_reply_message", synthetic_input, &ctx).await {
                Ok(output) => output,
                Err(e) => {
                    tracing::warn!(error = %e, "Synthetic reply tool execution failed");
                    ToolOutput::error(format!("Tool error: {e}"))
                }
            };

            publish_event(event_bus, ThreadEvent::ToolCompleted {
                thread_name: thread_name.to_string(),
                tool_name: "jyc_reply_reply_message".to_string(),
                success: !synthetic_output.is_error,
                duration_secs: tool_start.elapsed().as_secs(),
                output: if synthetic_output.is_error { Some(truncate_str(&synthetic_output.content, 200)) } else { None },
                timestamp: Utc::now(),
            }).await;

            // Append synthetic assistant message + tool result to raw_context
            // so the LLM sees the progress was sent and can continue from there.
            let synthetic_tool_calls = vec![(
                synthetic_call_id.clone(),
                "jyc_reply_reply_message".to_string(),
                synthetic_args.clone(),
            )];
            raw_context.push(provider.build_raw_assistant_message(
                "",
                "",
                &synthetic_tool_calls,
            ));
            raw_context.push(provider.format_tool_result(
                &synthetic_call_id,
                &synthetic_output.content,
                synthetic_output.is_error,
            ));

            // Also append to internal history for completeness
            history.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: synthetic_call_id.clone(),
                    name: "jyc_reply_reply_message".to_string(),
                    input: serde_json::json!({"message": &progress_text}),
                }],
            });
            history.push(Message::tool_result(
                &synthetic_call_id,
                &synthetic_output.content,
                synthetic_output.is_error,
            ));

            // Reset iteration counter for next cycle
            iter_in_cycle = 0;
            continue;
        }

        tracing::debug!(
            iteration = total_iterations,
            iter_in_cycle,
            cycle = cycle_count,
            history_len = history.len(),
            raw_context_len = raw_context.len(),
            "Agent loop iteration"
        );

        // 1. Send to LLM using raw context (preserves provider-specific fields)
        let stream = provider
            .complete_raw(&raw_context, &tools.definitions(), system_prompt)
            .await?;

        // 2. Collect the response
        let response = collect_response(stream).await?;

        // Track tokens: input_tokens from last call is the current context size
        // (each call sends full context, so latest = total). Output tokens accumulate.
        if response.input_tokens > 0 {
            total_input_tokens = response.input_tokens;
        }
        total_output_tokens += response.output_tokens;

        // 3. Check for empty response (likely an API error we didn't catch)
        if response.text.is_empty() && response.tool_calls.is_empty() && response.input_tokens == 0 {
            tracing::warn!(
                iteration = total_iterations,
                "LLM returned empty response (no text, no tools, 0 tokens) — possible API error"
            );
        }

        // 4. Add assistant message to internal history AND raw context
        history.push(response.to_message());
        // Only save raw assistant message if it has content or tool_calls
        // (reasoning_content alone is not accepted by DeepSeek on replay)
        if !response.text.is_empty() || !response.tool_calls.is_empty() {
            raw_context.push(response.to_raw_message(provider));
        }

        // 5. If no tool calls, we're done
        if response.tool_calls.is_empty() {
            tracing::info!(
                total_iterations,
                cycle = cycle_count,
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
                raw_context,
            });
        }

        // 6. Execute tool calls
        tracing::info!(
            iteration = total_iterations,
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

            // Add tool result to internal history AND raw context
            history.push(Message::tool_result(
                &tool_call.id,
                &output.content,
                output.is_error,
            ));
            raw_context.push(provider.format_tool_result(
                &tool_call.id,
                &output.content,
                output.is_error,
            ));
        }

        // If reply was sent by tool, we can stop early
        if reply_sent_by_tool {
            tracing::info!(total_iterations, "Reply sent by MCP tool, stopping loop");

            let duration = start_time.elapsed();
            publish_event(event_bus, ThreadEvent::ProcessingCompleted {
                thread_name: thread_name.to_string(),
                message_id: "agent-loop".to_string(),
                success: true,
                duration_secs: duration.as_secs(),
                timestamp: Utc::now(),
            }).await;

            return Ok(AgentLoopResult {
                text: String::new(),
                reply_sent_by_tool: true,
                reply_text_from_tool,
                input_tokens: total_input_tokens,
                output_tokens: total_output_tokens,
                history,
                raw_context,
            });
        }

        // Publish progress (only when continuing the loop)
        let elapsed = start_time.elapsed();
        publish_event(event_bus, ThreadEvent::ProcessingProgress {
            thread_name: thread_name.to_string(),
            elapsed_secs: elapsed.as_secs(),
            activity: "tool execution".to_string(),
            progress: Some(format!(
                "cycle {}, iteration {} ({}), {} tokens",
                cycle_count + 1, total_iterations + 1, iter_in_cycle + 1, total_input_tokens
            )),
            parts_count: total_iterations + 1,
            output_length: total_output_tokens as usize,
            timestamp: Utc::now(),
        }).await;

        iter_in_cycle += 1;
        total_iterations += 1;
    }

    // Loop ended (cancellation only — there's no max-cycles limit)
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
        raw_context,
    })
}

/// Generate a progress summary using a separate LLM call.
///
/// Asks the same provider/model to produce a 2-3 sentence progress update
/// based on the current conversation context. Used at cycle boundaries to
/// inform the user that work is still in progress.
async fn generate_progress_summary(
    provider: &dyn Provider,
    raw_context: &[serde_json::Value],
    cycle_count: usize,
    total_iterations: usize,
) -> Result<String> {
    let summary_system = format!(
        "You are summarizing in-progress work for the user. Based on the conversation so far, \
         write a concise 2-3 sentence progress update in the user's language. Format:\n\
         - What you've done (e.g., \"Implemented X, Y, refactored Z\")\n\
         - What you're still working on\n\
         - End with: \"Will continue and reply again when complete.\" (or equivalent in user's language)\n\n\
         This is progress update #{} after {} iterations of work.\n\n\
         Reply with ONLY the progress text. No preamble, no markdown headers, no tool calls.",
        cycle_count, total_iterations
    );

    // Use complete_raw with no tools — just want a text response
    let stream = provider
        .complete_raw(raw_context, &[], &summary_system)
        .await?;

    let response = collect_response(stream).await?;

    if response.text.is_empty() {
        anyhow::bail!("LLM returned empty progress summary");
    }

    Ok(response.text)
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
    reasoning_content: String,
    tool_calls: Vec<ToolCall>,
    input_tokens: u64,
    output_tokens: u64,
}

impl CollectedResponse {
    /// Convert to a Message for internal logic (reply detection, text extraction).
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

    /// Build the raw provider JSON for this assistant response.
    fn to_raw_message(&self, provider: &dyn crate::provider::Provider) -> serde_json::Value {
        let tool_calls: Vec<(String, String, String)> = self.tool_calls.iter()
            .map(|tc| (tc.id.clone(), tc.name.clone(), tc.arguments.clone()))
            .collect();
        provider.build_raw_assistant_message(
            &self.text,
            &self.reasoning_content,
            &tool_calls,
        )
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
            StreamEvent::ReasoningDelta(text) => {
                response.reasoning_content.push_str(&text);
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
