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
    /// Optional smaller/faster provider for ancillary LLM calls (e.g.,
    /// cycle-boundary progress summary). When `None`, the main `provider`
    /// is reused for those calls.
    pub small_provider: Option<&'a dyn Provider>,
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
        provider, small_provider, tools, system_prompt, user_message,
        working_dir, cancel, thread_name, event_bus, prior_history, prior_raw_context,
        max_iterations,
    } = config;

    // Provider used for the cycle-boundary progress summary. Falls back to
    // the main provider when `small_model` is unconfigured or its provider
    // failed to construct (logged at construction time in the service).
    let summary_provider: &dyn Provider = small_provider.unwrap_or(provider);

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

            // 1. Generate the progress text via a separate, isolated LLM call.
            //    This call joins raw_context into a single plain-text
            //    transcript and asks the model to summarize. It is fully
            //    out-of-band: the main loop's `raw_context` is NEVER mutated
            //    by it. That preserves the reasoning_content contract that
            //    DeepSeek's thinking mode requires (every assistant turn that
            //    came from the model must be replayed with its
            //    reasoning_content intact on subsequent requests).
            //
            //    `summary_provider` is the small/fast model from
            //    `[agent].small_model` if configured, else the main provider.
            let progress_text = generate_summary_from_joined_history(
                summary_provider,
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

            // 2. Post the progress reply to the user via the reply tool.
            //    This sends the GitHub comment / IM message. We do NOT push
            //    a synthetic assistant turn into `raw_context` — doing so
            //    would inject an assistant turn the model never produced
            //    (and thus has no reasoning_content), violating DeepSeek's
            //    thinking-mode contract on the next request.
            let synthetic_call_id = format!("progress-cycle-{}", cycle_count);
            let synthetic_args = serde_json::json!({"message": &progress_text}).to_string();

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

            // 3. Append the synthetic event to internal `history` for
            //    diagnostics only. `history` is used for chat-log rendering
            //    and is NEVER replayed to the LLM.
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

            // 4. Reset iteration counter for next cycle. raw_context is
            //    intentionally left unchanged so the next API call replays
            //    the model's own last assistant turn (with reasoning_content)
            //    followed by its tool_result, and the model continues from
            //    where it left off.
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

/// Generate a progress summary using a separate, isolated LLM call.
///
/// The conversation transcript is rendered into a single plain-text string
/// and sent as a single user message. This is intentionally NOT a replay of
/// `raw_context`'s structured messages — that would replay assistant turns
/// with their `reasoning_content` fields, alternation rules, and tool-call
/// schema, which couples the summary call to the main loop's contract.
///
/// Joining to text decouples the call:
/// - No `tool_calls` in the request, so no schema dependency.
/// - No prior assistant turns, so no `reasoning_content` replay requirements
///   (DeepSeek `thinking = enabled` mode requires reasoning_content to be
///   round-tripped on every assistant turn it produced; an isolated text
///   call sidesteps that contract entirely).
/// - The main loop's `raw_context` is untouched.
///
/// Used at cycle boundaries to inform the user that work is still in progress.
async fn generate_summary_from_joined_history(
    provider: &dyn Provider,
    raw_context: &[serde_json::Value],
    cycle_count: usize,
    total_iterations: usize,
) -> Result<String> {
    let summary_system = format!(
        "You are summarizing in-progress work for the user. Based on the transcript below, \
         write a concise 2-3 sentence progress update in the user's language. Format:\n\
         - What you've done (e.g., \"Implemented X, Y, refactored Z\")\n\
         - What you're still working on\n\
         - End with: \"Will continue and reply again when complete.\" (or equivalent in user's language)\n\n\
         This is progress update #{} after {} iterations of work.\n\n\
         Reply with ONLY the progress text. No preamble, no markdown headers, no tool calls.",
        cycle_count, total_iterations
    );

    let joined = render_raw_context_as_text(raw_context);
    let user_msg = provider.format_user_message(&joined);

    let stream = provider
        .complete_raw(&[user_msg], &[], &summary_system)
        .await?;

    let response = collect_response(stream).await?;

    if response.text.is_empty() {
        anyhow::bail!("LLM returned empty progress summary");
    }

    Ok(response.text)
}

/// Render `raw_context` (a list of OpenAI/Anthropic-shaped JSON messages)
/// as a single plain-text transcript suitable for one-shot summarization.
///
/// The output is best-effort and lossy by design — it is consumed only by
/// the summarization LLM call, never replayed to the main loop. Tool calls,
/// tool results, and reasoning_content are flattened into readable lines.
fn render_raw_context_as_text(raw_context: &[serde_json::Value]) -> String {
    let mut out = String::with_capacity(raw_context.len() * 256);
    out.push_str("=== Conversation transcript ===\n\n");
    for msg in raw_context {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("unknown");
        match role {
            "user" => {
                let text = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                if !text.is_empty() {
                    out.push_str("USER: ");
                    out.push_str(text);
                    out.push_str("\n\n");
                }
            }
            "assistant" => {
                out.push_str("ASSISTANT");
                // OpenAI: content as string
                if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                    if !text.is_empty() {
                        out.push_str(": ");
                        out.push_str(text);
                    }
                }
                // Anthropic: content as array of blocks
                if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                    for block in blocks {
                        let t = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match t {
                            "text" => {
                                if let Some(s) = block.get("text").and_then(|x| x.as_str()) {
                                    out.push_str(": ");
                                    out.push_str(s);
                                }
                            }
                            "tool_use" => {
                                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                                out.push_str(&format!("\n  [tool_use: {}]", name));
                            }
                            _ => {}
                        }
                    }
                }
                // OpenAI: tool_calls array
                if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("?");
                        out.push_str(&format!("\n  [tool_call: {}]", name));
                    }
                }
                out.push_str("\n\n");
            }
            "tool" => {
                let text = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
                let truncated = if text.len() > 500 {
                    format!("{}…", &text[..text.floor_char_boundary(500)])
                } else {
                    text.to_string()
                };
                out.push_str("TOOL_RESULT: ");
                out.push_str(&truncated);
                out.push_str("\n\n");
            }
            _ => {}
        }
    }
    out
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

#[cfg(test)]
mod render_raw_context_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_user_assistant_tool_sequence() {
        let ctx = vec![
            json!({"role": "user", "content": "fix bug"}),
            json!({
                "role": "assistant",
                "content": "I'll start",
                "reasoning_content": "thinking...",
                "tool_calls": [{
                    "id": "1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "1", "content": "output"}),
            json!({"role": "assistant", "content": "Done."}),
        ];
        let rendered = render_raw_context_as_text(&ctx);
        assert!(rendered.contains("USER: fix bug"));
        assert!(rendered.contains("ASSISTANT: I'll start"));
        assert!(rendered.contains("[tool_call: bash]"));
        assert!(rendered.contains("TOOL_RESULT: output"));
        assert!(rendered.contains("ASSISTANT: Done."));
    }

    #[test]
    fn truncates_long_tool_results() {
        let long = "x".repeat(2000);
        let ctx = vec![json!({"role": "tool", "tool_call_id": "1", "content": long})];
        let rendered = render_raw_context_as_text(&ctx);
        // Truncation cap is 500 + "…", plus the "TOOL_RESULT: " prefix and trailing newlines.
        assert!(rendered.len() < 700);
        assert!(rendered.contains("…"));
    }

    #[test]
    fn skips_unknown_roles_and_empty_content() {
        let ctx = vec![
            json!({"role": "system", "content": "ignored"}),
            json!({"role": "user", "content": ""}),
            json!({"role": "user", "content": "real"}),
        ];
        let rendered = render_raw_context_as_text(&ctx);
        assert!(!rendered.contains("ignored"));
        assert!(rendered.contains("USER: real"));
    }
}

