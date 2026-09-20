//! Retry logic for LLM calls (SSE + throttled classes).
//!
//! Extracted from the monolithic `agent_loop.rs`.

use anyhow::Result;
use chrono::Utc;
use tokio_util::sync::CancellationToken;

use super::publish_event;
use super::response::{
    CollectedResponse, collect_response, looks_like_leak_in_mixed_response,
    looks_like_leaked_tool_call,
};
use crate::provider::{Provider, RetryClass, classify_retry, extract_retry_after};
use crate::types::ToolDefinition;
use jyc_core::topic_event::TopicEvent;
use jyc_core::topic_event_bus::TopicEventBusRef;

const SSE_MAX_ATTEMPTS: u32 = 3;

/// Backoff (milliseconds) before each retry of a transient failure.
/// Indexed by retry number (0-based: the wait BEFORE the 2nd attempt is
/// `[0]`, before the 3rd is `[1]`, etc.). Length must be
/// `SSE_MAX_ATTEMPTS - 1`.
///
/// 10s/20s (was 1s/2s): a transient failure — e.g. an SSE idle timeout
/// that already waited `sse_read_timeout` on a silent stream — should not
/// be retried almost immediately; give the upstream a few seconds before
/// re-issuing. (#617)
pub(super) const SSE_RETRY_BACKOFF_MS: &[u64] = &[10000, 20000];

/// Maximum attempts for throttled failures (HTTP 429/502/503/504, #391).
/// Rate-limit windows are typically tens of seconds, so this schedule is
/// slower and more patient than the transient one.
const THROTTLED_MAX_ATTEMPTS: u32 = 5;

/// Backoff (milliseconds) before each retry of a throttled failure.
/// Length must be `THROTTLED_MAX_ATTEMPTS - 1`.
const THROTTLED_RETRY_BACKOFF_MS: &[u64] = &[5000, 15000, 30000, 60000];

/// Cap on any single backoff, including waits derived from the provider's
/// `Retry-After` header — bounds how long a pathological value can stall
/// a topic.
const MAX_BACKOFF_MS: u64 = 120_000;

/// Maximum attempts for the given retry class (includes the initial call).
fn max_attempts_for(class: RetryClass) -> u32 {
    match class {
        RetryClass::Throttled => THROTTLED_MAX_ATTEMPTS,
        _ => SSE_MAX_ATTEMPTS,
    }
}

/// Compute the wait (milliseconds) before the next retry.
///
/// `retry_after_secs` is the provider's `Retry-After` value when captured
/// by the diagnostic probe; it acts as a floor on top of the class's fixed
/// schedule, and the result is capped at [`MAX_BACKOFF_MS`]. `attempt_idx`
/// is clamped to the schedule length so a mid-loop class change (e.g. a
/// transient failure followed by a throttled one) cannot index out of
/// bounds.
///
/// `transient_backoff_ms` is the transient schedule — production passes
/// [`SSE_RETRY_BACKOFF_MS`]; tests pass a tiny schedule to keep the
/// retry-loop tests fast.
fn retry_wait_ms(
    class: RetryClass,
    transient_backoff_ms: &[u64],
    attempt_idx: u32,
    retry_after_secs: Option<u64>,
) -> u64 {
    let schedule = match class {
        RetryClass::Throttled => THROTTLED_RETRY_BACKOFF_MS,
        _ => transient_backoff_ms,
    };
    let idx = (attempt_idx as usize).min(schedule.len() - 1);
    let fixed = schedule[idx];
    let floor = retry_after_secs.unwrap_or(0).saturating_mul(1000);
    fixed.max(floor).min(MAX_BACKOFF_MS)
}

/// Issue one LLM call and collect its streaming response, retrying on
/// transient SSE / network failures and throttling rejections (#391).
///
/// On a failure classified by [`classify_retry`]:
/// - `Transient` → fixed schedule (3 attempts, 10s/20s backoff).
/// - `Throttled` (429/502/503/504) → slow schedule (5 attempts,
///   5s/15s/30s/60s backoff), honoring the provider's `Retry-After`
///   header as a floor when captured.
/// - `Terminal` → propagate immediately.
///
/// Before each retry, a `SessionStatus { status_type: "retry", attempt: N }`
/// event is published (and a `tracing::warn!` logged) carrying the next
/// retry's absolute time and, when known, the Retry-After value — so both
/// the dashboard and the logs show when the next attempt will happen.
///
/// Retries re-issue the entire request (no resume — providers don't
/// support it). Output tokens from the failed attempt are discarded; only
/// the successful attempt's tokens are counted by the caller.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn complete_with_retry(
    provider: &dyn Provider,
    raw_context: &[serde_json::Value],
    tools: &[ToolDefinition],
    system_prompt: &str,
    topic_name: &str,
    event_bus: Option<&TopicEventBusRef>,
    sse_read_timeout: std::time::Duration,
    cancel: &CancellationToken,
    thinking_enabled: bool,
    transient_backoff_ms: &[u64],
) -> Result<CollectedResponse> {
    let mut last_err: anyhow::Error =
        anyhow::anyhow!("complete_with_retry exited without attempting any call");

    for attempt_idx in 0..THROTTLED_MAX_ATTEMPTS {
        // Check cancellation before each attempt so /cancel takes effect
        // immediately, not just between loop iterations.
        if cancel.is_cancelled() {
            return Err(anyhow::anyhow!(
                "cancelled before LLM call attempt {}",
                attempt_idx + 1
            ));
        }

        let result = issue_call(
            provider,
            raw_context,
            tools,
            system_prompt,
            topic_name,
            event_bus,
            sse_read_timeout,
            cancel,
            thinking_enabled,
        )
        .await;

        match result {
            Ok(r) => return Ok(r),
            Err(e) => last_err = e,
        }

        // Retry decision: classify the failure, then apply the class's
        // budget. Terminal errors propagate immediately.
        let err_display = format!("{:#}", last_err);
        let class = classify_retry(&last_err);
        let max_attempts = max_attempts_for(class);
        let is_last_attempt = attempt_idx + 1 >= max_attempts;
        if class == RetryClass::Terminal || is_last_attempt {
            break;
        }

        let retry_after_secs = extract_retry_after(&err_display);
        let wait_ms = retry_wait_ms(class, transient_backoff_ms, attempt_idx, retry_after_secs);
        let next_attempt = attempt_idx + 2; // 1-based attempt # we're about to make
        let next_at = Utc::now() + chrono::Duration::milliseconds(wait_ms as i64);
        let retry_after_note = retry_after_secs
            .map(|s| format!(", retry-after: {s}s"))
            .unwrap_or_default();
        let timing = format!(
            "next retry at {} UTC (in {}s{})",
            next_at.format("%H:%M:%S"),
            wait_ms / 1000,
            retry_after_note
        );
        let truncated_err = jyc_utils::helpers::truncate_str_ellipsis(&err_display, 160);

        tracing::warn!(
            attempt = next_attempt,
            max_attempts,
            class = ?class,
            wait_ms,
            error = %err_display,
            "LLM call failed, {timing}"
        );

        publish_event(
            event_bus,
            TopicEvent::SessionStatus {
                topic_name: topic_name.to_string(),
                status_type: "retry".to_string(),
                attempt: Some(next_attempt),
                message: Some(format!(
                    "{class:?} error, retrying ({}/{}), {}: {}",
                    next_attempt, max_attempts, timing, truncated_err
                )),
                timestamp: Utc::now(),
            },
        )
        .await;

        // Interruptible backoff: cancel takes effect immediately instead of
        // waiting for the full sleep duration.
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => {}
            _ = cancel.cancelled() => {
                return Err(anyhow::anyhow!(
                    "cancelled during retry backoff (attempt {}/{})",
                    next_attempt, max_attempts
                ));
            }
        }
    }

    Err(last_err)
}

/// Issue one LLM call and collect its streaming response, honouring the
/// cancellation token (a cancelled token aborts immediately).
#[allow(clippy::too_many_arguments)]
async fn issue_call(
    provider: &dyn Provider,
    raw_context: &[serde_json::Value],
    tools: &[ToolDefinition],
    system_prompt: &str,
    topic_name: &str,
    event_bus: Option<&TopicEventBusRef>,
    sse_read_timeout: std::time::Duration,
    cancel: &CancellationToken,
    thinking_enabled: bool,
) -> Result<CollectedResponse> {
    tokio::select! {
        r = async {
            let stream = provider
                .complete_raw(raw_context, tools, system_prompt)
                .await?;
            let collected = collect_response(
                stream,
                sse_read_timeout,
                event_bus,
                topic_name,
                thinking_enabled,
            )
            .await?;
            // Provider format-failure guard (#786): raw tool-call syntax in
            // the text channel is garbage to the user — fail the attempt
            // (transient "provider format failure") so the retry loop
            // re-issues the call. Applies even when structured tool_calls
            // parsed fine: a partially parsed stream can carry valid calls
            // while the unparsed remainder lands in the text channel and
            // ships verbatim via the auto-delivery fallback — but in that
            // mixed mode only the closing-tag / repetition checks run, so
            // one start-anchored quote in legit prose does not kill a valid
            // call. Also reject
            // structured tool calls with an EMPTY name: degenerate streams
            // parse into blank-name calls that bypass the text guard and
            // ship as `<response_tools>` garbage to the user.
            let text_is_leak = if collected.tool_calls.is_empty() {
                looks_like_leaked_tool_call(&collected.text)
            } else {
                // Mixed mode: a valid structured call coexists with the text
                // channel. Use the narrow check — a single start-anchored
                // marker quote in legit prose must not kill a valid tool
                // call; only closing tags and repetition storms flag here.
                looks_like_leak_in_mixed_response(&collected.text)
            };
            if text_is_leak
                || collected
                    .tool_calls
                    .iter()
                    .any(|tc| tc.name.trim().is_empty())
            {
                tracing::warn!(
                    text_excerpt = %jyc_utils::helpers::truncate_str_ellipsis(&collected.text, 200),
                    empty_name_calls = collected
                        .tool_calls
                        .iter()
                        .filter(|tc| tc.name.trim().is_empty())
                        .count(),
                    "provider format failure: model emitted tool-call syntax as text"
                );
                Err(anyhow::anyhow!(
                    "provider format failure: model emitted tool-call syntax as text"
                ))
            } else {
                Ok(collected)
            }
        } => r,
        _ = cancel.cancelled() => Err(anyhow::anyhow!("cancelled during LLM call")),
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::provider::{EventStream, Provider};
    use crate::types::{Message, StreamEvent, ToolDefinition};
    use async_trait::async_trait;
    use futures::stream;
    use jyc_core::topic_event_bus::{SimpleThreadEventBus, TopicEventBusRef};
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
            mock_format_user_message(blocks)
        }

        fn format_tool_result(
            &self,
            tool_call_id: &str,
            content: &str,
            _is_error: bool,
        ) -> serde_json::Value {
            mock_format_tool_result(tool_call_id, content)
        }

        fn build_raw_assistant_message(
            &self,
            text: &str,
            _reasoning: &str,
            _tool_calls: &[(String, String, String)],
        ) -> serde_json::Value {
            mock_build_raw_assistant_message(text)
        }
    }

    use super::super::event_test_helpers::drain_events;
    use crate::types::ContentBlock;

    /// Shared `Provider::format_user_message` body for the mock providers
    /// below: text blocks joined into one JSON object.
    fn mock_format_user_message(blocks: &[ContentBlock]) -> serde_json::Value {
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

    /// Shared `Provider::format_tool_result` body for the mock providers.
    fn mock_format_tool_result(tool_call_id: &str, content: &str) -> serde_json::Value {
        serde_json::json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": content,
        })
    }

    /// Shared `Provider::build_raw_assistant_message` body for the mock
    /// providers.
    fn mock_build_raw_assistant_message(text: &str) -> serde_json::Value {
        serde_json::json!({"role": "assistant", "content": text})
    }

    /// Mock provider whose FIRST call returns a well-formed stream whose
    /// text is leaked tool-call syntax (with no structured tool_calls);
    /// every later call returns a normal text response. Used to prove the
    /// leak guard fails the attempt as a transient error and the retry
    /// re-issues the call (#786).
    struct LeakyProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for LeakyProvider {
        fn name(&self) -> &str {
            "leaky"
        }
        fn model(&self) -> &str {
            "leaky-1"
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
            let text = if n == 0 {
                "<response tools=\"<call tool=\"bash\" index=\"1\">".to_string()
            } else {
                "ok".to_string()
            };
            let events: Vec<anyhow::Result<StreamEvent>> =
                vec![Ok(StreamEvent::TextDelta(text)), Ok(StreamEvent::Done)];
            Ok(Box::pin(stream::iter(events)))
        }

        fn format_user_message(&self, blocks: &[ContentBlock]) -> serde_json::Value {
            mock_format_user_message(blocks)
        }

        fn format_tool_result(
            &self,
            tool_call_id: &str,
            content: &str,
            _is_error: bool,
        ) -> serde_json::Value {
            mock_format_tool_result(tool_call_id, content)
        }

        fn build_raw_assistant_message(
            &self,
            text: &str,
            _reasoning: &str,
            _tool_calls: &[(String, String, String)],
        ) -> serde_json::Value {
            mock_build_raw_assistant_message(text)
        }
    }

    /// Mock provider that ALWAYS returns a valid structured tool call plus
    /// prose that BEGINS with a single quoted `<response_tools>` marker —
    /// the legit meta-discussion shape the mixed-mode guard must not flag
    /// (regression test for the start-anchored false positive).
    struct MixedQuotingProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for MixedQuotingProvider {
        fn name(&self) -> &str {
            "mixed-quoting"
        }
        fn model(&self) -> &str {
            "mixed-quoting-1"
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
            self.calls.fetch_add(1, Ordering::SeqCst);
            let events: Vec<anyhow::Result<StreamEvent>> = vec![
                Ok(StreamEvent::ToolUseStart {
                    id: "1".to_string(),
                    name: "bash".to_string(),
                }),
                Ok(StreamEvent::ToolInputDelta("{}".to_string())),
                Ok(StreamEvent::ToolUseEnd),
                Ok(StreamEvent::TextDelta(
                    "<response_tools>\n这是正文里引用一次标签。".to_string(),
                )),
                Ok(StreamEvent::Done),
            ];
            Ok(Box::pin(stream::iter(events)))
        }

        fn format_user_message(&self, blocks: &[ContentBlock]) -> serde_json::Value {
            mock_format_user_message(blocks)
        }

        fn format_tool_result(
            &self,
            tool_call_id: &str,
            content: &str,
            _is_error: bool,
        ) -> serde_json::Value {
            mock_format_tool_result(tool_call_id, content)
        }

        fn build_raw_assistant_message(
            &self,
            text: &str,
            _reasoning: &str,
            _tool_calls: &[(String, String, String)],
        ) -> serde_json::Value {
            mock_build_raw_assistant_message(text)
        }
    }

    /// Mock provider whose FIRST call returns a stream with BOTH a valid
    /// structured tool call AND leaked tool-call syntax in the text channel
    /// (partial-parse mixed mode, which used to bypass the guard via
    /// `tool_calls.is_empty()` and ship the garbage silently); every later
    /// call returns a normal text response.
    struct MixedLeakyProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for MixedLeakyProvider {
        fn name(&self) -> &str {
            "mixed-leaky"
        }
        fn model(&self) -> &str {
            "mixed-leaky-1"
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
            let events: Vec<anyhow::Result<StreamEvent>> = if n == 0 {
                vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "1".to_string(),
                        name: "bash".to_string(),
                    }),
                    Ok(StreamEvent::ToolInputDelta("{}".to_string())),
                    Ok(StreamEvent::ToolUseEnd),
                    // Fragment storm: bare openers, no closing tags.
                    Ok(StreamEvent::TextDelta(format!(
                        "先说结论。\n{}",
                        "<response_tools>\n".repeat(6)
                    ))),
                    Ok(StreamEvent::Done),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta("ok".to_string())),
                    Ok(StreamEvent::Done),
                ]
            };
            Ok(Box::pin(stream::iter(events)))
        }

        fn format_user_message(&self, blocks: &[ContentBlock]) -> serde_json::Value {
            mock_format_user_message(blocks)
        }

        fn format_tool_result(
            &self,
            tool_call_id: &str,
            content: &str,
            _is_error: bool,
        ) -> serde_json::Value {
            mock_format_tool_result(tool_call_id, content)
        }

        fn build_raw_assistant_message(
            &self,
            text: &str,
            _reasoning: &str,
            _tool_calls: &[(String, String, String)],
        ) -> serde_json::Value {
            mock_build_raw_assistant_message(text)
        }
    }

    /// Mock provider whose FIRST call returns a structured tool call with an
    /// EMPTY name (degenerate-stream signature: the runtime parses the
    /// `<response_tools>` storm into blank-name calls, which bypass the text
    /// guard and ship as garbage to the user); every later call returns
    /// normal text.
    struct EmptyNameLeakyProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for EmptyNameLeakyProvider {
        fn name(&self) -> &str {
            "empty-name-leaky"
        }
        fn model(&self) -> &str {
            "empty-name-leaky-1"
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
            let events: Vec<anyhow::Result<StreamEvent>> = if n == 0 {
                vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "1".to_string(),
                        name: "".to_string(),
                    }),
                    Ok(StreamEvent::ToolInputDelta("{}".to_string())),
                    Ok(StreamEvent::ToolUseEnd),
                    Ok(StreamEvent::TextDelta("先说结论。".to_string())),
                    Ok(StreamEvent::Done),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta("ok".to_string())),
                    Ok(StreamEvent::Done),
                ]
            };
            Ok(Box::pin(stream::iter(events)))
        }

        fn format_user_message(&self, blocks: &[ContentBlock]) -> serde_json::Value {
            mock_format_user_message(blocks)
        }

        fn format_tool_result(
            &self,
            tool_call_id: &str,
            content: &str,
            _is_error: bool,
        ) -> serde_json::Value {
            mock_format_tool_result(tool_call_id, content)
        }

        fn build_raw_assistant_message(
            &self,
            text: &str,
            _reasoning: &str,
            _tool_calls: &[(String, String, String)],
        ) -> serde_json::Value {
            mock_build_raw_assistant_message(text)
        }
    }

    /// A structured tool call with an empty name is a degenerate-stream
    /// signature that bypasses the text leak guard; the attempt must fail as
    /// a provider format failure and the retry re-issue the call.
    #[tokio::test]
    async fn empty_name_tool_call_is_retried_then_succeeds() {
        let provider = EmptyNameLeakyProvider {
            calls: AtomicUsize::new(0),
        };
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(10));

        let result = complete_with_retry(
            &provider,
            &[],
            &[],
            "system",
            "topic-x",
            Some(&bus),
            std::time::Duration::from_secs(120),
            &CancellationToken::new(),
            true,
            &[1, 2],
        )
        .await;

        assert!(
            result.is_ok(),
            "expected Ok after retry, got {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().text, "ok");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "expected 2 total calls (1 empty-name + 1 good)"
        );
    }

    /// Two transient failures then success → returns Ok, publishes 2 retry events.
    #[tokio::test]
    async fn retries_transient_sse_errors_then_succeeds() {
        let provider = FlakyProvider {
            fail_count: 2,
            fail_message: "error decoding response body".to_string(),
            calls: AtomicUsize::new(0),
        };
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx = bus.subscribe().await.unwrap();

        // Fast transient backoff so the retry loop doesn't sleep 10s+20s
        // per retry in a unit test — the schedule VALUES themselves are
        // pinned by the `retry_wait_ms` unit tests below.
        let result = complete_with_retry(
            &provider,
            &[],
            &[],
            "system",
            "topic-x",
            Some(&bus),
            std::time::Duration::from_secs(120),
            &CancellationToken::new(),
            true,
            &[1, 2],
        )
        .await;

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
                TopicEvent::SessionStatus {
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
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx = bus.subscribe().await.unwrap();

        let result = complete_with_retry(
            &provider,
            &[],
            &[],
            "system",
            "topic-x",
            Some(&bus),
            std::time::Duration::from_secs(120),
            &CancellationToken::new(),
            true,
            &[1, 2],
        )
        .await;

        assert!(result.is_err(), "expected Err after exhausting retries");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            SSE_MAX_ATTEMPTS as usize,
            "should have made exactly SSE_MAX_ATTEMPTS calls"
        );

        let events = drain_events(&mut rx).await;
        let retry_count = events
            .iter()
            .filter(|e| matches!(e, TopicEvent::SessionStatus { status_type, .. } if status_type == "retry"))
            .count();
        assert_eq!(
            retry_count,
            (SSE_MAX_ATTEMPTS - 1) as usize,
            "should publish one retry event per retry (not for the initial attempt or the final failed attempt)"
        );
    }

    /// Leaked tool-call syntax in the text channel (no structured
    /// tool_calls) fails the attempt as a transient provider format failure
    /// and the retry re-issues the call (#786).
    #[tokio::test]
    async fn leaked_tool_call_syntax_is_retried_then_succeeds() {
        let provider = LeakyProvider {
            calls: AtomicUsize::new(0),
        };
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(10));

        let result = complete_with_retry(
            &provider,
            &[],
            &[],
            "system",
            "topic-x",
            Some(&bus),
            std::time::Duration::from_secs(120),
            &CancellationToken::new(),
            true,
            &[1, 2],
        )
        .await;

        assert!(
            result.is_ok(),
            "expected Ok after retry, got {:?}",
            result.err()
        );
        let response = result.unwrap();
        assert_eq!(response.text, "ok");
        assert!(response.tool_calls.is_empty());
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "expected 2 total calls (1 leaked + 1 good)"
        );
    }

    /// Mixed mode: valid tool_calls + leaked syntax in the text channel —
    /// the attempt must STILL fail the leak guard (no `tool_calls.is_empty()`
    /// bypass) and the retry re-issue the call.
    #[tokio::test]
    async fn mixed_leak_with_valid_tool_calls_is_retried() {
        let provider = MixedLeakyProvider {
            calls: AtomicUsize::new(0),
        };
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(10));

        let result = complete_with_retry(
            &provider,
            &[],
            &[],
            "system",
            "topic-x",
            Some(&bus),
            std::time::Duration::from_secs(120),
            &CancellationToken::new(),
            true,
            &[1, 2],
        )
        .await;

        assert!(
            result.is_ok(),
            "expected Ok after retry, got {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().text, "ok");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "expected 2 total calls (1 mixed-leak + 1 good)"
        );
    }

    /// Mixed mode: a valid tool call plus prose that begins with ONE quoted
    /// marker must pass the guard untouched — the start-anchored check only
    /// applies to text-only responses, otherwise legit meta-discussion
    /// kills a valid tool call and burns the retry budget.
    #[tokio::test]
    async fn mixed_mode_single_marker_quote_is_not_flagged() {
        let provider = MixedQuotingProvider {
            calls: AtomicUsize::new(0),
        };
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(10));

        let result = complete_with_retry(
            &provider,
            &[],
            &[],
            "system",
            "topic-x",
            Some(&bus),
            std::time::Duration::from_secs(120),
            &CancellationToken::new(),
            true,
            &[1, 2],
        )
        .await;

        let response = result.expect("single marker quote in mixed mode must pass");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "bash");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "no retry expected for a valid mixed response"
        );
    }

    /// The leak-guard and truncation errors must classify as Transient so
    /// they retry on the fast SSE schedule instead of dying Terminal (#786).
    #[test]
    fn format_failures_classify_as_transient() {
        let leak =
            anyhow::anyhow!("provider format failure: model emitted tool-call syntax as text");
        assert!(
            matches!(classify_retry(&leak), RetryClass::Transient),
            "leak guard error should be transient"
        );

        let truncated =
            anyhow::anyhow!("SSE stream ended without Done marker (truncated mid-stream)");
        assert!(
            matches!(classify_retry(&truncated), RetryClass::Transient),
            "truncated-stream error should be transient"
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
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx = bus.subscribe().await.unwrap();

        let result = complete_with_retry(
            &provider,
            &[],
            &[],
            "system",
            "topic-x",
            Some(&bus),
            std::time::Duration::from_secs(120),
            &CancellationToken::new(),
            true,
            &[1, 2],
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "non-transient error must not retry"
        );

        let events = drain_events(&mut rx).await;
        let retry_count = events
            .iter()
            .filter(|e| matches!(e, TopicEvent::SessionStatus { status_type, .. } if status_type == "retry"))
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
    /// causing the topic to die after one attempt.
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
        let bus: TopicEventBusRef = Arc::new(SimpleThreadEventBus::new(10));
        let mut rx = bus.subscribe().await.unwrap();

        let result = complete_with_retry(
            &provider,
            &[],
            &[],
            "system",
            "topic-x",
            Some(&bus),
            std::time::Duration::from_secs(120),
            &CancellationToken::new(),
            true,
            &[1, 2],
        )
        .await;

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
                TopicEvent::SessionStatus {
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

    #[test]
    fn retry_wait_transient_uses_schedule() {
        assert_eq!(
            retry_wait_ms(RetryClass::Transient, SSE_RETRY_BACKOFF_MS, 0, None),
            10000
        );
        assert_eq!(
            retry_wait_ms(RetryClass::Transient, SSE_RETRY_BACKOFF_MS, 1, None),
            20000
        );
    }

    #[test]
    fn retry_wait_throttled_uses_slow_schedule() {
        assert_eq!(retry_wait_ms(RetryClass::Throttled, &[1], 0, None), 5000);
        assert_eq!(retry_wait_ms(RetryClass::Throttled, &[1], 1, None), 15000);
        assert_eq!(retry_wait_ms(RetryClass::Throttled, &[1], 2, None), 30000);
        assert_eq!(retry_wait_ms(RetryClass::Throttled, &[1], 3, None), 60000);
    }

    #[test]
    fn retry_wait_honors_retry_after_as_floor() {
        // Retry-After larger than the fixed schedule wins.
        assert_eq!(
            retry_wait_ms(RetryClass::Throttled, &[1], 0, Some(30)),
            30000
        );
        // Retry-After smaller than the fixed schedule does not shrink it.
        assert_eq!(
            retry_wait_ms(RetryClass::Throttled, &[1], 1, Some(5)),
            15000
        );
    }

    #[test]
    fn retry_wait_caps_at_max_backoff() {
        assert_eq!(
            retry_wait_ms(RetryClass::Throttled, &[1], 3, Some(3600)),
            MAX_BACKOFF_MS,
            "pathological Retry-After must be capped"
        );
    }

    #[test]
    fn retry_wait_clamps_attempt_idx_to_schedule() {
        // A mid-loop class change can push attempt_idx past the transient
        // schedule's end — clamp instead of panicking.
        assert_eq!(
            retry_wait_ms(RetryClass::Transient, SSE_RETRY_BACKOFF_MS, 5, None),
            20000
        );
    }

    #[test]
    fn max_attempts_per_class() {
        assert_eq!(max_attempts_for(RetryClass::Transient), SSE_MAX_ATTEMPTS);
        assert_eq!(
            max_attempts_for(RetryClass::Throttled),
            THROTTLED_MAX_ATTEMPTS
        );
        assert_eq!(max_attempts_for(RetryClass::Terminal), SSE_MAX_ATTEMPTS);
    }
}
