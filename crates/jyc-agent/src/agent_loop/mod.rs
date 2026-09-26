//! The core agentic loop.
//!
//! Sends messages to the LLM, detects tool calls, executes them,
//! and loops until the LLM responds with only text (no tool calls).

use anyhow::Result;
use chrono::Utc;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing;

use jyc_core::topic_event::TopicEvent;
use jyc_core::topic_event_bus::TopicEventBusRef;

use jyc_types::channel::ContextStrategyConfig;

use crate::provider::Provider;
use crate::tools::{
    OutboundsMap, ToolContext, ToolOutput, TopicManagersMap, registry::ToolRegistry,
};
use crate::types::{AgentLoopResult, ContentBlock, Message, Role};

/// Default maximum number of tool-call iterations before giving up.
/// Can be overridden via AgentLoopConfig.max_iterations.
const DEFAULT_MAX_ITERATIONS: usize = 100;

/// Interval between `TopicEvent::LoopTick` heartbeats while the agent
/// loop is running. 1 s = 1 Hz — coarse on purpose so the WS bus and the
/// dashboard's render loop don't churn. The dashboard re-renders on every
/// tick; at 1 Hz that's once per second, which matches the cadence of
/// the OS-level progress indicators (activity monitor, `top`, etc.) the
/// user is used to. The very first tick fires at t=0 (see
/// `run_ticker`), so a short sub-second loop still produces one event.
const LOOP_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

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
    /// Topic directory on disk. Used to persist token counts after every
    /// LLM call via `session::persist_tokens`. The post-loop
    /// `update_tokens` is still the owner of the auto-reset.
    pub topic_path: &'a Path,
    pub cancel: CancellationToken,
    /// Topic name (for event publishing).
    pub topic_name: &'a str,
    /// Optional event bus for dashboard propagation.
    pub event_bus: Option<&'a TopicEventBusRef>,
    /// Prior conversation history (internal format, for logic).
    pub prior_history: Vec<Message>,
    /// Prior raw context (provider-formatted JSON, for API calls).
    pub prior_raw_context: Vec<serde_json::Value>,
    /// Maximum loop iterations. Defaults to DEFAULT_MAX_ITERATIONS.
    pub max_iterations: Option<usize>,
    /// SSE read timeout — maximum gap between SSE events before the stream
    /// is considered hung. Defaults to 120 seconds.
    pub sse_read_timeout: std::time::Duration,
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
    /// Cross-channel topic managers keyed by channel name.
    /// Passed through to `ToolContext` so the `jyc_send_to_topic` tool
    /// can inject messages into topics in other channels.
    pub topic_managers: Option<TopicManagersMap>,
    /// Current channel name, for tools that need source context
    /// (e.g. `jyc_send_to_topic` sets `source_channel` metadata from this).
    pub current_channel: Option<String>,
    /// Cross-channel outbound adapters keyed by channel name.
    /// Passed through to `ToolContext` so the `jyc_send_message` tool can
    /// send proactive messages through any channel's outbound adapter.
    pub outbounds: Option<OutboundsMap>,
    /// Context window size in tokens for mid-loop token check.
    /// When the total input tokens exceed `context_window * auto_reset_threshold`,
    /// the raw context is compressed in-memory before the next LLM call.
    pub context_window: Option<u64>,
    /// Auto-reset threshold as a fraction of context window (0.0~1.0).
    /// Default: 0.95.
    pub auto_reset_threshold: f64,
    /// Whether to publish `TopicEvent::Thinking` events for dashboard display.
    /// Controlled by the `/thinking show/hide` command. Default: `true`.
    pub thinking_enabled: bool,
    /// Billing rates for the active model. `None` when the model has no
    /// configured `pricing`, in which case no cost is computed and nothing
    /// is written to the ledger.
    pub pricing: Option<jyc_types::ModelPricing>,
    /// How the active model's provider is paid for. Provider-level: a
    /// subscription plan covers every model under the provider.
    pub billing_mode: jyc_types::config::BillingMode,
    /// Central billing ledger directory (`<data_home>/billing`), resolved
    /// once at construction. `None` drops billing writes with a warning.
    pub billing_dir: Option<std::path::PathBuf>,
    /// Model identifier (`"provider/model"`) recorded on each ledger entry.
    /// Only used for billing, so an empty string is harmless when
    /// `pricing` is `None`.
    pub model_label: &'a str,
    /// Context management strategy. Controls how prior conversation history
    /// is shaped before being sent to the LLM. The on-disk
    /// `.jyc/agent-context.json` is always the full raw context; this
    /// field only affects the wire payload.
    pub context_strategy: ContextStrategyConfig,
    /// Synchronous delivery target for `jyc_reply_message`. Passed through
    /// to `ToolContext`; `None` in contexts without a live inbound message
    /// (tests, sub-agents), where the reply tool falls back to the
    /// `reply.md`/`reply-sent.flag` file relay.
    pub reply_target: Option<crate::tools::ReplyTarget>,
    /// Shared question/answer registry for the `ask_user` tool. Passed
    /// through to `ToolContext`; `None` disables interactive questions
    /// (the tool then reports unavailability to the model).
    pub question_hub: Option<std::sync::Arc<jyc_core::question::QuestionHub>>,
}

/// Run the agent loop to completion.
///
/// Returns the final text response and metadata about tool usage.
pub async fn run(config: AgentLoopConfig<'_>) -> Result<AgentLoopResult> {
    let AgentLoopConfig {
        provider,
        small_provider: _,
        // `small_provider` is consumed by the service (context-reset
        // summaries), not by the loop itself.
        tools,
        system_prompt,
        user_blocks,
        working_dir,
        topic_path,
        cancel,
        topic_name,
        event_bus,
        prior_history,
        prior_raw_context,
        max_iterations,
        sse_read_timeout,
        additional_read_roots,
        additional_write_roots,
        pattern_inject_images,
        outbound,
        topic_managers,
        current_channel,
        outbounds,
        context_window,
        auto_reset_threshold,
        thinking_enabled,
        pricing,
        billing_mode,
        billing_dir,
        model_label,
        context_strategy,
        reply_target,
        question_hub,
    } = config;

    // Topic label stamped on every billing entry this loop writes.
    let billing_label =
        jyc_core::billing_log_store::BillingLogStore::label_for(topic_name, topic_path);

    let max_iter = max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS);

    // Build internal history: prior context + current message
    let mut history: Vec<Message> = prior_history;
    history.push(Message::user_with_blocks(user_blocks.clone()));

    // Build raw context: prior raw + current user message. The full context
    // is persisted to `.jyc/agent-context.json` unchanged at the end of the
    // loop; the strategy decides what is sent to the LLM (see
    // `build_send_context`).
    let prior_len = prior_raw_context.len();
    let mut raw_context: Vec<serde_json::Value> = prior_raw_context;
    raw_context.push(provider.format_user_message(&user_blocks));

    let mut context_input_tokens: u64 = 0;
    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    // Sum of every LLM call's prompt-cache-hit tokens in this round.
    // Mirrors `total_input_tokens`; zeroed by callers on session reset
    // and surfaced to the dashboard as `total_cache_hit_tokens`.
    let mut total_cache_hit_tokens: u64 = 0;
    // Sum of every LLM call's prompt-cache-**creation** (write)
    // tokens in this round. Anthropic and OpenAI (GPT-5.6+) report
    // writes separately; for every other vendor this stays at `0`.
    // Surfaced to the dashboard as `total_cache_creation_tokens`.
    let mut total_cache_creation_tokens: u64 = 0;
    // Sum of every LLM call's reasoning (thinking) tokens in this round.
    // Informational only — already included in `total_output_tokens`.
    let mut total_reasoning_tokens: u64 = 0;
    let mut reply_delivered = false;

    // Shared ToolContext for tool execution. Built once: every field is
    // static for the duration of the loop. The `context_browse` snapshot
    // below mutates `ctx.raw_context` per batch.
    let mut ctx = ToolContext::with_roots(working_dir, additional_read_roots.clone());
    ctx.additional_write_roots = additional_write_roots.clone();
    ctx.pattern_inject_images = pattern_inject_images;
    ctx.outbound = outbound.clone();
    ctx.topic_managers = topic_managers.clone();
    ctx.current_channel = current_channel.clone();
    ctx.current_topic = Some(topic_name.to_string());
    ctx.outbounds = outbounds.clone();
    ctx.reply_target = reply_target.clone();
    ctx.question_hub = question_hub.clone();
    let start_time = Instant::now();

    // RAII guard: the spawned ticker task is terminated on every return
    // path (success, error, cancel, no-reply guard, etc.). Without this,
    // the ticker leaks on natural completion — the topic-level cancel
    // token only fires on explicit `/cancel` or shutdown.
    let _ticker_guard = if let Some(bus) = event_bus {
        let ticker_cancel = cancel.child_token();
        let handle = run_ticker(
            start_time,
            LOOP_TICK_INTERVAL,
            ticker_cancel.clone(),
            Some(bus),
            topic_name.to_string(),
        );
        Some(TickerGuard::new(handle, ticker_cancel))
    } else {
        None
    };

    // Cycle tracking: when iter_in_cycle reaches max_iter, send a heartbeat
    // reply, reset the counter, and continue. No upper bound on cycles.
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
        TopicEvent::ProcessingStarted {
            topic_name: topic_name.to_string(),
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

        // Check for cycle boundary: send a heartbeat reply and reset the
        // counter. Canned text on purpose — the LLM must not be interrupted
        // mid-task, and the heartbeat only signals liveness on long turns.
        if iter_in_cycle >= max_iter {
            cycle_count += 1;
            tracing::info!(
                cycle = cycle_count,
                total_iterations,
                input_tokens = context_input_tokens,
                "Cycle boundary reached, sending heartbeat reply and continuing"
            );

            let heartbeat = format!(
                "Still working on this task — cycle {cycle_count}, \
                 ~{total_iterations} tool iterations completed. \
                 I will send the full reply when done."
            );
            match crate::tools::deliver_reply(&ctx, tools.hooks(), topic_name, &heartbeat).await {
                Ok(delivery) => {
                    if delivery.direct {
                        publish_reply_sent(event_bus, topic_name, &heartbeat).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Heartbeat reply delivery failed");
                }
            }

            // Reset the iteration counter for the next cycle. raw_context is
            // intentionally left unchanged so the next API call replays the
            // model's own last assistant turn (with reasoning_content)
            // followed by its tool_result, and the model continues from
            // where it left off.
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
            TopicEvent::LLMRequestStarted {
                topic_name: topic_name.to_string(),
                iteration: total_iterations,
                timestamp: Utc::now(),
            },
        )
        .await;

        // 1. Send to LLM using raw context (preserves provider-specific fields)
        // 2. Collect the response
        //
        // `send_context` is the strategy-shaped view of `raw_context` used
        // for the wire payload. The full `raw_context` continues to
        // accumulate current-turn messages and is what we persist at the
        // end of the loop — the strategy only changes what the LLM sees.
        //
        // `send_regions` is the parallel Vec<u8> of per-message region
        // labels (1=compacted history / Full-mode, 2=verbatim, 3=current
        // turn) — used only by the wire-payload debug dump.
        let (send_context, send_regions) =
            crate::agent_loop::context::build_send_context_with_regions(
                provider,
                &raw_context,
                prior_len,
                &context_strategy,
            );
        // Optional debug dump: when the user runs `/context dump on`, append
        // one JSON line per LLM call to `<topic>/.jyc/wire-payload.jsonl`
        // (capped at 50 lines). Best-effort — failures are not fatal.
        if crate::session::read_wire_payload_dump_enabled(topic_name, topic_path).await {
            crate::session::append_wire_payload_dump(
                topic_name,
                topic_path,
                total_iterations,
                &context_strategy,
                &send_regions,
                send_context.as_ref(),
            )
            .await;
        }
        //
        // Wrapped in a bounded retry loop: transient SSE failures (TCP RST
        // mid-stream, body decode glitch, idle timeout) get a few automatic
        // retries with backoff before the topic is failed. See
        // `complete_with_retry` for classifier and policy.
        //
        // A failure while `cancel` is already fired is a user-initiated
        // `/cancel`, not an error: break so the post-loop
        // `ProcessingCompleted` event still fires (the dashboard clears its
        // "AI thinking" state only on that event).
        let tool_defs = tools.definitions();

        let mut response = match complete_with_retry(
            provider,
            &send_context,
            &tool_defs,
            system_prompt,
            topic_name,
            event_bus,
            sse_read_timeout,
            &cancel,
            thinking_enabled,
            SSE_RETRY_BACKOFF_MS,
        )
        .await
        {
            Ok(r) => r,
            Err(e) if cancel.is_cancelled() => {
                tracing::info!(total_iterations, error = %e, "Agent loop cancelled during LLM call");
                break;
            }
            Err(e) => return Err(e),
        };

        // Track tokens across LLM calls in this round:
        // - `context_input_tokens` = input tokens from the most recent LLM call
        //   (current context size, since each call sends full context).
        // - `total_input_tokens` / `total_output_tokens` = running sums across
        //   every call in this round. Each call's `input_tokens` (= full context
        //   size) is added via `+=`, so `total_input_tokens` also represents the
        //   lifetime tokens billed as input by the API for this round.
        if response.input_tokens > 0 {
            context_input_tokens = response.input_tokens;
        }
        total_input_tokens += response.input_tokens;
        total_output_tokens += response.output_tokens;
        total_cache_hit_tokens += response.cache_hit_tokens;
        total_cache_creation_tokens += response.cache_creation_tokens;
        total_reasoning_tokens += response.reasoning_tokens;

        // Bill this call from its own usage payload, before anything can
        // reset or overwrite the round's counters. Doing it per call (rather
        // than once post-loop) means a round that is cancelled or errors out
        // still keeps the cost of the calls that did complete, and a
        // mid-round model switch bills each call at its own rate.
        let call_cost = bill_call(
            pricing.as_ref(),
            billing_mode,
            billing_dir.as_deref(),
            &billing_label,
            model_label,
            jyc_core::billing_log_store::KIND_CALL,
            response.input_tokens,
            response.output_tokens,
            response.cache_hit_tokens,
            response.cache_creation_tokens,
        );

        // Mid-loop token check: if the current context size (last call's
        // input_tokens) exceeds the threshold, compress raw_context
        // in-memory to prevent API 400 on the next call.
        if let Some(cw) = context_window
            && context_input_tokens >= (cw as f64 * auto_reset_threshold) as u64
        {
            let before_count = raw_context.len();
            let before_tokens = context_input_tokens;

            // Apply heuristic compaction: keep last 3 user+assistant pairs.
            // Uses a fixed keep_pairs=3 because mid-loop compression is a
            // safety mechanism to prevent API 400 on the next LLM call — it is
            // NOT equivalent to the user-configured compression strategy
            // (ResetCompressionConfig.keep_pairs), which is used on explicit
            // session resets (/reset, dashboard, between-message auto-reset).
            raw_context = compact_raw_context_heuristic(&raw_context, 3);
            // Also compact internal history to match raw_context
            history = compact_history_heuristic(&history, 3);

            // Reset token counter after compression
            context_input_tokens = 0;

            tracing::info!(
                before_messages = before_count,
                after_messages = raw_context.len(),
                before_tokens,
                context_window = cw,
                threshold = auto_reset_threshold,
                "Mid-loop context compressed to prevent token overflow"
            );

            publish_event(
                event_bus,
                TopicEvent::SessionStatus {
                    topic_name: topic_name.to_string(),
                    status_type: "session_reset".to_string(),
                    attempt: None,
                    message: Some(format!(
                        "mid-loop compression: {before_count}→{} msgs, {before_tokens}→0 tokens",
                        raw_context.len()
                    )),
                    timestamp: Utc::now(),
                },
            )
            .await;
        }

        // Persist latest token counts to disk so dashboard polls see fresh
        // data mid-round. Called AFTER the mid-loop compression block so
        // the on-disk value reflects the post-compression state. Does not
        // trigger auto-reset — that decision belongs to the post-loop
        // `update_tokens` call in `service.rs`.
        crate::session::persist_tokens(
            topic_name,
            topic_path,
            context_input_tokens,
            total_input_tokens,
            total_output_tokens,
            total_cache_hit_tokens,
            total_cache_creation_tokens,
            total_reasoning_tokens,
            context_window,
            auto_reset_threshold,
            call_cost,
        )
        .await;

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

        // 5. If no tool calls, the turn is over: the final assistant text
        //    IS the reply — deliver it (directly or via the file relay).
        if response.tool_calls.is_empty() {
            // Trimmed, so whitespace-only narration counts as empty: nothing
            // is delivered.
            let text_len = response.text.trim().len();

            // No-reply state: the model produced no text and no tool call,
            // so the user will see nothing. Surface it via a SessionStatus
            // event (common with thinking models that end a long tool
            // sequence with an empty response).
            if text_len == 0 {
                tracing::warn!(total_iterations, "Agent loop: no-reply, exiting");
                publish_event(
                    event_bus,
                    TopicEvent::SessionStatus {
                        topic_name: topic_name.to_string(),
                        status_type: "no_reply".to_string(),
                        attempt: None,
                        message: Some(format!(
                            "AI produced no text and no tool call in final iteration \
                             (total_iterations={total_iterations}) — user will see no reply"
                        )),
                        timestamp: Utc::now(),
                    },
                )
                .await;
            } else {
                tracing::info!(
                    total_iterations,
                    cycle = cycle_count,
                    text_len,
                    "Agent loop complete (text-only response)"
                );
            }

            // Embedded-question shim: models with weak function-calling
            // sometimes write the `ask_user` call as XML in the reply text
            // instead of emitting a native tool call. Recover a well-formed
            // tag — deliver the prose first, block on the question, then
            // continue the turn with the answer as a synthetic tool result.
            // Malformed tags are stripped (all of them) so raw syntax never
            // ships to the user; the remaining prose falls through to
            // normal delivery below.
            while let Some(embedded_ask::EmbeddedAsk::Malformed { span }) =
                embedded_ask::find_embedded_ask(&response.text)
            {
                tracing::warn!("Malformed <ask_user> tag in reply text; stripping it");
                response.text = embedded_ask::remove_span(&response.text, span);
            }
            if let Some(embedded_ask::EmbeddedAsk::WellFormed {
                span,
                question,
                options,
                timeout_secs,
            }) = embedded_ask::find_embedded_ask(&response.text)
            {
                let prose = embedded_ask::remove_span(&response.text, span);
                if !prose.trim().is_empty() {
                    deliver_progress_text(tools, &ctx, event_bus, topic_name, &prose).await;
                }
                let input = serde_json::json!({
                    "question": question,
                    "options": options,
                    "timeout_seconds": timeout_secs,
                });
                let output = match tools.execute("ask_user", input, &ctx).await {
                    Ok(output) => output,
                    Err(e) => {
                        tracing::warn!(error = %e, "Embedded ask_user execution failed");
                        ToolOutput::error(format!("Tool error: {e}"))
                    }
                };
                history.push(Message::tool_result(
                    "embedded-ask-user",
                    &output.content,
                    output.is_error,
                ));
                raw_context.push(provider.format_tool_result(
                    "embedded-ask-user",
                    &output.content,
                    output.is_error,
                ));
                continue;
            }

            // Deliver the final text as the turn's reply.
            let final_text = response.text;
            if !final_text.trim().is_empty() {
                match crate::tools::deliver_reply(&ctx, tools.hooks(), topic_name, &final_text)
                    .await
                {
                    Ok(delivery) => {
                        reply_delivered = true;
                        if delivery.direct {
                            publish_reply_sent(event_bus, topic_name, &final_text).await;
                        }
                    }
                    Err(e) => {
                        if e.to_string().contains("suppressed by reply_send hook") {
                            tracing::info!(reason = %e, "Final reply suppressed by hook");
                        } else {
                            tracing::warn!(error = %e, "Final reply delivery failed");
                        }
                    }
                }
            }

            let duration = start_time.elapsed();
            publish_event(
                event_bus,
                TopicEvent::ProcessingCompleted {
                    topic_name: topic_name.to_string(),
                    message_id: "agent-loop".to_string(),
                    success: true,
                    duration_secs: duration.as_secs(),
                    timestamp: Utc::now(),
                },
            )
            .await;

            return Ok(AgentLoopResult {
                text: final_text,
                reply_delivered,
                input_tokens: context_input_tokens,
                total_input_tokens,
                output_tokens: total_output_tokens,
                total_cache_hit_tokens,
                total_cache_creation_tokens,
                total_reasoning_tokens,
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

        // Question ordering: when this batch includes a blocking `ask_user`,
        // deliver the narration text first — the user must read the message
        // before the question card arrives. The final auto-delivery still
        // fires when the run ends, so the post-answer conclusion is not lost.
        if response.tool_calls.iter().any(|tc| tc.name == "ask_user")
            && !response.text.trim().is_empty()
        {
            // A model may mix a native call with an XML-style tag in the
            // same response — never let raw syntax ship in the narration.
            let span = embedded_ask::find_embedded_ask(&response.text).map(|ask| match ask {
                embedded_ask::EmbeddedAsk::WellFormed { span, .. } => span,
                embedded_ask::EmbeddedAsk::Malformed { span } => span,
            });
            let narration = span.map_or_else(
                || response.text.clone(),
                |span| embedded_ask::remove_span(&response.text, span),
            );
            deliver_progress_text(tools, &ctx, event_bus, topic_name, &narration).await;
        }

        // Snapshot for `context_browse` — only when the tool is actually in
        // this batch. `raw_context` is mutated as tool results are appended
        // below, so the snapshot must be taken before the loop; but cloning
        // the full transcript on every batch that never browses would be
        // wasted O(n) work per iteration. (The shared `ctx` built at the top
        // of `run()` is reused.)
        if response
            .tool_calls
            .iter()
            .any(|tc| tc.name == "context_browse")
        {
            ctx.raw_context = raw_context.clone();
        }

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
                TopicEvent::ToolStarted {
                    topic_name: topic_name.to_string(),
                    tool_name: tool_call.name.clone(),
                    input: Some(tool_call.arguments.clone()),
                    timestamp: Utc::now(),
                },
            )
            .await;

            let tool_start = Instant::now();

            // Race the tool execution against cancellation. Dropping the
            // in-flight future aborts the tool:
            //  - bash: tokio::process::Child::drop kills the spawned shell
            //    and any of its descendants (bash.rs:95-103).
            //  - webfetch: drops the reqwest send future, cancelling the HTTP
            //    request.
            //  - read/write/edit/glob/grep/read_image: drops the I/O future;
            //    a write/edit cancelled mid-flush may leave a partial file —
            //    accepted trade-off for the immediate-cancel guarantee.
            //  - mcp_*: the dropped oneshot reply is cleaned up by the bridge.
            let output = tokio::select! {
                result = tools.execute(&tool_call.name, input.clone(), &ctx) => {
                    match result {
                        Ok(output) => output,
                        Err(e) => {
                            tracing::warn!(tool = %tool_call.name, error = %e, "Tool execution failed");
                            ToolOutput::error(format!("Tool error: {e}"))
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    tracing::info!(tool = %tool_call.name, "Cancelled during tool execution");
                    cancelled_during_tools = true;
                    break;
                }
            };

            let tool_duration = tool_start.elapsed();

            // Publish ToolCompleted
            publish_event(
                event_bus,
                TopicEvent::ToolCompleted {
                    topic_name: topic_name.to_string(),
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

        // Publish progress (only when continuing the loop)
        let elapsed = start_time.elapsed();
        publish_event(
            event_bus,
            TopicEvent::ProcessingProgress {
                topic_name: topic_name.to_string(),
                elapsed_secs: elapsed.as_secs(),
                activity: "tool execution".to_string(),
                progress: Some(format!(
                    "cycle {}, iteration {} ({}), {} tokens",
                    cycle_count + 1,
                    total_iterations + 1,
                    iter_in_cycle + 1,
                    context_input_tokens
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
        TopicEvent::ProcessingCompleted {
            topic_name: topic_name.to_string(),
            message_id: "agent-loop".to_string(),
            success: false,
            duration_secs: duration.as_secs(),
            timestamp: Utc::now(),
        },
    )
    .await;

    Ok(AgentLoopResult {
        text: String::new(),
        reply_delivered,
        input_tokens: context_input_tokens,
        total_input_tokens,
        output_tokens: total_output_tokens,
        total_cache_hit_tokens,
        total_cache_creation_tokens,
        total_reasoning_tokens,
        history,
        raw_context,
    })
}

/// Deliver a mid-turn user-visible message (cycle-boundary heartbeat,
/// pre-question narration) and publish `ReplySent` on direct delivery.
/// Delivery failures are logged, never fatal — the turn continues.
async fn deliver_progress_text(
    tools: &ToolRegistry,
    ctx: &ToolContext<'_>,
    event_bus: Option<&TopicEventBusRef>,
    topic_name: &str,
    text: &str,
) {
    match crate::tools::deliver_reply(ctx, tools.hooks(), topic_name, text).await {
        Ok(delivery) => {
            if delivery.direct {
                publish_reply_sent(event_bus, topic_name, text).await;
            }
        }
        Err(e) => {
            if e.to_string().contains("suppressed by reply_send hook") {
                tracing::info!(reason = %e, "Mid-turn reply suppressed by hook");
            } else {
                tracing::warn!(error = %e, "Mid-turn reply delivery failed");
            }
        }
    }
}

/// Compute and record the cost of one LLM call, returning the amount so
/// the caller can fold it into `session_cost`.
///
/// Returns `0.0` when no pricing is configured, or when the provider
/// reported no usage at all (nothing to bill). Every call that consumes
/// tokens is billed, including the ancillary summarization calls —
/// `kind` distinguishes them in the ledger so summarization overhead can
/// be separated from user-facing spend.
///
/// Ledger write failures are logged and swallowed: billing is
/// observability and must never fail a user's reply.
#[allow(clippy::too_many_arguments)]
fn bill_call(
    pricing: Option<&jyc_types::ModelPricing>,
    billing: jyc_types::config::BillingMode,
    billing_dir: Option<&Path>,
    topic_label: &str,
    model_label: &str,
    kind: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_hit_tokens: u64,
    cache_creation_tokens: u64,
) -> f64 {
    let Some(p) = pricing else { return 0.0 };
    if input_tokens == 0
        && output_tokens == 0
        && cache_hit_tokens == 0
        && cache_creation_tokens == 0
    {
        return 0.0;
    }

    let (cost, rates) = jyc_types::pricing::compute_cost_split_with_rates(
        p,
        input_tokens,
        output_tokens,
        cache_hit_tokens,
        cache_creation_tokens,
    );
    let (time_window, utc_offset) = rates.source.billing_fields();
    let entry = jyc_core::billing_log_store::BillingEntry {
        ts: Utc::now().to_rfc3339(),
        topic: topic_label.to_string(),
        model: model_label.to_string(),
        input_tokens,
        output_tokens,
        cache_hit_tokens,
        cache_creation_tokens,
        cost,
        currency: p.currency_label().to_string(),
        kind: kind.to_string(),
        billing: billing.as_str().to_string(),
        input_rate_per_million: rates.input_per_million,
        output_rate_per_million: rates.output_per_million,
        cache_hit_rate_per_million: rates.cache_hit_per_million,
        cache_creation_rate_per_million: rates
            .cache_creation_per_million
            .unwrap_or(rates.cache_hit_per_million),
        time_window,
        utc_offset,
    };
    match billing_dir {
        Some(dir) => {
            if let Err(e) = jyc_core::billing_log_store::BillingLogStore::append(dir, &entry) {
                tracing::warn!(error = %e, kind, "Failed to append billing entry");
            }
        }
        None => tracing::warn!(kind, "No data home resolvable; dropping billing entry"),
    }
    cost
}

pub(crate) async fn publish_event(event_bus: Option<&TopicEventBusRef>, event: TopicEvent) {
    if let Some(bus) = event_bus {
        let _ = bus.publish(event).await;
    }
}

/// Publish the `ReplySent` dashboard event for a synchronous (direct
/// adapter) delivery. The file-relay watcher/worker publish it for relayed
/// deliveries, so exactly one `ReplySent` fires per delivered reply.
pub(crate) async fn publish_reply_sent(
    event_bus: Option<&TopicEventBusRef>,
    topic_name: &str,
    text: &str,
) {
    publish_event(
        event_bus,
        TopicEvent::ReplySent {
            topic_name: topic_name.to_string(),
            text: text.to_string(),
            timestamp: Utc::now(),
        },
    )
    .await;
}

/// Spawn the live-duration ticker. While the agent loop is alive, the
/// spawned task publishes a `TopicEvent::LoopTick` every `interval`
/// (with the very first tick fired immediately at t=0) so the dashboard
/// can show the wall-clock elapsed time even during silent LLM/tool work
/// (when no iteration has produced a `ProcessingProgress` event yet).
/// Returns the task's `JoinHandle` so the caller can cancel it on
/// natural loop completion.
///
/// The cancel token here is the *topic-level* token. On explicit cancel
/// or shutdown it fires and the task exits — but on natural completion
/// (success / no_reply guard) nothing fires it. Without a `JoinHandle`
/// the spawned task would leak, broadcasting stale `LoopTick` events at
/// 1 Hz until runtime shutdown. The caller is responsible for aborting
/// the handle on every non-cancel exit (see `TickerGuard` in `run`).
///
/// `interval` is taken as a parameter (rather than reading
/// `LOOP_TICK_INTERVAL` directly) so tests can use a fast override and
/// stay under one second.
fn run_ticker(
    start_time: Instant,
    interval: std::time::Duration,
    cancel: CancellationToken,
    event_bus: Option<&TopicEventBusRef>,
    topic_name: String,
) -> JoinHandle<()> {
    let bus = event_bus.cloned();
    tokio::spawn(async move {
        loop {
            // Publish first so the very first tick lands at t=0 — the
            // dashboard's `live_tick_ms_for` otherwise returns None
            // until the first `interval` elapses, leaving short
            // sub-second loops invisible.
            let elapsed_ms = start_time.elapsed().as_millis() as u64;
            let event = TopicEvent::LoopTick {
                topic_name: topic_name.clone(),
                elapsed_ms,
                timestamp: Utc::now(),
            };
            if let Some(bus) = bus.as_ref() {
                let _ = bus.publish(event).await;
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = cancel.cancelled() => break,
            }
        }
    })
}

/// RAII guard that aborts and joins a spawned ticker task when dropped.
/// Placed at the top of `agent_loop::run` so every return path — early
/// error from `complete_with_retry`, the no-reply guard, the success
/// returns, the cycle-boundary continue, the cancellation break, the
/// dangling-tool-call cleanup — terminates the ticker cleanly. Without
/// this, the ticker task leaks on natural completion (the topic-level
/// cancel token only fires on explicit `/cancel` or shutdown).
struct TickerGuard {
    handle: Option<JoinHandle<()>>,
    cancel: CancellationToken,
}

impl TickerGuard {
    fn new(handle: JoinHandle<()>, cancel: CancellationToken) -> Self {
        Self {
            handle: Some(handle),
            cancel,
        }
    }
}

impl Drop for TickerGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

/// A collected tool call from the LLM response.
#[derive(Debug, Clone)]
pub(crate) struct ToolCall {
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

#[cfg(test)]
mod no_reply_tests;

/// Shared test helpers for agent_loop integration tests. Available to
/// sibling `#[cfg(test)]` mods via `pub(super)`.
#[cfg(test)]
mod event_test_helpers;

#[cfg(test)]
mod guardrail_tests;

/// Regression tests for tool-execution cancellation.
///
/// Verifies the contract added by the `tokio::select!` around
/// `tools.execute(...)`: when the per-topic CancellationToken is fired
/// while a tool is running, the agent loop returns within seconds
/// (not the tool's own timeout), and no reply text is produced.
#[cfg(test)]
mod cancel_during_tool_tests;

/// Regression tests for the message-before-question ordering guarantee and
/// the embedded `<ask_user>` recovery shim (models that write the question
/// as XML in the reply text instead of a native tool call).
#[cfg(test)]
mod embedded_ask_tests;

/// Verifies the live-duration ticker: spawns `run_ticker`, observes
/// `LoopTick` events on the bus, and confirms the task stops on both
/// cancel-driven and JoinHandle-driven exit (the latter covers natural
/// completion — the bug `TickerGuard` exists to fix).
#[cfg(test)]
mod ticker_tests;

mod context;
mod embedded_ask;
mod response;
mod retry;

use context::{compact_history_heuristic, compact_raw_context_heuristic};
use retry::{SSE_RETRY_BACKOFF_MS, complete_with_retry};
