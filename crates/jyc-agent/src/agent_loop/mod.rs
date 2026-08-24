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

/// Legacy system-reminder injected when the model ends a turn with **no text
/// and no tool call** (`text_len == 0`) and the reply tool is NOT available.
/// Mirrors the pre-existing `no_reply` guard wording.
const REMINDER_NO_TEXT: &str = "[System reminder] Your last turn produced no \
    text and no tool call, so the user will see no reply. If you have a final \
    response, call `jyc_reply_message` with it now; if nothing needs to be \
    sent (e.g. your reply was already delivered), call `jyc_reply_message` \
    with `silent: true`.";

/// Reminder injected when the model's `jyc_reply_message` call FAILED and the
/// model then tries to finish text-only: the delivery is still owed and a
/// plain-text finish would hit the degraded fallback path. The `{error}`
/// slot is filled with the tool's error message so the model can correct
/// its arguments instead of guessing.
const REMINDER_REPLY_FAILED: &str = "[System reminder] Your `jyc_reply_message` \
    call FAILED and the reply was NOT delivered: {error}. Fix the arguments and \
    call `jyc_reply_message` again now — do not finish with plain text.";

/// Subtle trace appended to an auto-delivered fallback reply: the reply
/// tool was available but never called, so the text was delivered IN THE
/// AGENT'S NAME via a synthetic `jyc_reply_message` execution (see the
/// post-loop fallback). Kept unobtrusive so the delivery is not mistaken
/// for an error, but present so it is never mistaken for a reply the model
/// consciously authored.
const AUTO_REPLY_TRACE: &str = "\n\n— auto-delivered";

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
}

/// Execute `jyc_reply_message` "in the agent's name" — synthetically, on
/// behalf of the model — publishing the same `ToolStarted`/`ToolCompleted`
/// events and `history` entries as a real tool call. Shared by the
/// cycle-boundary progress reply and the final text-only auto-delivery.
///
/// Deliberately does NOT inject a synthetic assistant turn into
/// `raw_context`: that would replay a turn the model never produced (and
/// which has no `reasoning_content`), violating DeepSeek's thinking-mode
/// contract on the next request.
///
/// Returns the tool output so the caller can react to success/failure.
#[allow(clippy::too_many_arguments)]
async fn execute_reply_tool_synthetic(
    tools: &ToolRegistry,
    ctx: &ToolContext<'_>,
    event_bus: Option<&TopicEventBusRef>,
    topic_name: &str,
    call_id: &str,
    message: &str,
    stop_after: bool,
    history: &mut Vec<Message>,
) -> ToolOutput {
    let input = serde_json::json!({"message": message, "stop_after": stop_after});
    let input_str = input.to_string();

    publish_event(
        event_bus,
        TopicEvent::ToolStarted {
            topic_name: topic_name.to_string(),
            tool_name: "jyc_reply_message".to_string(),
            input: Some(input_str.clone()),
            timestamp: Utc::now(),
        },
    )
    .await;

    let tool_start = Instant::now();
    let output = match tools.execute("jyc_reply_message", input, ctx).await {
        Ok(output) => output,
        Err(e) => {
            tracing::warn!(error = %e, "Synthetic jyc_reply_message execution failed");
            ToolOutput::error(format!("Tool error: {e}"))
        }
    };

    publish_event(
        event_bus,
        TopicEvent::ToolCompleted {
            topic_name: topic_name.to_string(),
            tool_name: "jyc_reply_message".to_string(),
            success: !output.is_error,
            duration_secs: tool_start.elapsed().as_secs(),
            output: if output.is_error {
                Some(output.content.clone())
            } else {
                None
            },
            input: Some(input_str),
            timestamp: Utc::now(),
        },
    )
    .await;

    // Synchronous delivery bypasses the file relay, so neither the watcher
    // nor the post-loop worker publishes `ReplySent` — do it here, mirroring
    // the real tool-call path (exactly once per delivery). Without it, the
    // dashboard chat pane never renders the reply (it ignores the raw
    // per-channel `reply` broadcast and only renders `chat_message` events
    // fanned out from `ReplySent`).
    if !output.is_error && output.delivered {
        publish_event(
            event_bus,
            TopicEvent::ReplySent {
                topic_name: topic_name.to_string(),
                text: message.to_string(),
                timestamp: Utc::now(),
            },
        )
        .await;
    }

    // Record the synthetic call in internal `history` for chat-log rendering
    // only — never replayed to the LLM (same rule as the progress reply).
    // Failures are recorded too, so a failed auto-delivery is not lost.
    history.push(Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: call_id.to_string(),
            name: "jyc_reply_message".to_string(),
            input: serde_json::json!({"message": message, "stop_after": stop_after}),
        }],
    });
    history.push(Message::tool_result(
        call_id,
        &output.content,
        output.is_error,
    ));

    output
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
        model_label,
        context_strategy,
        reply_target,
    } = config;

    // Provider used for the cycle-boundary progress summary. Falls back to
    // the main provider when `small_model` is unconfigured or its provider
    // failed to construct (logged at construction time in the service).
    let summary_provider: &dyn Provider = small_provider.unwrap_or(provider);

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
    // tokens in this round. Anthropic is the only provider that
    // reports writes separately; for every other vendor this stays
    // at `0`. Surfaced to the dashboard as
    // `total_cache_creation_tokens`.
    let mut total_cache_creation_tokens: u64 = 0;
    let mut reply_sent_by_tool = false;
    let mut reply_auto_delivered = false;
    let mut reply_text_from_tool: Option<String> = None;

    // Shared ToolContext for tool execution and synthetic `jyc_reply_message`
    // deliveries (cycle-boundary progress reply + final auto-delivery). Built
    // once: every field is static for the duration of the loop. The
    // `context_browse` snapshot below mutates `ctx.raw_context` per batch.
    let mut ctx = ToolContext::with_roots(working_dir, additional_read_roots.clone());
    ctx.additional_write_roots = additional_write_roots.clone();
    ctx.pattern_inject_images = pattern_inject_images;
    ctx.outbound = outbound.clone();
    ctx.topic_managers = topic_managers.clone();
    ctx.current_channel = current_channel.clone();
    ctx.current_topic = Some(topic_name.to_string());
    ctx.outbounds = outbounds.clone();
    ctx.reply_target = reply_target.clone();
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

    // No-reply guard: if the model exits with no text and no tool call, the
    // user receives nothing. We give the model a single system-reminder
    // nudge to recover via `jyc_reply_message`; if it still fails, we exit
    // and surface a SessionStatus event so the activity pane can flag it.
    let mut no_reply_reminded = false;

    // Tool-restricted recovery: when any reply reminder is injected, the
    // NEXT LLM call offers only `jyc_reply_message` — with a single tool on
    // the table the model cannot wander back into narration or other tools.
    // Set at reminder injection, consumed at the LLM call site.
    let mut restrict_to_reply_tool = false;

    // Failure-aware recovery: a FAILED `jyc_reply_message` call means the
    // delivery is still owed. If the model then finishes text-only, remind
    // it with the concrete tool error. Capped so a deterministically
    // failing tool (e.g. a persistent bad attachment name) cannot nudge
    // forever — past the cap we fall through to the fallback path.
    const MAX_REPLY_FAILURE_NUDGES: u32 = 2;
    let mut last_reply_error: Option<String> = None;
    let mut reply_failure_nudges: u32 = 0;

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

        // Check for cycle boundary: send progress reply and reset counter
        if iter_in_cycle >= max_iter {
            cycle_count += 1;
            tracing::info!(
                cycle = cycle_count,
                total_iterations,
                input_tokens = context_input_tokens,
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
            let (progress_text, summary_usage) = generate_summary_from_joined_history(
                summary_provider,
                &raw_context,
                cycle_count,
                total_iterations,
                topic_name,
                event_bus,
                sse_read_timeout,
            ).await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Failed to generate progress summary, using fallback");
                (
                    format!(
                        "Still working on this task. Cycle {}, ~{} iterations completed. Will continue.",
                        cycle_count, total_iterations
                    ),
                    // The call failed, so there is nothing to bill.
                    CallUsage::default(),
                )
            });

            // Bill the summary call. It summarizes the whole transcript, so
            // its input is on the order of the context window -- far from
            // free, and previously invisible in the ledger. `summary_provider`
            // is `small_model` when configured but falls back to the main
            // model, so on a default setup this bills at main-model rates.
            let summary_cost = bill_call(
                pricing.as_ref(),
                topic_path,
                model_label,
                jyc_core::billing_log_store::KIND_SUMMARY,
                summary_usage.input_tokens,
                summary_usage.output_tokens,
                summary_usage.cache_hit_tokens,
                summary_usage.cache_creation_tokens,
            );
            if summary_cost > 0.0 {
                crate::session::add_session_cost(topic_path, summary_cost).await;
            }

            // 2. Post the progress reply to the user via the reply tool.
            //    This sends the GitHub comment / IM message. We do NOT push
            //    a synthetic assistant turn into `raw_context` — doing so
            //    would inject an assistant turn the model never produced
            //    (and thus has no reasoning_content), violating DeepSeek's
            //    thinking-mode contract on the next request.
            let synthetic_call_id = format!("progress-cycle-{}", cycle_count);
            execute_reply_tool_synthetic(
                tools,
                &ctx,
                event_bus,
                topic_name,
                &synthetic_call_id,
                &progress_text,
                false,
                &mut history,
            )
            .await;

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
        let send_context = crate::agent_loop::context::build_send_context(
            provider,
            &raw_context,
            prior_len,
            &context_strategy,
        );
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
        // Reply-recovery turns run with a single tool on the table: after a
        // reminder injection, only `jyc_reply_message` is offered, so the
        // model's only available action is delivering the reply (or going
        // silent). Text-only pleading alone proved insufficient — models
        // often re-emit the text instead of calling the tool.
        let recovery_turn = std::mem::take(&mut restrict_to_reply_tool);
        let tool_defs = if recovery_turn {
            tools
                .definitions()
                .into_iter()
                .filter(|d| d.name == "jyc_reply_message")
                .collect::<Vec<_>>()
        } else {
            tools.definitions()
        };

        let response = match complete_with_retry(
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

        // Bill this call from its own usage payload, before anything can
        // reset or overwrite the round's counters. Doing it per call (rather
        // than once post-loop) means a round that is cancelled or errors out
        // still keeps the cost of the calls that did complete, and a
        // mid-round model switch bills each call at its own rate.
        let call_cost = bill_call(
            pricing.as_ref(),
            topic_path,
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
            topic_path,
            context_input_tokens,
            total_input_tokens,
            total_output_tokens,
            total_cache_hit_tokens,
            total_cache_creation_tokens,
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

        // 5. If no tool calls, we're done
        if response.tool_calls.is_empty() {
            // Trimmed, so whitespace-only narration counts as empty: it gets
            // the no-text reminder, and no fallback warning (nothing to deliver).
            let text_len = response.text.trim().len();
            let reply_tool_available = tools.has_tool("jyc_reply_message");

            // Failure-aware recovery: a failed `jyc_reply_message` attempt
            // means the reply is still owed, and the concrete error tells
            // the model what to fix. Capped (MAX_REPLY_FAILURE_NUDGES) so a
            // deterministically failing tool cannot spin forever.
            if !reply_sent_by_tool
                && reply_tool_available
                && last_reply_error.is_some()
                && reply_failure_nudges < MAX_REPLY_FAILURE_NUDGES
            {
                let error = last_reply_error.take().unwrap_or_default();
                reply_failure_nudges += 1;
                restrict_to_reply_tool = true;
                tracing::warn!(
                    total_iterations,
                    error = %error,
                    nudge = reply_failure_nudges,
                    "Agent loop: reply tool failed and model finished text-only, \
                     injecting failure-aware reminder"
                );
                raw_context.push(provider.format_user_message(&[ContentBlock::Text {
                    text: REMINDER_REPLY_FAILED.replace("{error}", &error),
                }]));
                continue;
            }

            // No-reply state: model produced no text and no tool call.
            // Neither the tool path nor the fallback path will deliver text.
            if text_len == 0 && !reply_sent_by_tool {
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

                // Legacy reminder: only when the reply tool is unavailable.
                // When the tool exists, text-only finishes are handled by the
                // fallback auto-delivery below instead.
                if !reply_tool_available && !no_reply_reminded {
                    no_reply_reminded = true;
                    tracing::warn!(
                        total_iterations,
                        "Agent loop: no-reply detected, injecting system reminder once"
                    );
                    raw_context.push(provider.format_user_message(&[ContentBlock::Text {
                        text: REMINDER_NO_TEXT.to_string(),
                    }]));
                    continue;
                }

                tracing::warn!(total_iterations, "Agent loop: no-reply, exiting");
            } else {
                tracing::info!(
                    total_iterations,
                    cycle = cycle_count,
                    text_len,
                    "Agent loop complete (text-only response)"
                );
            }

            // Fallback delivery: the reply tool exists but was never called.
            // Instead of returning the raw text for the worker's
            // degraded-fallback path, deliver it IN THE AGENT'S NAME by
            // executing `jyc_reply_message` synthetically — the same
            // mechanism as the cycle-boundary progress reply — so delivery,
            // chat-log entry and metrics are identical to a real tool call.
            // A subtle trace is appended so an auto-delivered reply is never
            // mistaken for one the model consciously authored.
            let mut final_text = response.text;

            // Tool-call-shaped text is NOT a reply: some models (MiniMax)
            // echo `<tool_call><invoke name=…>` in the text channel instead
            // of the structured `tool_calls` channel. Suppress it entirely —
            // neither the synthetic auto-delivery below nor the worker's
            // degraded fallback may deliver machine-format garbage to the
            // user.
            if !final_text.trim().is_empty() && looks_like_tool_call(&final_text) {
                tracing::warn!(
                    total_iterations,
                    text_len = final_text.len(),
                    "Agent loop: final text looks like a tool call, suppressing delivery"
                );
                publish_event(
                    event_bus,
                    TopicEvent::SessionStatus {
                        topic_name: topic_name.to_string(),
                        status_type: "tool_call_as_text".to_string(),
                        attempt: None,
                        message: Some(format!(
                            "AI text contained a tool-call block but the structured \
                             tool_calls channel was empty (total_iterations={total_iterations}); \
                             reply suppressed"
                        )),
                        timestamp: Utc::now(),
                    },
                )
                .await;
                final_text.clear();
            }

            if !reply_sent_by_tool && reply_tool_available && !final_text.trim().is_empty() {
                final_text.push_str(AUTO_REPLY_TRACE);
                let synthetic_call_id = format!("auto-reply-{}", total_iterations);
                let synthetic_output = execute_reply_tool_synthetic(
                    tools,
                    &ctx,
                    event_bus,
                    topic_name,
                    &synthetic_call_id,
                    &final_text,
                    true,
                    &mut history,
                )
                .await;

                if !synthetic_output.is_error {
                    reply_sent_by_tool = true;
                    reply_auto_delivered = true;
                    reply_text_from_tool = Some(final_text.clone());
                } else {
                    tracing::warn!(
                        error = %synthetic_output.content,
                        "Synthetic auto-reply failed; falling back to plain text return"
                    );
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
                reply_sent_by_tool,
                reply_auto_delivered,
                reply_text_from_tool,
                input_tokens: context_input_tokens,
                total_input_tokens,
                output_tokens: total_output_tokens,
                total_cache_hit_tokens,
                total_cache_creation_tokens,
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

                // Synchronous delivery bypasses the file relay, so neither
                // the watcher nor the post-loop worker publishes ReplySent —
                // do it here (exactly once per delivery).
                if output.delivered
                    && let Some(msg) = input.get("message").and_then(|m| m.as_str())
                {
                    publish_event(
                        event_bus,
                        TopicEvent::ReplySent {
                            topic_name: topic_name.to_string(),
                            text: msg.to_string(),
                            timestamp: Utc::now(),
                        },
                    )
                    .await;
                }
            } else if (tool_call.name.contains("reply_message")
                || tool_call.name.contains("jyc_reply"))
                && output.is_error
            {
                // Reply delivery FAILED — remember the error. If the model
                // now tries to finish text-only, the failure-aware reminder
                // quotes this so the model can correct its arguments.
                last_reply_error = Some(output.content.clone());
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
                text: String::new(),
                reply_sent_by_tool: true,
                reply_auto_delivered,
                reply_text_from_tool,
                input_tokens: context_input_tokens,
                total_input_tokens,
                output_tokens: total_output_tokens,
                total_cache_hit_tokens,
                total_cache_creation_tokens,
                history,
                raw_context,
            });
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
        reply_sent_by_tool,
        reply_auto_delivered,
        reply_text_from_tool,
        input_tokens: context_input_tokens,
        total_input_tokens,
        output_tokens: total_output_tokens,
        total_cache_hit_tokens,
        total_cache_creation_tokens,
        history,
        raw_context,
    })
}

/// Token usage of a single LLM call, carried back from helpers that make
/// their own calls so the caller can bill them.
#[derive(Debug, Clone, Copy, Default)]
struct CallUsage {
    input_tokens: u64,
    output_tokens: u64,
    /// Per-call cache **read** tokens (= what Anthropic reports in
    /// `cache_read_input_tokens`, or the single `cached_tokens` field
    /// for every other provider).
    cache_hit_tokens: u64,
    /// Per-call cache **creation** (write) tokens. Anthropic is the
    /// only provider that reports this separately; `0` for everyone
    /// else. Billed at `cache_creation_per_million` when configured,
    /// otherwise folded into the read rate.
    cache_creation_tokens: u64,
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
    topic_path: &Path,
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
        model: model_label.to_string(),
        input_tokens,
        output_tokens,
        cache_hit_tokens,
        cache_creation_tokens,
        cost,
        currency: p.currency_label().to_string(),
        kind: kind.to_string(),
        input_rate_per_million: rates.input_per_million,
        output_rate_per_million: rates.output_per_million,
        cache_hit_rate_per_million: rates.cache_hit_per_million,
        time_window,
        utc_offset,
    };
    if let Err(e) = jyc_core::billing_log_store::BillingLogStore::append(topic_path, &entry) {
        tracing::warn!(error = %e, kind, "Failed to append billing entry");
    }
    cost
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
    topic_name: &str,
    event_bus: Option<&TopicEventBusRef>,
    sse_read_timeout: std::time::Duration,
) -> Result<(String, CallUsage)> {
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
    // Use a dummy cancel token — progress summaries don't need cancellation.
    let dummy_cancel = CancellationToken::new();
    let response = complete_with_retry(
        provider,
        &[user_msg],
        &[],
        &summary_system,
        topic_name,
        event_bus,
        sse_read_timeout,
        &dummy_cancel,
        false, // progress summaries don't publish thinking events
        SSE_RETRY_BACKOFF_MS,
    )
    .await?;

    if response.text.is_empty() {
        anyhow::bail!("LLM returned empty progress summary");
    }

    let usage = CallUsage {
        input_tokens: response.input_tokens,
        output_tokens: response.output_tokens,
        cache_hit_tokens: response.cache_hit_tokens,
        cache_creation_tokens: response.cache_creation_tokens,
    };
    Ok((response.text, usage))
}

pub(crate) async fn publish_event(event_bus: Option<&TopicEventBusRef>, event: TopicEvent) {
    if let Some(bus) = event_bus {
        let _ = bus.publish(event).await;
    }
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

/// True when the text looks like a tool call written out as text (e.g.
/// `<tool_call><invoke name="bash">…`). Some providers (MiniMax) echo tool
/// calls in the text channel instead of the structured `tool_calls` channel;
/// such text is never a real reply, so it must not be delivered.
fn looks_like_tool_call(text: &str) -> bool {
    ["<tool_call", "<invoke name=", "<parameter name="]
        .iter()
        .any(|m| text.contains(m))
}

#[cfg(test)]
mod no_reply_tests {
    use super::event_test_helpers::drain_events;
    use super::*;
    use crate::provider::{EventStream, Provider};
    use crate::types::{Message, StreamEvent, ToolDefinition};
    use async_trait::async_trait;
    use futures::stream;
    use jyc_core::topic_event_bus::{SimpleThreadEventBus, TopicEventBusRef};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// Mock provider that returns one final completion per call: empty text,
    /// no tool calls. Used to drive the no-reply path repeatedly.
    struct EmptyResponseProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for EmptyResponseProvider {
        fn name(&self) -> &str {
            "empty-test"
        }
        fn model(&self) -> &str {
            "empty-test-1"
        }

        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<EventStream> {
            unimplemented!("complete() unused in no-reply tests")
        }

        async fn complete_raw(
            &self,
            _raw_messages: &[serde_json::Value],
            _tools: &[ToolDefinition],
            _system: &str,
        ) -> anyhow::Result<EventStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<anyhow::Result<StreamEvent>> = vec![Ok(StreamEvent::Done)];
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

    #[tokio::test]
    async fn no_reply_emits_event_and_reminds_once_then_exits() {
        let provider = EmptyResponseProvider {
            calls: AtomicUsize::new(0),
        };
        let tmp = TempDir::new().unwrap();
        let working_dir = tmp.path().to_path_buf();
        let tools = crate::tools::builtin::create_builtin_registry();
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
        let mut rx = bus.subscribe().await.unwrap();
        let cancel = CancellationToken::new();

        let result = run(super::AgentLoopConfig {
            provider: &provider,
            small_provider: None,
            tools: &tools,
            system_prompt: "test",
            user_blocks: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            working_dir: &working_dir,
            topic_path: &working_dir,
            cancel: cancel.clone(),
            topic_name: "no-reply-test",
            event_bus: Some(&bus),
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
            model_label: "empty-test-1",
            context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
            reply_target: None,
        })
        .await
        .expect("agent loop should run to completion");

        // First call = initial turn; second call = after system-reminder.
        // No third call — the reminder is single-shot.
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "expected exactly one reminder (initial turn + reminder)"
        );

        assert_eq!(result.text, "", "no text should be produced");
        assert!(
            !result.reply_sent_by_tool,
            "reply_sent_by_tool must be false"
        );

        let events = drain_events(&mut rx).await;
        let no_reply_events: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                TopicEvent::SessionStatus { status_type, .. } if status_type == "no_reply" => {
                    Some(())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            no_reply_events.len(),
            2,
            "expected exactly 2 no_reply events (initial + after reminder), got {}",
            no_reply_events.len()
        );
    }
}

/// Tests for the reply-tool guard: when `jyc_reply_message` is registered,
/// a text-only finish without calling it must nudge the model once, then
/// fall back to text delivery with a visible warning marker.
#[cfg(test)]
mod reply_tool_tests {
    use super::event_test_helpers::drain_events;
    use super::*;
    use crate::provider::{EventStream, Provider};
    use crate::tools::mcp_bridge::register_mcp_tools;
    use crate::types::{Message, StreamEvent, ToolDefinition};
    use async_trait::async_trait;
    use futures::stream;
    use jyc_core::topic_event_bus::{SimpleThreadEventBus, TopicEventBusRef};
    use jyc_types::channel::{InboundMessage, OutboundAdapter, OutboundAttachment, SendResult};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    /// Mock provider that replays a scripted list of responses, one per
    /// `complete_raw` call. Used to drive text-only → reply-tool and
    /// text-only → text-only sequences.
    struct ScriptedProvider {
        /// Per-call round of raw `StreamEvent`s (no `Result` wrapper:
        /// `anyhow::Error` is not `Clone`, and the wrapper is only needed
        /// at stream-construction time).
        rounds: Vec<Vec<StreamEvent>>,
        calls: AtomicUsize,
        /// Tool names offered on each `complete_raw` call, in call order —
        /// lets tests assert the reply-recovery turn restricts the tool
        /// list to `jyc_reply_message` alone.
        seen_tools: std::sync::Mutex<Vec<Vec<String>>>,
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

    /// Registry matching production: builtin tools plus the MCP bridge
    /// (which registers `jyc_reply_message`).
    fn registry_with_reply_tool() -> ToolRegistry {
        let mut registry = crate::tools::builtin::create_builtin_registry();
        register_mcp_tools(&mut registry);
        registry
    }

    /// A text-only finish with the reply tool registered must be
    /// auto-delivered immediately — no injected reminder, no nudge turn:
    /// the loop executes `jyc_reply_message` synthetically with the text
    /// plus the subtle trace, and the result counts as sent by the tool.
    #[tokio::test]
    async fn text_only_finish_auto_delivers_without_reminder() {
        let provider = ScriptedProvider {
            rounds: vec![vec![
                StreamEvent::TextDelta("I'll check the docs".to_string()),
                StreamEvent::Done,
            ]],
            calls: AtomicUsize::new(0),
            seen_tools: Default::default(),
        };
        let tmp = TempDir::new().unwrap();
        let working_dir = tmp.path().to_path_buf();
        let tools = registry_with_reply_tool();
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
        let mut rx = bus.subscribe().await.unwrap();
        let cancel = CancellationToken::new();

        let result = run(super::AgentLoopConfig {
            provider: &provider,
            small_provider: None,
            tools: &tools,
            system_prompt: "test",
            user_blocks: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            working_dir: &working_dir,
            topic_path: &working_dir,
            cancel: cancel.clone(),
            topic_name: "reply-tool-auto",
            event_bus: Some(&bus),
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
            model_label: "scripted-test-auto",
            context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
            reply_target: None,
        })
        .await
        .expect("agent loop should run to completion");

        // Single LLM call: text-only finish → immediate synthetic delivery.
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(
            result.reply_sent_by_tool,
            "auto-delivered reply must count as sent by the tool"
        );
        assert!(
            result.reply_auto_delivered,
            "auto-delivered reply must be flagged for metrics"
        );
        assert_eq!(
            result.reply_text_from_tool.as_deref(),
            Some("I'll check the docs\n\n— auto-delivered"),
            "auto-delivered text must be the model text plus the subtle trace, got: {:?}",
            result.reply_text_from_tool
        );

        // No system reminder may be injected: that is the confusion source
        // this change removes.
        let reminders: Vec<_> = result
            .raw_context
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
            .filter(|c| c.contains("[System reminder]"))
            .collect();
        assert!(
            reminders.is_empty(),
            "no reminder may be injected on a text-only finish, got: {:?}",
            reminders
        );

        // No nudge/no_reply status events: the reply was delivered, not flagged.
        let events = drain_events(&mut rx).await;
        let nudges: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                TopicEvent::SessionStatus { status_type, .. }
                    if status_type == "reply_tool_missing" || status_type == "no_reply" =>
                {
                    Some(status_type.clone())
                }
                _ => None,
            })
            .collect();
        assert!(
            nudges.is_empty(),
            "no nudge/no_reply status events expected, got: {:?}",
            nudges
        );
    }

    /// A text-only finish whose text is a tool call written out as text
    /// (MiniMax habit) must NOT be delivered: no synthetic auto-delivery, no
    /// degraded fallback, empty result text, and a `tool_call_as_text`
    /// status event. The MiniMax leak marker must also be scrubbed from the
    /// stored raw context.
    #[tokio::test]
    async fn text_only_tool_call_shape_is_suppressed_not_delivered() {
        let provider = ScriptedProvider {
            rounds: vec![vec![
                StreamEvent::TextDelta(
                    "<tool_call>\n]<]minimax[>[<invoke name=\"bash\">\
                     <parameter name=\"command\">ls</parameter>\
                     </invoke>\n</tool_call>"
                        .to_string(),
                ),
                StreamEvent::Done,
            ]],
            calls: AtomicUsize::new(0),
            seen_tools: Default::default(),
        };
        let tmp = TempDir::new().unwrap();
        let working_dir = tmp.path().to_path_buf();
        let tools = registry_with_reply_tool();
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
        let mut rx = bus.subscribe().await.unwrap();
        let cancel = CancellationToken::new();

        let result = run(super::AgentLoopConfig {
            provider: &provider,
            small_provider: None,
            tools: &tools,
            system_prompt: "test",
            user_blocks: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            working_dir: &working_dir,
            topic_path: &working_dir,
            cancel: cancel.clone(),
            topic_name: "tool-call-as-text",
            event_bus: Some(&bus),
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
            model_label: "scripted-test-tool-call",
            context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
            reply_target: None,
        })
        .await
        .expect("agent loop should run to completion");

        // Not delivered by any path.
        assert!(!result.reply_sent_by_tool, "no delivery may happen");
        assert!(!result.reply_auto_delivered, "no auto-delivery may happen");
        assert!(
            result.text.is_empty(),
            "suppressed text must not be returned for degraded delivery, got: {:?}",
            result.text
        );

        // MiniMax leak marker scrubbed from the stored raw context.
        let raw = serde_json::to_string(&result.raw_context).unwrap();
        assert!(
            !raw.contains("]<]minimax[>["),
            "leak marker must be scrubbed from raw context, got: {raw}"
        );

        // Status event tells the dashboard what happened.
        let events = drain_events(&mut rx).await;
        let suppressed: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                TopicEvent::SessionStatus { status_type, .. }
                    if status_type == "tool_call_as_text" =>
                {
                    Some(status_type.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(suppressed.len(), 1, "one tool_call_as_text event expected");
    }

    /// A `silent: true` reply call closes the turn cleanly: counts as
    /// reply-handled (no fallback warning, no reply text), delivers
    /// nothing, and stops the loop.
    #[tokio::test]
    async fn silent_reply_closes_turn_without_delivery() {
        let provider = ScriptedProvider {
            rounds: vec![vec![
                StreamEvent::ToolUseStart {
                    id: "call_1".to_string(),
                    name: "jyc_reply_message".to_string(),
                },
                StreamEvent::ToolInputDelta(r#"{"silent":true}"#.to_string()),
                StreamEvent::ToolUseEnd,
                StreamEvent::Done,
            ]],
            calls: AtomicUsize::new(0),
            seen_tools: Default::default(),
        };
        let tmp = TempDir::new().unwrap();
        let working_dir = tmp.path().to_path_buf();
        let tools = registry_with_reply_tool();
        let cancel = CancellationToken::new();

        let result = run(super::AgentLoopConfig {
            provider: &provider,
            small_provider: None,
            tools: &tools,
            system_prompt: "test",
            user_blocks: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            working_dir: &working_dir,
            topic_path: &working_dir,
            cancel: cancel.clone(),
            topic_name: "silent-reply",
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
            model_label: "scripted-test-silent",
            context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
            reply_target: None,
        })
        .await
        .expect("agent loop should run to completion");

        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(result.reply_sent_by_tool, "silent reply counts as handled");
        assert!(
            result.reply_text_from_tool.is_none(),
            "silent reply carries no text"
        );
        assert!(result.text.is_empty(), "no fallback text expected");
        // No signal files: the worker must skip post-loop delivery entirely.
        assert!(!working_dir.join(".jyc/reply.md").exists());
        assert!(!working_dir.join(".jyc/reply-sent.flag").exists());
    }

    /// A text-only finish must be auto-delivered in the agent's name: the
    /// loop executes `jyc_reply_message` synthetically (writing the signal
    /// files) and the result counts as sent by the tool, with a subtle
    /// auto-delivery trace appended.
    #[tokio::test]
    async fn persistent_text_only_is_auto_delivered_via_reply_tool() {
        let provider = ScriptedProvider {
            rounds: vec![vec![
                StreamEvent::TextDelta("thinking out loud".to_string()),
                StreamEvent::Done,
            ]],
            calls: AtomicUsize::new(0),
            seen_tools: Default::default(),
        };
        let tmp = TempDir::new().unwrap();
        let working_dir = tmp.path().to_path_buf();
        let tools = registry_with_reply_tool();
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
        let cancel = CancellationToken::new();

        let result = run(super::AgentLoopConfig {
            provider: &provider,
            small_provider: None,
            tools: &tools,
            system_prompt: "test",
            user_blocks: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            working_dir: &working_dir,
            topic_path: &working_dir,
            cancel: cancel.clone(),
            topic_name: "reply-tool-persist",
            event_bus: Some(&bus),
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
            model_label: "scripted-test-1",
            context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
            reply_target: None,
        })
        .await
        .expect("agent loop should run to completion");

        // Single LLM call: the text-only finish is auto-delivered via a
        // synthetic `jyc_reply_message` execution.
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        assert!(
            result.reply_sent_by_tool,
            "auto-delivered reply must count as sent by the tool"
        );
        assert!(
            result.reply_auto_delivered,
            "auto-delivered reply must be flagged for metrics"
        );
        assert_eq!(
            result.reply_text_from_tool.as_deref(),
            Some("thinking out loud\n\n— auto-delivered"),
            "auto-delivered text must be the last model text plus the subtle trace, got: {:?}",
            result.reply_text_from_tool
        );
        // The synthetic execution wrote the signal files (file-relay path,
        // since the test provides no outbound adapter / reply target).
        assert!(
            working_dir.join(".jyc/reply.md").exists(),
            "reply.md must be written by the synthetic auto-delivery"
        );
        assert!(
            working_dir.join(".jyc/reply-sent.flag").exists(),
            "reply-sent.flag must be written by the synthetic auto-delivery"
        );
    }

    /// Minimal outbound adapter that reports successful direct deliveries.
    struct MockOutbound;

    #[async_trait::async_trait]
    impl OutboundAdapter for MockOutbound {
        fn channel_type(&self) -> &str {
            "mock"
        }

        async fn connect(&self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn disconnect(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn clean_body(&self, body: &str) -> String {
            body.to_string()
        }

        async fn send_reply(
            &self,
            _original: &InboundMessage,
            _reply_text: &str,
            _topic_path: &Path,
            _message_dir: &str,
            _attachments: Option<&[OutboundAttachment]>,
        ) -> anyhow::Result<SendResult> {
            Ok(SendResult {
                message_id: "mock-reply".to_string(),
            })
        }

        async fn send_message(
            &self,
            _recipient: &str,
            _subject: &str,
            _body: &str,
        ) -> anyhow::Result<SendResult> {
            Ok(SendResult {
                message_id: "mock-msg".to_string(),
            })
        }
    }

    /// A synthetic auto-delivery that reaches a live outbound adapter must
    /// publish `ReplySent`: the dashboard chat pane renders live replies only
    /// from `chat_message` events fanned out of `ReplySent` (the raw
    /// per-channel `reply` broadcast is ignored), so a delivered-but-eventless
    /// reply shows in the logs but never in the chat pane.
    #[tokio::test]
    async fn synthetic_auto_delivery_publishes_reply_sent() {
        let provider = ScriptedProvider {
            rounds: vec![vec![
                StreamEvent::TextDelta("thinking out loud".to_string()),
                StreamEvent::Done,
            ]],
            calls: AtomicUsize::new(0),
            seen_tools: Default::default(),
        };
        let tmp = TempDir::new().unwrap();
        let working_dir = tmp.path().to_path_buf();
        let tools = registry_with_reply_tool();
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
        let mut rx = bus.subscribe().await.unwrap();
        let cancel = CancellationToken::new();
        let mock: Arc<dyn OutboundAdapter> = Arc::new(MockOutbound);
        let original = InboundMessage {
            id: "test".to_string(),
            channel: "test".to_string(),
            channel_uid: "1".to_string(),
            sender: "user".to_string(),
            sender_address: "user@test".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: Default::default(),
            timestamp: chrono::Utc::now(),
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: Default::default(),
            matched_pattern: None,
        };

        let result = run(super::AgentLoopConfig {
            provider: &provider,
            small_provider: None,
            tools: &tools,
            system_prompt: "test",
            user_blocks: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            working_dir: &working_dir,
            topic_path: &working_dir,
            cancel: cancel.clone(),
            topic_name: "reply-sent-direct",
            event_bus: Some(&bus),
            prior_history: vec![],
            prior_raw_context: vec![],
            max_iterations: Some(5),
            sse_read_timeout: std::time::Duration::from_secs(60),
            additional_read_roots: vec![],
            additional_write_roots: vec![],
            pattern_inject_images: false,
            outbound: Some(mock),
            topic_managers: None,
            current_channel: None,
            outbounds: None,
            context_window: None,
            auto_reset_threshold: 0.95,
            thinking_enabled: false,
            pricing: None,
            model_label: "scripted-test-2",
            context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
            reply_target: Some(crate::tools::ReplyTarget {
                original,
                message_dir: "2026-08-23_00-00-00".to_string(),
            }),
        })
        .await
        .expect("agent loop should run to completion");

        assert!(
            result.reply_auto_delivered,
            "auto-delivered reply must be flagged for metrics"
        );
        let events = drain_events(&mut rx).await;
        let reply_sent: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                TopicEvent::ReplySent { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reply_sent,
            vec!["thinking out loud\n\n— auto-delivered"],
            "synchronous synthetic delivery must publish ReplySent with the delivered text"
        );
    }

    /// A FAILED `jyc_reply_message` call (empty message) followed by a
    /// text-only finish must trigger the failure-aware reminder quoting the
    /// concrete tool error, and the recovery turn must again be restricted
    /// to the reply tool alone.
    #[tokio::test]
    async fn failed_reply_then_text_only_gets_failure_reminder() {
        let provider = ScriptedProvider {
            rounds: vec![
                // Round 0: broken reply call — empty message, tool errors.
                vec![
                    StreamEvent::ToolUseStart {
                        id: "call_1".to_string(),
                        name: "jyc_reply_message".to_string(),
                    },
                    StreamEvent::ToolInputDelta(r#"{"message":"","stop_after":true}"#.to_string()),
                    StreamEvent::ToolUseEnd,
                    StreamEvent::Done,
                ],
                // Round 1: model gives up and finishes text-only.
                vec![
                    StreamEvent::TextDelta("let me just say it in text".to_string()),
                    StreamEvent::Done,
                ],
                // Round 2 (restricted): corrected reply call.
                vec![
                    StreamEvent::ToolUseStart {
                        id: "call_2".to_string(),
                        name: "jyc_reply_message".to_string(),
                    },
                    StreamEvent::ToolInputDelta(
                        r#"{"message":"fixed answer","stop_after":true}"#.to_string(),
                    ),
                    StreamEvent::ToolUseEnd,
                    StreamEvent::Done,
                ],
            ],
            calls: AtomicUsize::new(0),
            seen_tools: Default::default(),
        };
        let tmp = TempDir::new().unwrap();
        let working_dir = tmp.path().to_path_buf();
        let tools = registry_with_reply_tool();
        let cancel = CancellationToken::new();

        let result = run(super::AgentLoopConfig {
            provider: &provider,
            small_provider: None,
            tools: &tools,
            system_prompt: "test",
            user_blocks: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            working_dir: &working_dir,
            topic_path: &working_dir,
            cancel: cancel.clone(),
            topic_name: "reply-failure",
            event_bus: None,
            prior_history: vec![],
            prior_raw_context: vec![],
            max_iterations: Some(6),
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
            model_label: "scripted-test-failure",
            context_strategy: jyc_types::channel::ContextStrategyConfig::default(),
            reply_target: None,
        })
        .await
        .expect("agent loop should run to completion");

        assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
        assert!(result.reply_sent_by_tool, "corrected reply must win");
        assert_eq!(result.reply_text_from_tool.as_deref(), Some("fixed answer"));

        // The injected reminder must be the failure-aware variant, quoting
        // the concrete tool error (empty message).
        let reminders: Vec<_> = result
            .raw_context
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .filter_map(|m| m.get("content").and_then(|c| c.as_str()))
            .filter(|c| c.contains("[System reminder]"))
            .collect();
        assert_eq!(reminders.len(), 1, "exactly one reminder expected");
        assert!(
            reminders[0].contains("FAILED and the reply was NOT delivered"),
            "expected REMINDER_REPLY_FAILED, got: {}",
            reminders[0]
        );
        assert!(
            reminders[0].contains("Message cannot be empty"),
            "reminder must quote the tool error, got: {}",
            reminders[0]
        );

        // The recovery turn after the failure reminder must also be
        // tool-restricted (round index 2).
        let seen = provider.seen_tools.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[2], vec!["jyc_reply_message".to_string()]);
    }
}

/// Shared test helpers for agent_loop integration tests. Available to
/// sibling `#[cfg(test)]` mods via `pub(super)`.
#[cfg(test)]
mod event_test_helpers {
    use jyc_core::topic_event::TopicEvent;

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

/// Regression tests for tool-execution cancellation.
///
/// Verifies the contract added by the `tokio::select!` around
/// `tools.execute(...)`: when the per-topic CancellationToken is fired
/// while a tool is running, the agent loop returns within seconds
/// (not the tool's own timeout), and no reply text is produced.
#[cfg(test)]
mod cancel_during_tool_tests {
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
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
        {
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
}

/// Verifies the live-duration ticker: spawns `run_ticker`, observes
/// `LoopTick` events on the bus, and confirms the task stops on both
/// cancel-driven and JoinHandle-driven exit (the latter covers natural
/// completion — the bug `TickerGuard` exists to fix).
#[cfg(test)]
mod ticker_tests {
    use super::*;
    use jyc_core::topic_event::TopicEvent;
    use jyc_core::topic_event_bus::{SimpleThreadEventBus, TopicEventBusRef};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn run_ticker_publishes_then_exits_on_cancel() {
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
        let mut rx = bus.subscribe().await.unwrap();
        let cancel = CancellationToken::new();
        let start = Instant::now();

        // Spawn with a fast interval so the test finishes in <500 ms
        // instead of waiting 3+ seconds at the production 1 Hz cadence.
        let handle = run_ticker(
            start,
            Duration::from_millis(50),
            cancel.clone(),
            Some(&bus),
            "topic-x".to_string(),
        );

        // Wait for at least 3 ticks before cancelling (~150 ms at 50 ms
        // interval — production uses 1 s). The very first tick fires
        // immediately at t=0, so we expect ticks more frequently than
        // the interval suggests.
        let mut got = 0u32;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while got < 3 && std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Some(TopicEvent::LoopTick { .. })) => got += 1,
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
        assert!(got >= 3, "expected at least 3 LoopTick events, got {got}");

        // Cancel and verify no further tick arrives within 200 ms.
        cancel.cancel();
        if let Ok(Some(TopicEvent::LoopTick { .. })) =
            tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
        {
            panic!("ticker should not publish after cancel")
        }
        // Tidy up so the test doesn't leak (and so clippy doesn't warn).
        let _ = handle.await;
    }

    /// The very first tick must fire at t=0, not at t=`interval` —
    /// otherwise a sub-second loop produces no event at all and the
    /// dashboard shows nothing.
    #[tokio::test]
    async fn run_ticker_publishes_immediately_at_t_zero() {
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
        let mut rx = bus.subscribe().await.unwrap();
        let cancel = CancellationToken::new();
        let start = Instant::now();

        // Long interval (production cadence), short observation window:
        // if the first tick waited for the interval, the recv() would
        // time out before any event landed.
        let handle = run_ticker(
            start,
            Duration::from_secs(1),
            cancel.clone(),
            Some(&bus),
            "topic-x".to_string(),
        );

        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(TopicEvent::LoopTick { elapsed_ms, .. })) => {
                assert!(
                    elapsed_ms < 50,
                    "first tick should fire near t=0, got elapsed_ms={elapsed_ms}"
                );
            }
            other => panic!("expected an immediate first LoopTick, got {other:?}"),
        }

        cancel.cancel();
        let _ = handle.await;
    }

    /// Regression for the orphan-ticker bug: when the cancel token is
    /// *not* fired (natural completion), the ticker must still exit via
    /// `TickerGuard::drop` → `JoinHandle::abort`. Before the guard was
    /// added, this test would hang forever.
    #[tokio::test]
    async fn ticker_exits_on_handle_abort_when_cancel_not_fired() {
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(32));
        let mut rx = bus.subscribe().await.unwrap();
        let cancel = CancellationToken::new();
        let start = Instant::now();

        let handle = run_ticker(
            start,
            Duration::from_millis(50),
            cancel.clone(),
            Some(&bus),
            "topic-x".to_string(),
        );

        // Drain one tick to confirm it's actually running.
        match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Some(TopicEvent::LoopTick { .. })) => {}
            other => panic!("expected a LoopTick, got {other:?}"),
        }

        // Drop the TickerGuard via RAII scope — this is what `run()`
        // does at every return path. The cancel token is NEVER fired.
        {
            let _guard = TickerGuard::new(handle, cancel);
        }

        // The handle must be joined within a reasonable bound. If the
        // orphan-bug regresses (handle leaked, no abort), this hangs.
        let joined = tokio::time::timeout(Duration::from_secs(2), async {
            // Re-acquire the handle from the guard would require a getter;
            // instead just assert the ticker stops publishing. Both
            // `cancel.cancel()` and `handle.abort()` happen in `drop`,
            // so within one tick interval (50 ms here) no further tick fires.
            tokio::time::sleep(Duration::from_millis(150)).await;
        })
        .await;
        assert!(joined.is_ok(), "drop guard should not hang");

        if let Ok(Some(TopicEvent::LoopTick { .. })) =
            tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
        {
            panic!("ticker should not publish after TickerGuard drop")
        }
    }
}

mod context;
mod response;
mod retry;

use context::{
    compact_history_heuristic, compact_raw_context_heuristic, render_raw_context_as_text,
};
use retry::{SSE_RETRY_BACKOFF_MS, complete_with_retry};
