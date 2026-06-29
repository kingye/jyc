//! The core agentic loop.
//!
//! Sends messages to the LLM, detects tool calls, executes them,
//! and loops until the LLM responds with only text (no tool calls).

use anyhow::Result;
use chrono::Utc;
use futures::StreamExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing;

use jyc_core::thread_event::ThreadEvent;
use jyc_core::thread_event_bus::ThreadEventBusRef;

use crate::provider::{Provider, is_transient_sse_error};
use crate::tools::{
    OutboundsMap, ThreadManagersMap, ToolContext, ToolOutput, registry::ToolRegistry,
};
use crate::types::{AgentLoopResult, ContentBlock, Message, Role, StreamEvent, ToolDefinition};

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
    /// First user-turn content blocks (text + optional image attachments).
    /// Use a single `ContentBlock::Text` for text-only prompts.
    pub user_blocks: Vec<ContentBlock>,
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
    /// Additional absolute paths permitted for tools that enforce a path
    /// boundary (currently: `read_image`). Used to allow access to a
    /// configured absolute `[attachments.inbound].save_path` outside
    /// `working_dir`.
    #[allow(dead_code)]
    pub additional_read_roots: Vec<std::path::PathBuf>,
    /// Additional absolute paths permitted for write tools (`write`, `edit`,
    /// `bash`). Configured via per-pattern `write` paths.
    pub additional_write_roots: Vec<std::path::PathBuf>,
    /// Whether the inbound-attachment pattern allows image injection.
    /// Mirrors `inject_inbound_images`: when `false`, the `read_image`
    /// tool should not use vision-fallback mode even if a `VisionClient`
    /// is configured (consistent with `build_user_blocks` behavior).
    pub pattern_inject_images: bool,
    /// Optional outbound adapter for proactive messaging tools (e.g.
    /// `jyc_send_message`). Passed through to `ToolContext` so tools
    /// can send messages directly without signal-file indirection.
    pub outbound: Option<Arc<dyn jyc_types::channel::OutboundAdapter>>,
    /// Cross-channel thread managers keyed by channel name.
    /// Passed through to `ToolContext` so the `jyc_send_to_thread` tool
    /// can inject messages into threads in other channels.
    pub thread_managers: Option<ThreadManagersMap>,
    /// Current channel name, for tools that need source context
    /// (e.g. `jyc_send_to_thread` sets `source_channel` metadata from this).
    pub current_channel: Option<String>,
    /// Cross-channel outbound adapters keyed by channel name.
    /// Passed through to `ToolContext` so the `jyc_send_message` tool can
    /// send proactive messages through any channel's outbound adapter.
    pub outbounds: Option<OutboundsMap>,
}

/// Run the agent loop to completion.
///
/// Returns the final text response and metadata about tool usage.
pub async fn run(config: AgentLoopConfig<'_>) -> Result<AgentLoopResult> {
    let AgentLoopConfig {
        provider,
        small_provider,
        tools,
        system_prompt,
        user_blocks,
        working_dir,
        cancel,
        thread_name,
        event_bus,
        prior_history,
        prior_raw_context,
        max_iterations,
        additional_read_roots,
        additional_write_roots,
        pattern_inject_images,
        outbound,
        thread_managers,
        current_channel,
        outbounds,
    } = config;

    // Provider used for the cycle-boundary progress summary. Falls back to
    // the main provider when `small_model` is unconfigured or its provider
    // failed to construct (logged at construction time in the service).
    let summary_provider: &dyn Provider = small_provider.unwrap_or(provider);

    let max_iter = max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS);

    // Build internal history: prior context + current message
    let mut history: Vec<Message> = prior_history;
    history.push(Message::user_with_blocks(user_blocks.clone()));

    // Build raw context: prior raw + current user message
    let mut raw_context: Vec<serde_json::Value> = prior_raw_context;
    raw_context.push(provider.format_user_message(&user_blocks));

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

    // Guardrail: some providers (e.g., GLM-5.2 via Ark) intermittently
    // generate tool calls with empty arguments, causing every tool to fail
    // with "Missing parameter". The model does not self-correct, leading to
    // an infinite loop. Track consecutive iterations where ALL tool calls
    // had empty arguments; abort after the threshold to avoid wasting tokens.
    const MAX_EMPTY_TOOL_CALL_ITERATIONS: u32 = 3;
    let mut consecutive_empty_tool_iterations: u32 = 0;

    // Publish ProcessingStarted
    publish_event(
        event_bus,
        ThreadEvent::ProcessingStarted {
            thread_name: thread_name.to_string(),
            message_id: "agent-loop".to_string(),
            timestamp: Utc::now(),
        },
    )
    .await;

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
                thread_name,
                event_bus,
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
            let synthetic_args =
                serde_json::json!({"message": &progress_text, "stop_after": false}).to_string();

            publish_event(
                event_bus,
                ThreadEvent::ToolStarted {
                    thread_name: thread_name.to_string(),
                    tool_name: "jyc_reply_message".to_string(),
                    input: Some(synthetic_args.clone()),
                    timestamp: Utc::now(),
                },
            )
            .await;

            let tool_start = Instant::now();
            let mut ctx = ToolContext::with_roots(working_dir, additional_read_roots.clone());
            ctx.additional_write_roots = additional_write_roots.clone();
            ctx.pattern_inject_images = pattern_inject_images;
            ctx.outbound = outbound.clone();
            ctx.thread_managers = thread_managers.clone();
            ctx.current_channel = current_channel.clone();
            ctx.current_thread = Some(thread_name.to_string());
            ctx.outbounds = outbounds.clone();
            let synthetic_input: serde_json::Value = serde_json::from_str(&synthetic_args)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            let synthetic_output = match tools
                .execute("jyc_reply_message", synthetic_input, &ctx)
                .await
            {
                Ok(output) => output,
                Err(e) => {
                    tracing::warn!(error = %e, "Synthetic reply tool execution failed");
                    ToolOutput::error(format!("Tool error: {e}"))
                }
            };

            publish_event(
                event_bus,
                ThreadEvent::ToolCompleted {
                    thread_name: thread_name.to_string(),
                    tool_name: "jyc_reply_message".to_string(),
                    success: !synthetic_output.is_error,
                    duration_secs: tool_start.elapsed().as_secs(),
                    output: if synthetic_output.is_error {
                        Some(synthetic_output.content.clone())
                    } else {
                        None
                    },
                    input: Some(synthetic_args),
                    timestamp: Utc::now(),
                },
            )
            .await;

            // 3. Append the synthetic event to internal `history` for
            //    diagnostics only. `history` is used for chat-log rendering
            //    and is NEVER replayed to the LLM.
            history.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: synthetic_call_id.clone(),
                    name: "jyc_reply_message".to_string(),
                    input: serde_json::json!({"message": &progress_text, "stop_after": false}),
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

        // Publish LLM request started event so the activity panel shows
        // "Thinking..." between tool execution and LLM response.
        publish_event(
            event_bus,
            ThreadEvent::LLMRequestStarted {
                thread_name: thread_name.to_string(),
                iteration: total_iterations,
                timestamp: Utc::now(),
            },
        )
        .await;

        // 1. Send to LLM using raw context (preserves provider-specific fields)
        // 2. Collect the response
        //
        // Wrapped in a bounded retry loop: transient SSE failures (TCP RST
        // mid-stream, body decode glitch, idle timeout) get a few automatic
        // retries with backoff before the thread is failed. See
        // `complete_with_retry` for classifier and policy.
        let response = complete_with_retry(
            provider,
            &raw_context,
            &tools.definitions(),
            system_prompt,
            thread_name,
            event_bus,
        )
        .await?;

        // Track tokens: input_tokens from last call is the current context size
        // (each call sends full context, so latest = total). Output tokens accumulate.
        if response.input_tokens > 0 {
            total_input_tokens = response.input_tokens;
        }
        total_output_tokens += response.output_tokens;

        // 3. Check for empty response (likely an API error we didn't catch)
        if response.text.is_empty() && response.tool_calls.is_empty() && response.input_tokens == 0
        {
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
            publish_event(
                event_bus,
                ThreadEvent::ProcessingCompleted {
                    thread_name: thread_name.to_string(),
                    message_id: "agent-loop".to_string(),
                    success: true,
                    duration_secs: duration.as_secs(),
                    timestamp: Utc::now(),
                },
            )
            .await;

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

        // 5b. Guardrail: detect models that repeatedly generate tool calls
        //     with empty arguments. If ALL tool calls in this iteration have
        //     empty arguments (empty string or "{}"), increment a counter.
        //     After MAX_EMPTY_TOOL_CALL_ITERATIONS consecutive occurrences,
        //     abort the loop to avoid wasting tokens.
        if all_tool_calls_empty(&response.tool_calls) {
            consecutive_empty_tool_iterations += 1;
            if consecutive_empty_tool_iterations >= MAX_EMPTY_TOOL_CALL_ITERATIONS {
                tracing::warn!(
                    consecutive = consecutive_empty_tool_iterations,
                    "Model repeatedly generated tool calls with empty arguments, aborting loop"
                );
                anyhow::bail!(
                    "model generated tool calls with empty arguments for {} consecutive \
                     iterations — this usually indicates the provider does not support \
                     function calling correctly",
                    consecutive_empty_tool_iterations
                );
            }
        } else {
            consecutive_empty_tool_iterations = 0;
        }

        // 6. Execute tool calls
        tracing::info!(
            iteration = total_iterations,
            tool_count = response.tool_calls.len(),
            tools = ?response.tool_calls.iter().map(|tc| tc.name.as_str()).collect::<Vec<_>>(),
            "Executing tool calls"
        );

        let mut ctx = ToolContext::with_roots(working_dir, additional_read_roots.clone());
        ctx.additional_write_roots = additional_write_roots.clone();
        ctx.pattern_inject_images = pattern_inject_images;
        ctx.outbound = outbound.clone();
        ctx.thread_managers = thread_managers.clone();
        ctx.current_channel = current_channel.clone();
        ctx.current_thread = Some(thread_name.to_string());
        ctx.outbounds = outbounds.clone();

        let mut cancelled_during_tools = false;

        for tool_call in &response.tool_calls {
            if cancel.is_cancelled() {
                tracing::info!("Cancelled during tool execution");
                cancelled_during_tools = true;
                break;
            }

            let input: serde_json::Value = serde_json::from_str(&tool_call.arguments)
                .unwrap_or(serde_json::Value::Object(Default::default()));

            // Publish ToolStarted
            publish_event(
                event_bus,
                ThreadEvent::ToolStarted {
                    thread_name: thread_name.to_string(),
                    tool_name: tool_call.name.clone(),
                    input: Some(tool_call.arguments.clone()),
                    timestamp: Utc::now(),
                },
            )
            .await;

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
            publish_event(
                event_bus,
                ThreadEvent::ToolCompleted {
                    thread_name: thread_name.to_string(),
                    tool_name: tool_call.name.clone(),
                    success: !output.is_error,
                    duration_secs: tool_duration.as_secs(),
                    output: if output.is_error || tool_call.name == "edit" {
                        Some(output.content.clone())
                    } else {
                        None
                    },
                    input: Some(tool_call.arguments.clone()),
                    timestamp: Utc::now(),
                },
            )
            .await;

            tracing::debug!(
                tool = %tool_call.name,
                is_error = output.is_error,
                output_len = output.content.len(),
                duration_ms = tool_duration.as_millis(),
                "Tool executed"
            );

            // Check if this was the reply_message tool
            if (tool_call.name.contains("reply_message") || tool_call.name.contains("jyc_reply"))
                && !output.is_error
            {
                if output.stop_after {
                    reply_sent_by_tool = true;
                    // Extract the message text from the tool input
                    if let Some(msg) = input.get("message").and_then(|m| m.as_str()) {
                        reply_text_from_tool = Some(msg.to_string());
                    }
                } else {
                    tracing::info!(
                        tool = %tool_call.name,
                        "Progress reply sent by tool (stop_after=false), continuing loop"
                    );
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

        // If cancelled mid-tool-execution, the assistant message we just added
        // to raw_context has tool_calls whose results were not all appended.
        // This creates a dangling tool_call that the API rejects on the next
        // run (400: "tool_call_ids did not have response messages"). Remove
        // the last assistant message to prevent persisting corrupted context.
        if cancelled_during_tools {
            tracing::warn!(
                "Cancelled during tool execution — removing dangling assistant message from raw_context"
            );
            // Find and remove the last assistant message with tool_calls.
            // It was pushed at line ~349 and is followed only by the tool
            // results that were completed before cancellation.
            if let Some(pos) = raw_context.iter().rposition(|msg| {
                msg.get("role").and_then(|r| r.as_str()) == Some("assistant")
                    && msg
                        .get("tool_calls")
                        .and_then(|t| t.as_array())
                        .is_some_and(|a| !a.is_empty())
            }) {
                // Remove the assistant message and everything after it
                // (partial tool results that reference the dangling call).
                raw_context.truncate(pos);
            }
            // Also remove from internal history: the last assistant message
            // with a ToolUse block and any subsequent tool results.
            if let Some(pos) = history.iter().rposition(|m| {
                m.role == Role::Assistant
                    && m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
            }) {
                history.truncate(pos);
            }
        }

        // Drain any images queued by tools (e.g. `read_image`) during this
        // batch. Emit them as a synthetic user turn so the model sees the
        // image content on the next request. The textual tool_result already
        // landed above; the images ride alongside as separate content blocks
        // in their own user message — required because OpenAI-compatible
        // `role: "tool"` content is a string-only field on most servers.
        let queued_images = ctx.take_pending_images();
        if !queued_images.is_empty() {
            let mut blocks: Vec<ContentBlock> = vec![ContentBlock::Text {
                text: format!(
                    "[{} image(s) loaded by tool — see attached content]",
                    queued_images.len()
                ),
            }];
            for src in queued_images {
                blocks.push(ContentBlock::Image { source: src });
            }
            history.push(Message::user_with_blocks(blocks.clone()));
            raw_context.push(provider.format_user_message(&blocks));
        }

        // If reply was sent by tool, we can stop early
        if reply_sent_by_tool {
            tracing::info!(total_iterations, "Reply sent by MCP tool, stopping loop");

            let duration = start_time.elapsed();
            publish_event(
                event_bus,
                ThreadEvent::ProcessingCompleted {
                    thread_name: thread_name.to_string(),
                    message_id: "agent-loop".to_string(),
                    success: true,
                    duration_secs: duration.as_secs(),
                    timestamp: Utc::now(),
                },
            )
            .await;

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
        publish_event(
            event_bus,
            ThreadEvent::ProcessingProgress {
                thread_name: thread_name.to_string(),
                elapsed_secs: elapsed.as_secs(),
                activity: "tool execution".to_string(),
                progress: Some(format!(
                    "cycle {}, iteration {} ({}), {} tokens",
                    cycle_count + 1,
                    total_iterations + 1,
                    iter_in_cycle + 1,
                    total_input_tokens
                )),
                parts_count: total_iterations + 1,
                output_length: total_output_tokens as usize,
                timestamp: Utc::now(),
            },
        )
        .await;

        iter_in_cycle += 1;
        total_iterations += 1;
    }

    // Loop ended (cancellation only — there's no max-cycles limit)
    let duration = start_time.elapsed();
    publish_event(
        event_bus,
        ThreadEvent::ProcessingCompleted {
            thread_name: thread_name.to_string(),
            message_id: "agent-loop".to_string(),
            success: false,
            duration_secs: duration.as_secs(),
            timestamp: Utc::now(),
        },
    )
    .await;

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
    thread_name: &str,
    event_bus: Option<&ThreadEventBusRef>,
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
    let user_msg = provider.format_user_message(&[ContentBlock::Text { text: joined }]);

    // Same transient-SSE-retry policy as the main loop call.
    let response = complete_with_retry(
        provider,
        &[user_msg],
        &[],
        &summary_system,
        thread_name,
        event_bus,
    )
    .await?;

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
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown");
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
                if let Some(text) = msg.get("content").and_then(|c| c.as_str())
                    && !text.is_empty()
                {
                    out.push_str(": ");
                    out.push_str(text);
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
                                let name =
                                    block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
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

/// Check if all tool calls have empty arguments (empty string, whitespace-only,
/// or `{}`). Used by the guardrail to detect models that generate tool calls
/// without proper arguments. Returns `false` for an empty slice.
fn all_tool_calls_empty(tool_calls: &[ToolCall]) -> bool {
    !tool_calls.is_empty()
        && tool_calls
            .iter()
            .all(|tc| tc.arguments.trim().is_empty() || tc.arguments.trim() == "{}")
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
        let tool_calls: Vec<(String, String, String)> = self
            .tool_calls
            .iter()
            .map(|tc| (tc.id.clone(), tc.name.clone(), tc.arguments.clone()))
            .collect();
        provider.build_raw_assistant_message(&self.text, &self.reasoning_content, &tool_calls)
    }
}

/// Maximum attempts for a single LLM call before failing the thread.
/// Includes the initial attempt — i.e. up to 2 retries after the first try.
const SSE_MAX_ATTEMPTS: u32 = 3;

/// Backoff (milliseconds) before each retry. Indexed by retry number (0-based:
/// the wait BEFORE the 2nd attempt is `[0]`, before the 3rd is `[1]`, etc.).
/// Length must be `SSE_MAX_ATTEMPTS - 1`.
const SSE_RETRY_BACKOFF_MS: &[u64] = &[1000, 2000];

/// Maximum gap (seconds) between SSE events before the stream is considered
/// hung. The reqwest client-level timeout is 300s, but a hung stream where
/// the server opened the connection but never sends data (or stops mid-stream)
/// would block for the full 300s. This per-read timeout catches it in 120s
/// and triggers a retry via the transient-error classifier.
const SSE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Issue one LLM call and collect its streaming response, retrying on
/// transient SSE / network failures.
///
/// On a transient failure (classified by `is_transient_sse_error`):
/// - Sleep with exponential backoff (1s, 2s).
/// - Publish a `SessionStatus { status_type: "retry", attempt: N }` event so
///   the dashboard surfaces the in-progress retry.
/// - Re-issue the entire request (no resume — providers don't support it).
///   Output tokens from the failed attempt are discarded; only the
///   successful attempt's tokens are counted by the caller.
///
/// On a non-transient failure (HTTP 4xx with captured body, malformed
/// arguments, etc.) propagate immediately.
async fn complete_with_retry(
    provider: &dyn Provider,
    raw_context: &[serde_json::Value],
    tools: &[ToolDefinition],
    system_prompt: &str,
    thread_name: &str,
    event_bus: Option<&ThreadEventBusRef>,
) -> Result<CollectedResponse> {
    let mut last_err: anyhow::Error =
        anyhow::anyhow!("complete_with_retry exited without attempting any call");

    for attempt_idx in 0..SSE_MAX_ATTEMPTS {
        let result: Result<CollectedResponse> = async {
            let stream = provider
                .complete_raw(raw_context, tools, system_prompt)
                .await?;
            collect_response(stream).await
        }
        .await;

        match result {
            Ok(r) => return Ok(r),
            Err(e) => last_err = e,
        }

        // Retry decision: must be a known transient error AND we still have
        // attempts remaining.
        let is_last_attempt = attempt_idx + 1 == SSE_MAX_ATTEMPTS;
        if is_last_attempt || !is_transient_sse_error(&last_err) {
            break;
        }

        let backoff_ms = SSE_RETRY_BACKOFF_MS[attempt_idx as usize];
        let next_attempt = attempt_idx + 2; // 1-based attempt # we're about to make
        let err_display = format!("{:#}", last_err);
        let truncated_err = truncate_str(&err_display, 160);

        tracing::warn!(
            attempt = next_attempt,
            max_attempts = SSE_MAX_ATTEMPTS,
            backoff_ms,
            error = %err_display,
            "Transient SSE error, retrying after backoff"
        );

        publish_event(
            event_bus,
            ThreadEvent::SessionStatus {
                thread_name: thread_name.to_string(),
                status_type: "retry".to_string(),
                attempt: Some(next_attempt),
                message: Some(format!(
                    "transient SSE error, retrying ({}/{}): {}",
                    next_attempt, SSE_MAX_ATTEMPTS, truncated_err
                )),
                timestamp: Utc::now(),
            },
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
    }

    Err(last_err)
}

/// Collect a streaming response into a complete response.
async fn collect_response(stream: crate::provider::EventStream) -> Result<CollectedResponse> {
    let mut response = CollectedResponse::default();
    let mut current_tool_id: Option<String> = None;
    let mut current_tool_name: Option<String> = None;
    let mut current_tool_args = String::new();

    tokio::pin!(stream);

    loop {
        let event = match tokio::time::timeout(SSE_READ_TIMEOUT, stream.next()).await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "SSE stream timed out: no events for {}s",
                    SSE_READ_TIMEOUT.as_secs()
                ));
            }
        };
        match event? {
            StreamEvent::TextDelta(text) => {
                response.text.push_str(&text);
            }
            StreamEvent::ReasoningDelta(text) => {
                response.reasoning_content.push_str(&text);
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
            } => {
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

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::provider::{EventStream, Provider};
    use crate::types::{Message, StreamEvent, ToolDefinition};
    use async_trait::async_trait;
    use futures::stream;
    use jyc_core::thread_event_bus::{SimpleThreadEventBus, ThreadEventBusRef};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock provider that fails its first `fail_count` calls with the given
    /// error message, then succeeds with an empty-but-valid stream.
    struct FlakyProvider {
        fail_count: usize,
        fail_message: String,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for FlakyProvider {
        fn name(&self) -> &str {
            "flaky"
        }
        fn model(&self) -> &str {
            "flaky-1"
        }

        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<EventStream> {
            unimplemented!("complete() unused in retry tests")
        }

        async fn complete_raw(
            &self,
            _raw_messages: &[serde_json::Value],
            _tools: &[ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<EventStream> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_count {
                return Err(anyhow::anyhow!("SSE stream error: {}", self.fail_message));
            }
            // Successful stream: one text delta + Done.
            let events: Vec<anyhow::Result<StreamEvent>> = vec![
                Ok(StreamEvent::TextDelta("ok".to_string())),
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

    /// Drain a receiver synchronously to a Vec, with a small grace timeout
    /// so any in-flight publishes complete.
    async fn drain_events(rx: &mut tokio::sync::mpsc::Receiver<ThreadEvent>) -> Vec<ThreadEvent> {
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

    /// Two transient failures then success → returns Ok, publishes 2 retry events.
    #[tokio::test]
    async fn retries_transient_sse_errors_then_succeeds() {
        let provider = FlakyProvider {
            fail_count: 2,
            fail_message: "error decoding response body".to_string(),
            calls: AtomicUsize::new(0),
        };
        let bus: ThreadEventBusRef = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx = bus.subscribe().await.unwrap();

        // Override backoff via a test-fast version: we still pay the real
        // backoff (1s + 2s = 3s). That's fine for a unit test but let's
        // verify the path works regardless. (Fast timers would require
        // tokio's pause/advance which complicates this minimal test.)
        let result =
            complete_with_retry(&provider, &[], &[], "system", "thread-x", Some(&bus)).await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let response = result.unwrap();
        assert_eq!(response.text, "ok");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            3,
            "expected 3 total calls (2 fails + 1 success)"
        );

        let events = drain_events(&mut rx).await;
        let retry_events: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ThreadEvent::SessionStatus {
                    status_type,
                    attempt,
                    ..
                } if status_type == "retry" => Some(*attempt),
                _ => None,
            })
            .collect();
        assert_eq!(
            retry_events,
            vec![Some(2), Some(3)],
            "expected retry events for attempts 2 and 3, got {:?}",
            retry_events
        );
    }

    /// Three transient failures (all attempts exhausted) → Err propagates.
    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let provider = FlakyProvider {
            fail_count: 99, // fail forever
            fail_message: "error decoding response body".to_string(),
            calls: AtomicUsize::new(0),
        };
        let bus: ThreadEventBusRef = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx = bus.subscribe().await.unwrap();

        let result =
            complete_with_retry(&provider, &[], &[], "system", "thread-x", Some(&bus)).await;

        assert!(result.is_err(), "expected Err after exhausting retries");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            SSE_MAX_ATTEMPTS as usize,
            "should have made exactly SSE_MAX_ATTEMPTS calls"
        );

        let events = drain_events(&mut rx).await;
        let retry_count = events
            .iter()
            .filter(|e| matches!(e, ThreadEvent::SessionStatus { status_type, .. } if status_type == "retry"))
            .count();
        assert_eq!(
            retry_count,
            (SSE_MAX_ATTEMPTS - 1) as usize,
            "should publish one retry event per retry (not for the initial attempt or the final failed attempt)"
        );
    }

    /// Non-transient error (HTTP 4xx with captured body) → fails immediately,
    /// no retries, no retry events.
    #[tokio::test]
    async fn non_transient_errors_fail_immediately() {
        let provider = FlakyProvider {
            fail_count: 99,
            fail_message: "invalid request (HTTP 400 body: {\"error\": \"bad payload\"})"
                .to_string(),
            calls: AtomicUsize::new(0),
        };
        let bus: ThreadEventBusRef = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx = bus.subscribe().await.unwrap();

        let result =
            complete_with_retry(&provider, &[], &[], "system", "thread-x", Some(&bus)).await;

        assert!(result.is_err());
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "non-transient error must not retry"
        );

        let events = drain_events(&mut rx).await;
        let retry_count = events
            .iter()
            .filter(|e| matches!(e, ThreadEvent::SessionStatus { status_type, .. } if status_type == "retry"))
            .count();
        assert_eq!(
            retry_count, 0,
            "non-transient errors must not publish retry events"
        );
    }

    /// Regression for the May 26 production failure on bare-metal:
    ///
    /// The SSE stream died mid-flight with a reqwest send-side error
    /// (stale connection from pool, almost certainly), but the diagnostic
    /// re-POST issued by `fetch_error_body` came back HTTP 200 with a
    /// healthy first chunk. The previous classifier wrongly treated ANY
    /// `(HTTP <code> body:)` suffix as terminal and refused to retry,
    /// causing the thread to die after one attempt.
    ///
    /// After this fix, a 2xx diag status confirms the upstream is fine
    /// and the original transport error is transient → retry.
    #[tokio::test]
    async fn diag_2xx_with_send_error_is_retried() {
        let provider = FlakyProvider {
            fail_count: 2,
            fail_message: "error sending request for url \
                (https://api.deepseek.com/chat/completions) \
                (HTTP 200 body: data: {\"id\":\"abc\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null,\"reasoning_content\":\"\"}}]})"
                .to_string(),
            calls: AtomicUsize::new(0),
        };
        let bus: ThreadEventBusRef = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx = bus.subscribe().await.unwrap();

        let result =
            complete_with_retry(&provider, &[], &[], "system", "thread-x", Some(&bus)).await;

        assert!(
            result.is_ok(),
            "diag-200 send-error must be transient and recover, got {:?}",
            result.err()
        );
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            3,
            "expected 2 fails + 1 success"
        );

        let events = drain_events(&mut rx).await;
        let retry_attempts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ThreadEvent::SessionStatus {
                    status_type,
                    attempt,
                    ..
                } if status_type == "retry" => Some(*attempt),
                _ => None,
            })
            .collect();
        assert_eq!(
            retry_attempts,
            vec![Some(2), Some(3)],
            "expected retry events for attempts 2 and 3"
        );
    }
}

#[cfg(test)]
mod guardrail_tests {
    use super::*;

    fn tc(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args.to_string(),
        }
    }

    #[test]
    fn empty_string_args_detected() {
        assert!(all_tool_calls_empty(&[tc("1", "bash", "")]));
    }

    #[test]
    fn empty_object_args_detected() {
        assert!(all_tool_calls_empty(&[tc("1", "bash", "{}")]));
    }

    #[test]
    fn whitespace_only_args_detected() {
        assert!(all_tool_calls_empty(&[tc("1", "bash", "  ")]));
    }

    #[test]
    fn non_empty_args_not_detected() {
        assert!(!all_tool_calls_empty(&[tc(
            "1",
            "bash",
            r#"{"command":"ls"}"#
        )]));
    }

    #[test]
    fn mixed_args_not_all_empty() {
        let calls = [tc("1", "bash", ""), tc("2", "read", r#"{"file_path":"x"}"#)];
        assert!(!all_tool_calls_empty(&calls));
    }

    #[test]
    fn empty_slice_not_detected() {
        assert!(!all_tool_calls_empty(&[]));
    }
}
