//! The core agentic loop.
//!
//! Sends messages to the LLM, detects tool calls, executes them,
//! and loops until the LLM responds with only text (no tool calls).

use chrono::Utc;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing;

use jyc_core::topic_event::TopicEvent;
use jyc_core::topic_event_bus::TopicEventBusRef;
use jyc_core::topic_manager::TopicManagers;

use jyc_types::QueueItem;
use jyc_types::channel::ContextStrategyConfig;

use crate::provider::Provider;
use crate::tools::{OutboundsMap, ToolContext, ToolOutput, registry::ToolRegistry};
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
    /// LLM call via `session::persist_tokens`.
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
    /// Messages that arrived while this turn was running, handed over by
    /// the worker when the pattern's `live_injection` is on.
    ///
    /// The loop folds them into the conversation at its next drain point
    /// (the top of an iteration), so the model sees them in the next
    /// call it makes — as user messages, not as turns of their own.
    /// `None` when the caller has no live queue to drain (tests,
    /// sub-agents, the static agent).
    pub injection_rx: Option<&'a mut tokio::sync::mpsc::Receiver<QueueItem>>,
    /// Maximum loop iterations. Defaults to DEFAULT_MAX_ITERATIONS.
    pub max_iterations: Option<usize>,
    /// SSE read timeout — maximum gap between SSE events before the stream
    /// is considered hung. Defaults to 120 seconds.
    pub sse_read_timeout: std::time::Duration,
    /// Additional absolute paths permitted for every tool that enforces a
    /// path boundary (`read`, `read_image`, `bash`, `grep`). Used to allow
    /// access to a configured absolute `[attachments.inbound].save_path`,
    /// the external skill directories and the relocated `.jyc` state
    /// directory, all of which live outside `working_dir`.
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
    pub topic_managers: Option<Arc<TopicManagers>>,
    /// Current channel name, for tools that need source context
    /// (e.g. `jyc_send_to_topic` sets `source_channel` metadata from this).
    pub current_channel: Option<String>,
    /// Cross-channel outbound adapters keyed by channel name.
    /// Passed through to `ToolContext` so the `jyc_send_message` tool can
    /// send proactive messages through any channel's outbound adapter.
    pub outbounds: Option<OutboundsMap>,
    /// Context window size in tokens — the model's whole budget. Recorded
    /// with the token counts so the dashboard can show how full the window
    /// is.
    pub context_window: Option<u64>,
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
    /// Central billing ledger directory (`<data dir>/billing`), resolved
    /// once at construction. `None` drops billing writes with a warning.
    pub billing_dir: Option<std::path::PathBuf>,
    /// Model identifier (`"provider/model"`) recorded on each ledger entry.
    /// Only used for billing, so an empty string is harmless when
    /// `pricing` is `None`.
    pub model_label: &'a str,
    /// Context management strategy. Controls how prior conversation history
    /// is shaped before being sent to the LLM. The on-disk
    /// `.jyc/agent-context.jsonl` is always the full raw context; this
    /// field only affects the wire payload.
    pub context_strategy: ContextStrategyConfig,
    /// Synchronous delivery target for `jyc_reply_message`. Passed through
    /// to `ToolContext`; `None` in contexts without a live inbound message
    /// (tests, sub-agents), where the reply tool falls back to the
    /// `reply.md`/`reply-sent.flag` file relay.
    pub reply_target: Option<crate::tools::ReplyTarget>,
}

/// Run the agent loop to completion.
///
/// Returns the final text response and metadata about tool usage.
///
/// A turn that dies hands back the reply requests it still owed: they are
/// the only thing the caller cannot see any more, and whoever asked for
/// them has to hear that the turn failed.
pub async fn run(
    config: AgentLoopConfig<'_>,
) -> std::result::Result<AgentLoopResult, crate::types::AgentLoopError> {
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
        mut injection_rx,
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
        thinking_enabled,
        pricing,
        billing_mode,
        billing_dir,
        model_label,
        context_strategy,
        reply_target,
    } = config;

    // Topic label stamped on every billing entry this loop writes.
    let billing_label =
        jyc_core::billing_log_store::BillingLogStore::label_for(topic_name, topic_path);

    // The manager that serves this topic — the writer of the topic's live
    // status (the one source of truth for `TopicStatus`). `None` where the
    // loop runs without a channel (tests, sub-agents): there is no status
    // to write there.
    let status_topic = current_channel.as_deref().and_then(|channel| {
        topic_managers
            .as_ref()
            .and_then(|managers| managers.by_channel(channel))
    });

    // Where this turn's outcome goes. One binding for the whole loop, so
    // every path that settles the turn writes it the same way.
    let status = status_topic.as_deref().map(ManagerStatus);

    let max_iter = max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS);

    // Build internal history: prior context + current message
    let mut history: Vec<Message> = prior_history;
    history.push(Message::user_with_blocks(user_blocks.clone()));

    // Build raw context: prior raw + current user message. The full context
    // is persisted to the `.jyc/agent-context.jsonl` journal at the end of
    // the loop (tail-append when the prefix is unchanged); the strategy
    // decides what is sent to the LLM (see `build_send_context`).
    let mut prior_len = prior_raw_context.len();
    let mut raw_context: Vec<serde_json::Value> = prior_raw_context;
    raw_context.push(provider.format_user_message(&user_blocks));

    let mut context_input_tokens: u64 = 0;
    // Output tokens of the most recent LLM call — the reply that gets
    // appended to the context. Paired with `context_input_tokens` so the
    // two always describe the same call.
    let mut context_output_tokens: u64 = 0;
    // The session's totals so far: this turn's calls are added on top of
    // them, so the counters describe the session rather than restarting
    // at zero every turn.
    let seed = crate::session::session_token_totals(topic_name, topic_path).await;
    let mut total_input_tokens: u64 = seed.input;
    let mut total_output_tokens: u64 = seed.output;
    // Sum of every LLM call's prompt-cache-hit tokens in the session.
    // Mirrors `total_input_tokens`; zeroed by callers on session reset
    // and surfaced to the dashboard as `total_cache_hit_tokens`.
    let mut total_cache_hit_tokens: u64 = seed.cache_hit;
    // Sum of every LLM call's prompt-cache-**creation** (write)
    // tokens in the session. Anthropic and OpenAI (GPT-5.6+) report
    // writes separately; for every other vendor this stays at `0`.
    // Surfaced to the dashboard as `total_cache_creation_tokens`.
    let mut total_cache_creation_tokens: u64 = seed.cache_creation;
    // Sum of every LLM call's reasoning (thinking) tokens in the session.
    // Informational only — already included in `total_output_tokens`.
    let mut total_reasoning_tokens: u64 = seed.reasoning;
    let mut reply_delivered = false;
    // Set when a tool handed the user everything they need this turn
    // (`ask_user`'s questions): the loop stops at the end of that batch, since
    // the next model call would only be a chance to repeat what was just delivered.
    let mut handed_over = false;
    // What a handed-over turn still has to say: the questions that failed to go
    // out on the live channel, which become this turn's reply text so the worker
    // delivers them. Empty whenever the delivery worked, and never sent twice —
    // both paths that run tools end the turn this way.
    let mut turn_text = String::new();

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

    // The tool calls of this turn that failed and were not retried
    // successfully. They are a log line at the end of the turn, not the
    // turn's outcome: a model that gives up on a tool and reaches the
    // answer another way still did what it was asked.
    let mut tool_failures = ToolFailures::default();

    // Wall time split reported at the end of the turn: how much of it the
    // model spent, how much the tools spent. Nothing is logged while an LLM
    // call is in flight (the "Thinking..." state travels on the event bus),
    // so these are what make a quiet stretch of `jyc.log` legible.
    let mut turn_llm_ms: u64 = 0;
    let mut turn_tool_ms: u64 = 0;

    // The send context's history block is rebuilt per call unless cached;
    // within a turn the persisted prefix is fixed, so build it once (see
    // `SendContextCache`).
    let mut send_context_cache = crate::agent_loop::context::SendContextCache::default();

    // Publish ProcessingStarted
    // The topic's live status follows the events this loop publishes (see
    // `jyc_core::topic_manager::live_status`): write it first, so every
    // consumer that reads the status when the event lands sees the turn as
    // started.
    if let Some(tm) = status_topic.as_ref() {
        tm.set_topic_processing(topic_name);
    }
    publish_event(
        event_bus,
        TopicEvent::ProcessingStarted {
            topic_name: topic_name.to_string(),
            message_id: "agent-loop".to_string(),
            timestamp: Utc::now(),
        },
    )
    .await;

    // The reply requests that arrived while this turn was running. The
    // guard forwards an answer for each one the turn did not send.
    let mut pending_replies: Vec<crate::service::reply_guard::ReplyRequest> = Vec::new();

    loop {
        if cancel.is_cancelled() {
            tracing::info!(total_iterations, "Agent loop cancelled");
            break;
        }

        // Fold in the messages that arrived while this turn was running
        // (`live_injection`). They join the conversation here, before
        // this iteration's call is built, so the model sees them in it.
        if let Some(rx) = injection_rx.as_deref_mut() {
            // The sends this turn has made so far. A request folded in
            // now can only be answered by a send that comes after it: the
            // ones before it went to that topic before the model was shown
            // the request, so they were no answer to it.
            let sent_before = ctx
                .outbound_targets
                .lock()
                .expect("outbound_targets poisoned")
                .len();
            let (_, arrived) = drain_injections(
                rx,
                &mut history,
                &mut raw_context,
                provider,
                ctx.outbound.as_deref(),
                topic_name,
                topic_path,
            );
            // The requests carry the mark: it is what tells a send the
            // turn made afterwards from one it made before the request was
            // folded in.
            pending_replies.extend(arrived.into_iter().map(|source| {
                crate::service::reply_guard::ReplyRequest {
                    source,
                    sent_before,
                }
            }));
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
                        publish_reply_sent(event_bus, topic_name, &heartbeat, delivery.chat_seq)
                            .await;
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
        // `send_regions` is the parallel Vec<u8> of per-message labels
        // (1=text-only history / Full-mode, 2=current turn's recent raw
        // suffix, 3=folded current-turn checkpoint) — used by the
        // wire-payload debug dump and the composition snapshot.
        let build_started = Instant::now();
        let (send_context, send_regions) = crate::agent_loop::context::build_send_context_cached(
            provider,
            &raw_context,
            prior_len,
            &context_strategy,
            &mut send_context_cache,
        );
        let context_build_ms = build_started.elapsed().as_millis() as u64;
        // The composition of the payload about to be sent, measured from
        // the same region labels the debug dump uses. Persisted once the
        // call returns, so a failed call never overwrites the last
        // completed call's snapshot.
        let context_composition = crate::agent_loop::context::context_composition(
            send_context.as_ref(),
            &send_regions,
            &context_strategy.mode,
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

        // Wall-clock time of the call about to be made, banked into the
        // session's per-call average once it returns. Started here rather
        // than inside `complete_with_retry` so the retries and their
        // backoff are part of what the user waited.
        let call_started = Instant::now();
        // The call logs nothing until it returns; without these two lines a
        // slow model and a stalled process look the same in the log.
        tracing::info!(
            iteration = total_iterations,
            model = model_label,
            send_messages = send_context.len(),
            context_build_ms,
            history_rebuilt = %send_context_cache
                .history_rebuilt()
                .map_or("-", |rebuilt| if rebuilt { "rebuilt" } else { "reused" }),
            checkpoint_rebuilt = %send_context_cache
                .checkpoint_rebuilt()
                .map_or("-", |rebuilt| if rebuilt { "rebuilt" } else { "reused" }),
            last_input_tokens = context_input_tokens,
            "LLM call started"
        );
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
            Err(e) => {
                // The turn dies here: the tools that failed along the way are
                // logged so the log reads in order, and the failure goes back
                // to the caller, whose error path writes the status.
                log_standing_failures(topic_name, &tool_failures);
                // The reply requests that arrived mid-turn die with the
                // turn: they go back with the error, so the caller can tell
                // whoever asked that the turn failed.
                return Err(crate::types::AgentLoopError {
                    error: e,
                    pending_replies: std::mem::take(&mut pending_replies),
                });
            }
        };
        let call_ms = call_started.elapsed().as_millis() as u64;
        turn_llm_ms += call_ms;
        tracing::info!(
            iteration = total_iterations,
            duration_ms = call_ms,
            input_tokens = response.input_tokens,
            output_tokens = response.output_tokens,
            reasoning_tokens = response.reasoning_tokens,
            cache_hit_tokens = response.cache_hit_tokens,
            "LLM call finished"
        );

        // Track tokens across LLM calls in this session:
        // - `context_input_tokens` = input tokens from the most recent LLM call
        //   (current context size, since each call sends full context).
        // - `total_input_tokens` / `total_output_tokens` = running sums across
        //   every call in this session (seeded from the session file above).
        //   Each call's `input_tokens` (= full context size) is added via `+=`,
        //   so `total_input_tokens` also represents the lifetime tokens billed
        //   as input by the API for this session.
        if response.input_tokens > 0 {
            context_input_tokens = response.input_tokens;
            context_output_tokens = response.output_tokens;
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

        // Persist latest token counts to disk so dashboard polls see fresh
        // data mid-round. The last three are this call's own cache-hit,
        // input and output tokens — the turn-scoped figures are accumulated
        // per call, the way the latency pair is.
        crate::session::persist_tokens_with_composition(
            topic_name,
            topic_path,
            context_input_tokens,
            context_output_tokens,
            total_input_tokens,
            total_output_tokens,
            total_cache_hit_tokens,
            total_cache_creation_tokens,
            total_reasoning_tokens,
            context_window,
            call_cost,
            call_ms,
            response.cache_hit_tokens,
            response.input_tokens,
            response.output_tokens,
            response.reasoning_tokens,
            &context_composition,
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
            // instead of emitting a native tool call. Recover a well-formed tag
            // — deliver the prose first, then run the question exactly as the
            // real tool would, ending the turn there. Malformed tags are
            // stripped (all of them) so raw syntax never ships to the user; the
            // remaining prose falls through to normal delivery below.
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
            }) = embedded_ask::find_embedded_ask(&response.text)
            {
                let prose = embedded_ask::remove_span(&response.text, span);
                let has_prose = !prose.trim().is_empty();
                // Hand the prose to `ask_user` as well: a box shows the
                // question, and this is the only place the context can come
                // from if the delivery below does not reach the user.
                ctx.question_context = has_prose.then(|| prose.clone());
                if has_prose
                    && !deliver_progress_text(tools, &ctx, event_bus, topic_name, &prose).await
                {
                    tracing::warn!(
                        "ask_user prose was not delivered to the channel; the question box carries it instead"
                    );
                }
                let input = serde_json::json!({
                    "question": question,
                    "options": options,
                });
                let output = match tools.execute("ask_user", input, &ctx).await {
                    Ok(output) => output,
                    Err(e) => {
                        tracing::warn!(error = %e, "Embedded ask_user execution failed");
                        ToolOutput::error(format!("Tool error: {e}"))
                    }
                };
                // The embedded ask_user is a tool call like any other: it
                // stands in error until the model asks again and it works.
                tool_failures.record("ask_user", &output);
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
                // The same delivery the native batch path does after its tools:
                // this path runs `ask_user` too, and a plain-text question left
                // queued here is dropped while the model is told it was handed
                // over.
                let undelivered = drain_pending_texts(tools, &ctx, event_bus, topic_name).await;
                if !output.ends_turn {
                    // No live delivery target: the model's reply is the only
                    // surface left, so it keeps going and asks there.
                    continue;
                }
                // The same exit the native batch path uses — one way for a tool
                // that already handed its answer over to end the turn. The prose
                // went out above, so the only thing left to say is the block that
                // failed to deliver (empty otherwise, which also keeps the user
                // from being shown the questions twice).
                handed_over = true;
                turn_text = undelivered.join("\n\n");
                break;
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
                            publish_reply_sent(
                                event_bus,
                                topic_name,
                                &final_text,
                                delivery.chat_seq,
                            )
                            .await;
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
            // The turn's outcome: the status follows the turn itself, and the
            // tool calls that failed along the way are a log line. The status
            // is settled before the turn's end is announced.
            finish_turn(
                status.as_ref(),
                topic_name,
                &tool_failures,
                true,
                move || async move {
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
                    .await
                },
            )
            .await;

            return Ok(AgentLoopResult {
                text: final_text,
                reply_delivered,
                input_tokens: context_input_tokens,
                context_output_tokens,
                total_input_tokens,
                output_tokens: total_output_tokens,
                total_cache_hit_tokens,
                total_cache_creation_tokens,
                total_reasoning_tokens,
                history,
                raw_context,
                prior_len,
                outbound_targets: ctx
                    .outbound_targets
                    .lock()
                    .expect("outbound_targets poisoned")
                    .clone(),
                pending_replies: std::mem::take(&mut pending_replies),
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
                // The turn is over: log the tool calls that never worked
                // before the turn's own failure goes back to the caller.
                log_standing_failures(topic_name, &tool_failures);
                // The reply requests that arrived mid-turn die with the
                // turn: they go back with the error, so the caller can tell
                // whoever asked that the turn failed.
                return Err(crate::types::AgentLoopError {
                    error: anyhow::anyhow!(
                        "model generated tool calls with empty arguments for {} consecutive \
                         iterations — this usually indicates the provider does not support \
                         function calling correctly",
                        consecutive_empty_tool_iterations
                    ),
                    pending_replies: std::mem::take(&mut pending_replies),
                });
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

        // Question ordering: when this batch asks the user something, deliver
        // the narration text first — the user must read the context before the
        // question arrives. The turn ends at the question, so for a while this
        // is the last thing they see from us. The same text also travels with
        // the question itself (`ctx.question_context`), because a box has no
        // other way to show it and this delivery can fail silently.
        ctx.question_context = None;
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
            ctx.question_context = Some(narration.clone());
            if !deliver_progress_text(tools, &ctx, event_bus, topic_name, &narration).await {
                tracing::warn!(
                    "ask_user narration was not delivered to the channel; the question box carries it instead"
                );
            }
        }

        // Snapshot for `context_browse` — only when the tool is actually in
        // this batch. `raw_context` is mutated as tool results are appended
        // below, so the snapshot must be taken before the loop; but cloning
        // the full transcript on every batch that never browses would be
        // wasted O(n) work per iteration. (The shared `ctx` built at the top
        // of `run()` is reused.)
        //
        // The snapshot is this turn's messages only: the prior context is
        // not in memory (the reader parses the journal on demand), and
        // `context_browse` reads it from the journal itself.
        if response
            .tool_calls
            .iter()
            .any(|tc| tc.name == "context_browse")
        {
            ctx.raw_context = raw_context[prior_len..].to_vec();
        }

        let mut cancelled_during_tools = false;

        // Started after the narration delivery and the `context_browse`
        // snapshot: those write to a channel (SMTP, websocket) and clone the
        // transcript, neither of which is the tools' time.
        let tools_started = Instant::now();
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
            //  - read/write/edit/grep/read_image: drops the I/O future;
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
            // A failed tool leaves the turn in doubt until the model retries
            // it: the failure counts only when the tool never works again.
            tool_failures.record(&tool_call.name, &output);
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
            handed_over |= output.ends_turn;
        }

        let tool_batch_ms = tools_started.elapsed().as_millis() as u64;
        turn_tool_ms += tool_batch_ms;
        tracing::info!(
            iteration = total_iterations,
            duration_ms = tool_batch_ms,
            "Tool calls finished"
        );

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
                // The truncation never reaches into the persisted prefix in
                // practice (the dangling message is always from this turn),
                // but clamp defensively so `prior_len` never overstates
                // what is actually on disk.
                prior_len = prior_len.min(raw_context.len());
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

        // Drain any texts queued by tools that had to ask without a question
        // box (`ask_user` on a channel that cannot show one): the user has to
        // actually receive them, so they go out through the same reply path as
        // the final message instead of relying on the model to repeat them.
        let undelivered = drain_pending_texts(tools, &ctx, event_bus, topic_name).await;

        // Everything the user needed this turn is already delivered, so stop
        // here rather than going round again. The remaining tool calls of the
        // batch ran anyway: their results keep the transcript free of dangling
        // tool calls.
        if handed_over {
            if !undelivered.is_empty() && !cancel.is_cancelled() {
                // The questions never reached the live channel, and nothing else
                // will carry them this turn: the relay fallback writes `reply.md`
                // whose watcher dies with the turn, and the post-loop delivery
                // below is skipped for a turn that reported itself delivered. So
                // the block becomes this turn's reply text and the worker sends it
                // with the ordinary reply machinery. Asking the model instead is
                // pointless — its only surface is the same broken channel.
                tracing::error!(
                    topic = %topic_name,
                    "ask_user questions were not delivered; sending them as the turn's reply"
                );
                turn_text = undelivered.join("\n\n");
            }
            tracing::info!("Agent loop ended: ask_user handed its questions over");
            break;
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

    // Loop ended (cancellation, or a tool handed its answer over — there's no
    // max-cycles limit). A turn cancelled by the user is a failed turn whatever
    // it managed to deliver first, including a question that ended the turn.
    let duration = start_time.elapsed();
    // `llm_ms + tool_ms` need not reach `total_ms`: a call cancelled in
    // flight is timed nowhere, and the gap is where the rest of the turn
    // (delivery, persistence) sits.
    tracing::info!(
        iterations = total_iterations,
        total_ms = duration.as_millis() as u64,
        llm_ms = turn_llm_ms,
        tool_ms = turn_tool_ms,
        "Agent loop summary"
    );
    let turn_succeeded = handed_over && !cancel.is_cancelled();
    finish_turn(
        status.as_ref(),
        topic_name,
        &tool_failures,
        turn_succeeded,
        move || async move {
            publish_event(
                event_bus,
                TopicEvent::ProcessingCompleted {
                    topic_name: topic_name.to_string(),
                    message_id: "agent-loop".to_string(),
                    success: turn_succeeded,
                    duration_secs: duration.as_secs(),
                    timestamp: Utc::now(),
                },
            )
            .await
        },
    )
    .await;

    Ok(AgentLoopResult {
        text: turn_text,
        reply_delivered,
        input_tokens: context_input_tokens,
        context_output_tokens,
        total_input_tokens,
        output_tokens: total_output_tokens,
        total_cache_hit_tokens,
        total_cache_creation_tokens,
        total_reasoning_tokens,
        history,
        raw_context,
        prior_len,
        outbound_targets: ctx
            .outbound_targets
            .lock()
            .expect("outbound_targets poisoned")
            .clone(),
        pending_replies: std::mem::take(&mut pending_replies),
    })
}

/// Deliver a mid-turn user-visible message (cycle-boundary heartbeat,
/// pre-question narration) and publish `ReplySent` on direct delivery.
/// Delivery failures are logged, never fatal — the turn continues.
///
/// Returns whether the text reached the user's channel directly. A `false`
/// covers both a failed send and the relay fallback, whose watcher stops with
/// the turn — callers that cannot tolerate that loss need to say so.
async fn deliver_progress_text(
    tools: &ToolRegistry,
    ctx: &ToolContext<'_>,
    event_bus: Option<&TopicEventBusRef>,
    topic_name: &str,
    text: &str,
) -> bool {
    match crate::tools::deliver_reply(ctx, tools.hooks(), topic_name, text).await {
        Ok(delivery) => {
            if delivery.direct {
                publish_reply_sent(event_bus, topic_name, text, delivery.chat_seq).await;
                return true;
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
    false
}

/// Deliver the texts a tool queued for the user during this batch — currently
/// `ask_user`'s questions on a channel with no question box. Both paths that
/// run tools (the native batch and the embedded-`<ask_user>` recovery) call
/// this after the tool returns; a path that forgets drops the questions
/// silently while the tool result tells the model they were handed over.
///
/// Returns the texts that did not reach the user's channel, so the caller can
/// still get them out some other way.
async fn drain_pending_texts(
    tools: &ToolRegistry,
    ctx: &ToolContext<'_>,
    event_bus: Option<&TopicEventBusRef>,
    topic_name: &str,
) -> Vec<String> {
    let mut undelivered = Vec::new();
    for text in ctx.take_pending_texts() {
        if !deliver_progress_text(tools, ctx, event_bus, topic_name, &text).await {
            undelivered.push(text);
        }
    }
    undelivered
}

/// Fold the messages that arrived while this turn was running into the
/// conversation.
///
/// The channel is drained completely: everything queued at this moment
/// becomes one batch, in arrival order, and the model sees the whole
/// batch in the next call it makes. An injected message is information
/// for the running turn — it gets no turn and no reply of its own.
///
/// The body the model reads is cleaned the way the turn's own body is
/// cleaned (the channel strips the quoted history of a reply), so both
/// reach the model in the same shape. The transcript keeps the raw
/// words: the chat log entry is written from the message as it arrived.
///
/// Each folded message is appended to the topic's chat log, so the
/// transcript shows what the user actually said. Storing happens here,
/// where the message enters the conversation: a message this turn never
/// folds in is never stored by it, and the turn that later processes
/// that message stores it then — so every message lands in the log
/// exactly once.
///
/// Returns how many messages were folded in, and the sources that asked for
/// a reply.
///
/// The sources come back unmarked: how many sends the turn had made is the
/// turn's own bookkeeping, and the caller stamps the requests it gets here
/// with it.
fn drain_injections(
    rx: &mut tokio::sync::mpsc::Receiver<QueueItem>,
    history: &mut Vec<Message>,
    raw_context: &mut Vec<serde_json::Value>,
    provider: &dyn Provider,
    outbound: Option<&dyn jyc_types::channel::OutboundAdapter>,
    topic_name: &str,
    topic_path: &Path,
) -> (usize, Vec<crate::service::reply_guard::ReplySource>) {
    let mut count = 0usize;
    let mut requests: Vec<crate::service::reply_guard::ReplySource> = Vec::new();
    while let Ok(item) = rx.try_recv() {
        let Some(raw) = item.message.content.text.as_deref().map(str::trim) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        // A body that is nothing but quoted history cleans down to
        // nothing — keep the raw words then: the message still says
        // something the user wants the model to see.
        let cleaned = outbound.map(|o| o.clean_body(raw)).unwrap_or_default();
        let body = if cleaned.trim().is_empty() {
            raw
        } else {
            cleaned.trim()
        };
        // A message that names where it came from says so in the
        // conversation: the model has to know it came from another topic.
        // One that asked for a reply says that too — it is waiting for an
        // answer — and the request is collected as well, so the guard can
        // forward an answer the turn forgot to send.
        let source = crate::service::reply_guard::source_of(&item.message.metadata);
        let asks = crate::service::reply_guard::asks_for_reply(&item.message.metadata);
        let text = match (source, asks) {
            (Some(source), true) => {
                requests.push(source.clone());
                format!(
                    "{}{}",
                    crate::service::prompt::reply_request_notice(&source),
                    body
                )
            }
            (Some(source), false) => {
                format!("{}\n{}", crate::service::prompt::source_line(&source), body)
            }
            (None, _) => body.to_string(),
        };
        let blocks = vec![ContentBlock::Text { text }];
        history.push(Message::user_with_blocks(blocks.clone()));
        raw_context.push(provider.format_user_message(&blocks));

        let is_matched = !item.pattern_match.pattern_name.is_empty();
        let mut chat_log = jyc_core::chat_log_store::ChatLogStore::new(topic_name, topic_path);
        if let Err(e) = chat_log.append_message(&item.message, is_matched) {
            tracing::warn!(error = %e, "Failed to append an injected message to the chat log");
        }
        count += 1;
    }
    if count > 0 {
        tracing::info!(
            topic = %topic_name,
            count,
            "Injected live messages into the running turn"
        );
    }
    (count, requests)
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
    id: u64,
) {
    publish_event(
        event_bus,
        TopicEvent::ReplySent {
            topic_name: topic_name.to_string(),
            id,
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

/// The tool failures of a turn, and the ones the model recovered from.
mod tool_failures;

/// Regression tests for the tool-failure bookkeeping: a tool that failed
/// along the way must not leave the turn's status in error, whether the
/// model retried it or reached the answer another way.
#[cfg(test)]
mod tool_failures_tests;

/// The loop's own wiring of the session's token totals: a turn continues
/// the session's counts instead of restarting them.
#[cfg(test)]
mod session_totals_tests;

/// Live injection: the messages that arrive while a turn is running join
/// the conversation instead of waiting for the turn to end.
#[cfg(test)]
mod injection_tests;

pub(crate) mod context;
mod embedded_ask;
mod response;
mod retry;

use tool_failures::{ManagerStatus, ToolFailures, finish_turn, log_standing_failures};

use retry::{SSE_RETRY_BACKOFF_MS, complete_with_retry};
