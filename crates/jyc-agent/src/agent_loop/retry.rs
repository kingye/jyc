//! Retry logic for LLM calls (SSE + throttled classes).
//!
//! Extracted from the monolithic `agent_loop.rs`.

use anyhow::Result;
use chrono::Utc;
use tokio_util::sync::CancellationToken;

use super::publish_event;
use super::response::{CollectedResponse, collect_response};
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

        let result: Result<CollectedResponse> = tokio::select! {
            r = async {
                let stream = provider
                    .complete_raw(raw_context, tools, system_prompt)
                    .await?;
                collect_response(
                    stream,
                    sse_read_timeout,
                    event_bus,
                    topic_name,
                    thinking_enabled,
                )
                .await
            } => r,
            _ = cancel.cancelled() => {
                return Err(anyhow::anyhow!("cancelled during LLM call"));
            }
        };

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

    use super::super::event_test_helpers::drain_events;
    use crate::types::ContentBlock;

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
