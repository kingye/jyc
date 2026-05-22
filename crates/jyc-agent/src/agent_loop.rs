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

            // Compact raw_context in place: keep the original task anchor
            // (first user message), drop tool clutter from the cycle, and
            // preserve the synthetic assistant tool_call + tool_result we
            // just appended above so the next cycle starts with a small,
            // valid context. Without this the request size grows
            // unboundedly and providers like DeepSeek return HTTP 400
            // around iteration 200+.
            let summary_before = raw_context.len();
            summarize_raw_context_in_place(
                &mut raw_context,
                &progress_text,
                provider,
            );
            tracing::info!(
                cycle = cycle_count,
                before = summary_before,
                after = raw_context.len(),
                "raw_context summarized at cycle boundary"
            );

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

/// Compact `raw_context` in place at a cycle boundary.
///
/// Strategy:
/// 1. Keep the **first user message** if present — this anchors the original
///    task description so the model doesn't lose context of what it's doing.
/// 2. Drop everything between the first user message and the trailing two
///    entries (the synthetic assistant tool_call + tool_result that the
///    cycle-boundary block just appended).
/// 3. Insert a synthetic user message tagged `<jyc-cycle-summary>` carrying
///    the progress text so the model sees what was accomplished in the
///    previous cycle.
/// 4. Keep the trailing two entries as-is, preserving the alternation
///    invariant providers expect (assistant tool_call → tool result).
///
/// The output is always a small, valid 4-message context regardless of how
/// long the previous cycle was. Without this, providers like DeepSeek return
/// HTTP 400 once the per-request payload grows past their validation limits.
///
/// Caller contract: the last two entries of `raw_context` MUST be the
/// synthetic assistant tool_call and the matching tool_result that the
/// cycle-boundary block produces. If `raw_context` has fewer than 2 entries,
/// the function is a no-op (defensive).
pub(crate) fn summarize_raw_context_in_place(
    raw_context: &mut Vec<serde_json::Value>,
    progress_text: &str,
    provider: &dyn Provider,
) {
    if raw_context.len() < 2 {
        return;
    }

    // Lift the trailing two entries out (synthetic assistant tool_call + tool_result).
    let trailing_tool_result = raw_context.pop().unwrap();
    let trailing_assistant = raw_context.pop().unwrap();

    // Find the first user message in what remains (the original task anchor).
    let first_user = raw_context
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .cloned();

    // Build the summary user message.
    let summary_text = format!(
        "<jyc-cycle-summary>\nProgress so far (auto-generated at cycle boundary):\n\n{}\n</jyc-cycle-summary>",
        progress_text
    );
    let summary_user = provider.format_user_message(&summary_text);

    // Replace raw_context with the compacted version.
    let mut compacted: Vec<serde_json::Value> = Vec::with_capacity(4);
    if let Some(fu) = first_user {
        compacted.push(fu);
    }
    compacted.push(summary_user);
    compacted.push(trailing_assistant);
    compacted.push(trailing_tool_result);

    *raw_context = compacted;
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
mod summarize_raw_context_tests {
    use super::*;
    use crate::provider::Provider;
    use crate::types::{ToolDefinition, Message};
    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::json;

    /// Minimal stub provider for unit-testing `summarize_raw_context_in_place`.
    /// Implements `format_user_message`, `build_raw_assistant_message`, and
    /// `format_tool_result` with the OpenAI shape; everything else is panicking
    /// stubs because the test never invokes them.
    struct StubProvider;

    #[async_trait]
    impl Provider for StubProvider {
        fn name(&self) -> &str { "stub" }
        fn model(&self) -> &str { "stub" }

        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _system: &str,
        ) -> Result<crate::provider::EventStream> {
            panic!("not used in tests")
        }

        fn format_user_message(&self, text: &str) -> serde_json::Value {
            json!({"role": "user", "content": text})
        }

        fn format_tool_result(&self, tool_call_id: &str, content: &str, _is_error: bool) -> serde_json::Value {
            json!({"role": "tool", "tool_call_id": tool_call_id, "content": content})
        }

        fn build_raw_assistant_message(
            &self,
            text: &str,
            _reasoning: &str,
            tool_calls: &[(String, String, String)],
        ) -> serde_json::Value {
            let mut msg = json!({"role": "assistant"});
            if !text.is_empty() {
                msg["content"] = json!(text);
            } else {
                msg["content"] = serde_json::Value::Null;
            }
            if !tool_calls.is_empty() {
                let arr: Vec<serde_json::Value> = tool_calls.iter().map(|(id, name, args)| {
                    json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}})
                }).collect();
                msg["tool_calls"] = json!(arr);
            }
            msg
        }

        async fn complete_raw(
            &self,
            _raw_messages: &[serde_json::Value],
            _tools: &[ToolDefinition],
            _system: &str,
        ) -> Result<crate::provider::EventStream> {
            panic!("not used in tests")
        }
    }

    fn synthetic_pair() -> (serde_json::Value, serde_json::Value) {
        let assistant = json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "progress-cycle-1",
                "type": "function",
                "function": {"name": "jyc_reply_reply_message", "arguments": "{\"message\":\"progress\"}"}
            }]
        });
        let tool_result = json!({
            "role": "tool",
            "tool_call_id": "progress-cycle-1",
            "content": "Reply queued for delivery"
        });
        (assistant, tool_result)
    }

    #[test]
    fn compacts_long_context_to_4_messages() {
        let (assist, tres) = synthetic_pair();
        let mut ctx = vec![
            json!({"role": "user", "content": "fix bug X"}),
            json!({"role": "assistant", "content": "I'll start", "reasoning_content": "old"}),
            json!({"role": "tool", "tool_call_id": "x", "content": "out"}),
            json!({"role": "assistant", "content": null, "tool_calls": [{"id":"y","type":"function","function":{"name":"bash","arguments":"{}"}}]}),
            json!({"role": "tool", "tool_call_id": "y", "content": "more"}),
            assist.clone(),
            tres.clone(),
        ];
        summarize_raw_context_in_place(&mut ctx, "did A then B", &StubProvider);
        assert_eq!(ctx.len(), 4);
        assert_eq!(ctx[0]["role"], "user");
        assert_eq!(ctx[0]["content"], "fix bug X", "first user message must be preserved as task anchor");
        assert_eq!(ctx[1]["role"], "user");
        assert!(ctx[1]["content"].as_str().unwrap().contains("<jyc-cycle-summary>"));
        assert!(ctx[1]["content"].as_str().unwrap().contains("did A then B"));
        // Trailing pair preserved.
        assert_eq!(ctx[2], assist);
        assert_eq!(ctx[3], tres);
    }

    #[test]
    fn preserves_alternation_invariant() {
        // After compaction the sequence must be valid for an OpenAI-style API:
        // user → user → assistant(tool_call) → tool(tool_result).
        // (The two consecutive user messages are acceptable; only assistant→tool
        // alternation matters for tool-call/result pairs.)
        let (assist, tres) = synthetic_pair();
        let mut ctx = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "ack"}),
            assist,
            tres,
        ];
        summarize_raw_context_in_place(&mut ctx, "summary", &StubProvider);
        // Find the tool message; the entry immediately before it must be the
        // assistant carrying the matching tool_call id.
        let tool_idx = ctx.iter().position(|m| m["role"] == "tool").expect("tool present");
        assert!(tool_idx >= 1);
        let prev = &ctx[tool_idx - 1];
        assert_eq!(prev["role"], "assistant");
        let tc_id = &prev["tool_calls"][0]["id"];
        assert_eq!(tc_id, &ctx[tool_idx]["tool_call_id"]);
    }

    #[test]
    fn no_op_when_context_too_short() {
        // Defensive guard: if raw_context has fewer than 2 entries, do nothing
        // (the trailing-pair contract would be violated).
        let mut ctx = vec![json!({"role": "user", "content": "alone"})];
        let original = ctx.clone();
        summarize_raw_context_in_place(&mut ctx, "x", &StubProvider);
        assert_eq!(ctx, original);
    }

    #[test]
    fn handles_missing_first_user() {
        // Pathological: no user message in the context (shouldn't happen in
        // practice, but the helper should not panic). Result should still be
        // a valid 3-message context (summary user + trailing pair).
        let (assist, tres) = synthetic_pair();
        let mut ctx = vec![
            json!({"role": "assistant", "content": "drift"}),
            assist,
            tres,
        ];
        summarize_raw_context_in_place(&mut ctx, "summary", &StubProvider);
        assert_eq!(ctx.len(), 3);
        assert_eq!(ctx[0]["role"], "user");
        assert!(ctx[0]["content"].as_str().unwrap().contains("<jyc-cycle-summary>"));
    }
}

