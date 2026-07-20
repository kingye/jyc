//! LLM provider abstraction.
//!
//! Defines the `Provider` trait and implementations for:
//! - Anthropic Messages API (native)
//! - OpenAI-compatible Chat Completions API

pub mod anthropic;
pub mod openai_compat;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

use crate::types::{Message, StreamEvent, ToolDefinition};

/// Stream of events from an LLM provider.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

/// Filter raw messages before sending to any LLM API.
///
/// Removes assistant messages that have no meaningful content and no tool_calls.
/// Such messages are invalid for replay — even if they have reasoning_content
/// (DeepSeek) or other provider-specific fields.
///
/// Also removes assistant messages whose `tool_calls` lack matching tool result
/// messages (dangling tool_calls), along with all subsequent messages. API
/// providers reject contexts where a tool_call_id does not have a corresponding
/// tool/tool_result response.
///
/// IMPORTANT: `reasoning_content` on real assistant turns is preserved. DeepSeek
/// reasoner models (with `thinking = enabled`) require that reasoning_content
/// produced by the model be replayed back on subsequent requests; stripping it
/// triggers HTTP 400 with `"The reasoning_content in the thinking mode must be
/// passed back to the API."` (Issue diagnosed in v0.3.7 after a wrong fix in
/// v0.3.6 that did the opposite.)
///
/// Handles both formats:
/// - OpenAI: `"content": "text"` + `"tool_calls": [...]`
/// - Anthropic: `"content": [{"type": "text", "text": "..."}, {"type": "tool_use", ...}]`
pub fn filter_valid_messages(raw_messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let filtered: Vec<serde_json::Value> = raw_messages
        .iter()
        .filter(|m| {
            if m.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                return true;
            }
            // OpenAI format: content as non-empty string
            let has_string_content = m
                .get("content")
                .and_then(|c| c.as_str())
                .is_some_and(|s| !s.is_empty());
            // Anthropic format: content as array with meaningful blocks
            let has_array_content =
                m.get("content")
                    .and_then(|c| c.as_array())
                    .is_some_and(|blocks| {
                        blocks.iter().any(|b| {
                            let t = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            t == "tool_use"
                                || (t == "text"
                                    && b.get("text")
                                        .and_then(|x| x.as_str())
                                        .is_some_and(|s| !s.is_empty()))
                        })
                    });
            // OpenAI format: tool_calls array
            let has_tool_calls = m
                .get("tool_calls")
                .and_then(|t| t.as_array())
                .is_some_and(|a| !a.is_empty());

            has_string_content || has_array_content || has_tool_calls
        })
        .cloned()
        .collect();

    repair_dangling_tool_calls(filtered)
}

/// Remove assistant messages whose `tool_calls` lack matching `tool` result
/// messages, along with all subsequent messages that depend on them.
///
/// This repairs contexts corrupted by mid-execution cancellation or process
/// crashes: the assistant message was persisted but not all tool results
/// were appended, causing the API to reject the next request with
/// `"tool_call_ids did not have response messages"`.
///
/// For both OpenAI (`tool_calls` array) and Anthropic (`content` with
/// `tool_use` blocks) formats, extracts the tool call IDs and checks that
/// each has a corresponding `role: "tool"` (OpenAI) or `tool_result`
/// (Anthropic) message. If any are missing, the assistant message and
/// everything after it is dropped — later messages may depend on the
/// missing tool results and would create cascading errors.
fn repair_dangling_tool_calls(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut result = Vec::with_capacity(messages.len());

    for (i, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

        if role == "assistant" {
            // Collect tool call IDs from this assistant message.
            let tool_call_ids: Vec<String> = extract_tool_call_ids(msg);

            if !tool_call_ids.is_empty() {
                // Check that every tool_call_id has a matching tool result
                // in the subsequent messages.
                let remaining = &messages[i + 1..];
                let all_responded = tool_call_ids.iter().all(|id| {
                    remaining.iter().any(|m| {
                        m.get("role").and_then(|r| r.as_str()) == Some("tool")
                            && m.get("tool_call_id").and_then(|t| t.as_str()) == Some(id.as_str())
                    }) || remaining.iter().any(|m| {
                        // Anthropic format: tool_result block in a user message
                        m.get("role").and_then(|r| r.as_str()) == Some("user")
                            && m.get("content")
                                .and_then(|c| c.as_array())
                                .is_some_and(|blocks| {
                                    blocks.iter().any(|b| {
                                        b.get("type").and_then(|t| t.as_str())
                                            == Some("tool_result")
                                            && b.get("tool_use_id").and_then(|t| t.as_str())
                                                == Some(id.as_str())
                                    })
                                })
                    })
                });

                if !all_responded {
                    let missing: Vec<&String> = tool_call_ids
                        .iter()
                        .filter(|id| {
                            !remaining.iter().any(|m| {
                                m.get("role").and_then(|r| r.as_str()) == Some("tool")
                                    && m.get("tool_call_id").and_then(|t| t.as_str())
                                        == Some(id.as_str())
                            })
                        })
                        .collect();
                    tracing::warn!(
                        position = i,
                        total_before = messages.len(),
                        remaining = remaining.len(),
                        missing_ids = ?missing,
                        tool_call_ids = ?tool_call_ids,
                        "Dropping assistant message with dangling tool_calls and all subsequent messages"
                    );
                    // Drop this message and everything after it.
                    break;
                }
            }
        }

        result.push(msg.clone());
    }

    result
}

/// Extract tool call IDs from an assistant message (both OpenAI and
/// Anthropic formats).
fn extract_tool_call_ids(msg: &serde_json::Value) -> Vec<String> {
    let mut ids = Vec::new();

    // OpenAI format: tool_calls array
    if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                ids.push(id.to_string());
            }
        }
    }

    // Anthropic format: content array with tool_use blocks
    if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                && let Some(id) = block.get("id").and_then(|i| i.as_str())
            {
                ids.push(id.to_string());
            }
        }
    }

    ids
}

/// Trait for LLM providers.
///
/// Minimal interface: send messages with tools, get a streaming response.
/// Providers also handle raw context serialization for conversation persistence.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider name (e.g., "anthropic", "deepseek").
    fn name(&self) -> &str;

    /// Model identifier being used.
    fn model(&self) -> &str;

    /// Whether the active model accepts image content blocks (multimodal input).
    /// Resolved at construction time from config (`ModelConfig.supports_images`
    /// overrides `ProviderConfig.supports_images`; default false).
    fn supports_images(&self) -> bool {
        false
    }

    /// Send messages and get a streaming response.
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
    ) -> Result<EventStream>;

    /// Format a user message as raw provider JSON (for context persistence).
    ///
    /// Accepts arbitrary content blocks so multimodal user turns (text + images)
    /// can be expressed by callers. Providers that do not support images should
    /// gracefully degrade (e.g., serialize only the text blocks).
    fn format_user_message(&self, blocks: &[crate::types::ContentBlock]) -> serde_json::Value;

    /// Format a tool result as raw provider JSON (for context persistence).
    fn format_tool_result(
        &self,
        tool_call_id: &str,
        content: &str,
        is_error: bool,
    ) -> serde_json::Value;

    /// Build the raw assistant message JSON from a collected streaming response.
    /// This captures provider-specific fields (e.g., DeepSeek's reasoning_content)
    /// that must be round-tripped in subsequent API calls.
    fn build_raw_assistant_message(
        &self,
        text: &str,
        reasoning: &str,
        tool_calls: &[(String, String, String)], // (id, name, arguments)
    ) -> serde_json::Value;

    /// Send raw context messages directly to the API (for replaying persisted context).
    /// This bypasses the internal Message conversion and sends raw JSON.
    async fn complete_raw(
        &self,
        raw_messages: &[serde_json::Value],
        tools: &[ToolDefinition],
        system: &str,
    ) -> Result<EventStream>;
}

/// Create a provider from configuration.
///
/// Parses the model string (format: "provider_name/model_id") and creates
/// the appropriate provider instance.
///
/// Supports formats:
/// - "anthropic/claude-opus-4-6" → provider="anthropic", model="claude-opus-4-6"
/// - "deepseek/deepseek-v4-pro" → provider="deepseek", model="deepseek-v4-pro"
/// - "ark/ep-xxxxx" → provider="ark", model="ep-xxxxx"
///
/// The provider_name must match a key in the `[agent.providers.*]` config.
pub fn create_provider(
    model: &str,
    providers: &std::collections::HashMap<String, crate::types::ProviderConfig>,
) -> Result<Box<dyn Provider>> {
    let (provider_name, model_id) = model
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!(
            "Invalid model format '{}'. Expected 'provider/model-id' (e.g., 'anthropic/claude-opus-4-6')",
            model
        ))?;

    let config = providers.get(provider_name).ok_or_else(|| {
        anyhow::anyhow!(
            "Provider '{}' not found in [agent.providers]. Available: {:?}. \
             Add [agent.providers.{}] to config.toml.",
            provider_name,
            providers.keys().collect::<Vec<_>>(),
            provider_name
        )
    })?;

    // Read API key from environment
    let api_key = if let Some(env_var) = &config.api_key_env {
        std::env::var(env_var).ok()
    } else {
        None
    };

    // Resolve the wire model id: per-model `model_id` override, else the
    // models-map key. Config lookups (params, supports_images, ...) below
    // still use the map key, so multiple aliases can share one remote id.
    let wire_model_id = config
        .models
        .get(model_id)
        .and_then(|m| m.model_id.as_deref())
        .unwrap_or(model_id);

    // Resolve params: model-level overrides provider-level (shallow merge)
    let params = resolve_params(
        config.params.as_ref(),
        config.models.get(model_id).and_then(|m| m.params.as_ref()),
    );

    // Resolve supports_images: model-level overrides provider-level; default false.
    let supports_images = config
        .models
        .get(model_id)
        .and_then(|m| m.supports_images)
        .or(config.supports_images)
        .unwrap_or(false);

    // Resolve user_agent: model-level overrides provider-level.
    let user_agent = config
        .models
        .get(model_id)
        .and_then(|m| m.user_agent.as_deref())
        .or(config.user_agent.as_deref());

    match config.provider_type.as_str() {
        "anthropic" => {
            let base_url = config
                .base_url
                .as_deref()
                .unwrap_or("https://api.anthropic.com/v1");
            Ok(Box::new(anthropic::AnthropicProvider::new(
                base_url,
                wire_model_id,
                api_key.as_deref(),
                params,
                supports_images,
            )?))
        }
        "openai-compatible" | "openai" => {
            let base_url = config.base_url.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "OpenAI-compatible provider '{}' requires base_url",
                    provider_name
                )
            })?;
            Ok(Box::new(openai_compat::OpenAiCompatProvider::new(
                base_url,
                wire_model_id,
                api_key.as_deref(),
                params,
                supports_images,
                user_agent,
            )?))
        }
        other => anyhow::bail!(
            "Unknown provider type '{}' for provider '{}'",
            other,
            provider_name
        ),
    }
}

/// Issue a one-shot diagnostic POST with the same payload to capture the HTTP
/// status and response body when an SSE connection failed at the transport
/// or HTTP layer (typically a pre-stream `4xx` like 400 Bad Request).
///
/// `EventSource`'s error string for these cases is just `"Invalid status
/// code: 400 Bad Request"` and discards the response body. The provider's
/// actual error message — which is the only thing useful for diagnosis —
/// lives in that body. This helper recovers it via a single follow-up POST.
///
/// Used only on error paths — adds latency exclusively when something is
/// already broken. Returns `None` on network failure (in which case the
/// original SSE error is more informative anyway).
///
/// `apply_auth` lets each provider attach its own auth headers (OpenAI uses
/// `Authorization: Bearer <key>`, Anthropic uses `x-api-key: <key>` plus
/// `anthropic-version`). The closure is invoked once per call.
///
/// The captured body is truncated at 2000 bytes — enough for the leading
/// JSON error message from any sane provider, while bounding memory if the
/// upstream returns a huge HTML error page.
/// Diagnostic information captured by [`fetch_error_body`].
pub struct DiagInfo {
    /// HTTP status code of the diagnostic response.
    pub status: u16,
    /// Value of the `Retry-After` response header in whole seconds, when
    /// present and parseable as an integer. HTTP-date form is not parsed.
    pub retry_after: Option<u64>,
    /// Truncated response body (provider's actual error message).
    pub body: String,
}

pub async fn fetch_error_body<F>(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    apply_auth: F,
) -> Option<DiagInfo>
where
    F: FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
{
    // Force stream=false so the server returns a one-shot JSON error
    // response instead of an infinite SSE stream. Without this,
    // resp.text() hangs reading the stream until timeout, and the
    // diagnostic body is never captured.
    let mut body = body.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), serde_json::Value::Bool(false));
    }
    let req = client
        .post(url)
        .header("content-type", "application/json")
        .json(&body);
    let req = apply_auth(req);
    // Short, explicit timeout — this is a best-effort diagnostic POST on an
    // already-broken connection. The client-level timeout is 300s, which would
    // stall the thread for 5 minutes if the upstream hangs.
    let resp = req
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    let status = resp.status().as_u16();
    // Capture Retry-After (integer-seconds form only) so throttled retries
    // can honor the provider's requested wait window (#391).
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    let text = resp
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable body>".to_string());
    // Truncate very large bodies — we just need the leading error message.
    let trimmed = if text.len() > 2000 {
        format!("{}…(truncated, {} bytes total)", &text[..2000], text.len())
    } else {
        text
    };
    Some(DiagInfo {
        status,
        retry_after,
        body: trimmed,
    })
}

/// Format the diagnostic suffix appended to SSE error messages, e.g.
/// `(HTTP 429 retry-after: 30s body: {...})` or `(HTTP 400 body: {...})`
/// when no Retry-After header was captured.
pub fn format_diag_suffix(diag: &DiagInfo) -> String {
    match diag.retry_after {
        Some(secs) => format!(
            "(HTTP {} retry-after: {}s body: {})",
            diag.status, secs, diag.body
        ),
        None => format!("(HTTP {} body: {})", diag.status, diag.body),
    }
}

/// Retry classification for a failed LLM call (#391).
///
/// Used by `agent_loop` to pick a retry policy per failure:
/// - [`RetryClass::Transient`] — transport-level blips (TCP RST mid-stream,
///   body decode glitch, idle timeout, stale-connection send failure).
///   Fast retry schedule (few attempts, short backoff).
/// - [`RetryClass::Throttled`] — rate-limited or overloaded upstream
///   (HTTP 429 / 502 / 503 / 504). Slow retry schedule (more attempts,
///   longer backoff), honoring `Retry-After` when captured.
/// - [`RetryClass::Terminal`] — structural rejection (auth, quota, schema,
///   model-not-supported). Propagate immediately; retrying won't help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// Transport-level blip — fast retry.
    Transient,
    /// Rate-limited / overloaded (429/502/503/504) — slow retry.
    Throttled,
    /// Structural rejection — no retry.
    Terminal,
}

/// Classify an SSE / network error from `complete_raw` into a [`RetryClass`].
///
/// The classifier is intentionally string-matching the user-visible error
/// message — `complete_raw` returns `anyhow::Error`, and the underlying
/// `reqwest_eventsource::Error` and `reqwest::Error` types do not provide a
/// stable enum we can match through `anyhow::Error::downcast_ref` (the
/// errors are wrapped via `anyhow!("SSE stream error: {e}")` which loses
/// the source chain). String matching the well-known patterns is adequate
/// and easy to extend.
///
/// ## Diagnostic-suffix awareness
///
/// `fetch_error_body` may have appended `(HTTP <code> [retry-after: Ns]
/// body: <body>)` to the error after issuing a one-shot diagnostic POST.
/// The status code carried in that suffix is authoritative:
///
/// - `429` / `502` / `503` / `504` → rate-limit or overloaded gateway;
///   resolves after a wait window. **Throttled.**
/// - Other `4xx` / `5xx` → the request is structurally rejected (auth, quota,
///   schema, model-not-supported). **Terminal.**
/// - `2xx` → the diagnostic POST succeeded. The original SSE failure was
///   purely a transport-level glitch (stale connection in pool, NAT idle
///   reset, partial-write etc.). The diag confirms the upstream is fine
///   and a fresh attempt will likely succeed. **Transient.**
/// - `3xx` (rare) → treat as transient; safe re-issue.
///
/// Without the diag suffix, an `"Invalid status code: NNN"` pre-stream
/// rejection is classified by the embedded code the same way; any other
/// message falls back to substring matching against the well-known
/// transient patterns.
///
/// ## Transient patterns (substring match, case-insensitive)
///
/// - `"error decoding response body"` — reqwest's body decoder hit a
///   chunked-encoding glitch, malformed UTF-8, or premature EOF.
/// - `"error sending request"` — reqwest's transport-level send failure,
///   typically a stale connection from the pool that got silently dropped
///   by a NAT/load-balancer/peer. Almost always recoverable.
/// - `"stream ended"` — provider closed the SSE before `[DONE]`.
/// - `"connection reset"` / `"connection closed"` / `"broken pipe"` —
///   TCP-level transport interruption.
/// - `"operation timed out"` / `"request timed out"` / `"timed out"` —
///   reqwest's 300s timeout fired or an SSE idle-read timed out.
/// - `"dns error"` / `"tcp connect error"` — pre-connection failures.
/// - `"transport error"` / `"incomplete message"` / `"unexpected eof"` —
///   misc transport blips.
pub fn classify_retry(err: &anyhow::Error) -> RetryClass {
    let msg = format!("{:#}", err);
    let lower = msg.to_lowercase();

    // If the diagnostic POST captured a status code, trust it.
    if let Some(status) = extract_diag_status(&msg) {
        return classify_http_status(status);
    }

    // No diag suffix — a pre-stream "Invalid status code: NNN" rejection
    // still carries the code; classify it the same way. Unknown codes and
    // other pre-stream rejections stay terminal.
    if lower.contains("invalid status code") {
        return match extract_invalid_status(&lower) {
            Some(status) => classify_http_status(status),
            None => RetryClass::Terminal,
        };
    }

    if matches_transient_pattern(&lower) {
        RetryClass::Transient
    } else {
        RetryClass::Terminal
    }
}

/// Map an HTTP status code to a retry class: 429/502/503/504 are throttled,
/// other 4xx/5xx are terminal, everything else is transient.
fn classify_http_status(status: u16) -> RetryClass {
    match status {
        429 | 502 | 503 | 504 => RetryClass::Throttled,
        s if (400..600).contains(&s) => RetryClass::Terminal,
        _ => RetryClass::Transient,
    }
}

/// Parse the HTTP status code from an `"invalid status code: NNN"` message
/// (reqwest_eventsource's pre-stream rejection text).
fn extract_invalid_status(lower_msg: &str) -> Option<u16> {
    let start = lower_msg.find("invalid status code:")? + "invalid status code:".len();
    let rest = lower_msg.get(start..)?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Parse the HTTP status code from the `(HTTP <code> ...)` suffix
/// appended by `fetch_error_body`. Returns `None` when the suffix is not
/// present or the code is malformed.
fn extract_diag_status(msg: &str) -> Option<u16> {
    let start = msg.find("(HTTP ")? + "(HTTP ".len();
    let rest = msg.get(start..)?;
    let end = rest.find(' ')?;
    rest.get(..end)?.parse().ok()
}

/// Parse the `Retry-After` value (whole seconds) from the
/// `(HTTP <code> retry-after: Ns body: ...)` suffix appended by
/// `fetch_error_body`. Returns `None` when the header was not captured.
pub fn extract_retry_after(msg: &str) -> Option<u64> {
    let start = msg.find("retry-after: ")? + "retry-after: ".len();
    let rest = msg.get(start..)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn matches_transient_pattern(lower_msg: &str) -> bool {
    const TRANSIENT_PATTERNS: &[&str] = &[
        "error decoding response body",
        "error sending request",
        "stream ended",
        "connection reset",
        "connection closed",
        "broken pipe",
        "operation timed out",
        "request timed out",
        "timed out",
        "dns error",
        "tcp connect error",
        "transport error",
        "incomplete message",
        "unexpected eof",
    ];
    TRANSIENT_PATTERNS.iter().any(|p| lower_msg.contains(p))
}

/// Merge provider-level params with model-level params.
/// Model params override provider params (shallow merge of top-level keys).
fn resolve_params(
    provider_params: Option<&serde_json::Value>,
    model_params: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    match (provider_params, model_params) {
        (None, None) => None,
        (Some(p), None) => Some(p.clone()),
        (None, Some(m)) => Some(m.clone()),
        (Some(p), Some(m)) => {
            // Shallow merge: model keys override provider keys
            let mut merged = p.clone();
            if let (Some(base), Some(overlay)) = (merged.as_object_mut(), m.as_object()) {
                for (k, v) in overlay {
                    base.insert(k.clone(), v.clone());
                }
            }
            Some(merged)
        }
    }
}

#[cfg(test)]
mod classifier_tests {
    use super::*;

    fn err(msg: &str) -> anyhow::Error {
        anyhow::anyhow!("{}", msg)
    }

    #[test]
    fn diag_status_2xx_is_transient() {
        // Real production case (May 26 12:04:05): SSE failed mid-flight,
        // diag re-POST returned 200 with a healthy first chunk.
        // Retrying must succeed.
        let e = err("SSE stream error: error sending request for url \
             (https://api.deepseek.com/chat/completions) \
             (HTTP 200 body: data: {\"id\":\"abc\",\"choices\":[...]})");
        assert_eq!(
            classify_retry(&e),
            RetryClass::Transient,
            "diag-200 confirms upstream healthy → must be transient"
        );
    }

    #[test]
    fn diag_status_4xx_is_terminal() {
        // Diag captured a structured rejection — retrying won't help.
        let e = err("SSE stream error: Invalid status code: 400 Bad Request \
             (HTTP 400 body: {\"error\":{\"message\":\"bad payload\"}})");
        assert_eq!(
            classify_retry(&e),
            RetryClass::Terminal,
            "diag-400 is a structured rejection → terminal"
        );
    }

    #[test]
    fn diag_status_503_is_throttled() {
        // 503 Service Unavailable is an overloaded upstream — transient in
        // nature, worth retrying on the slow schedule (#391).
        let e = err(
            "SSE stream error: Invalid status code: 503 Service Unavailable \
             (HTTP 503 body: {\"error\":\"upstream down\"})",
        );
        assert_eq!(classify_retry(&e), RetryClass::Throttled);
    }

    #[test]
    fn diag_status_502_504_are_throttled() {
        for status in [502, 504] {
            let e = err(&format!(
                "SSE stream error: Invalid status code: {status} \
                 (HTTP {status} body: {{\"error\":\"gateway\"}})"
            ));
            assert_eq!(
                classify_retry(&e),
                RetryClass::Throttled,
                "diag-{status} is a gateway blip → throttled"
            );
        }
    }

    #[test]
    fn diag_status_429_is_throttled() {
        // 429 Too Many Requests — rate-limit that resolves after
        // the retry window. Retry on the slow schedule (#391).
        let e = err("SSE stream error: error sending request for url \
             (https://api.deepseek.com/chat/completions) \
             (HTTP 429 body: {\"error\":{\"message\":\"rate limit exceeded\"}})");
        assert_eq!(
            classify_retry(&e),
            RetryClass::Throttled,
            "diag-429 is a rate-limit → throttled"
        );
    }

    #[test]
    fn invalid_status_429_no_diag_is_throttled() {
        // Diag POST itself failed — the pre-stream rejection code is still
        // visible in the message and must be honored (#391).
        let e = err("SSE stream error: Invalid status code: 429 Too Many Requests");
        assert_eq!(classify_retry(&e), RetryClass::Throttled);
    }

    #[test]
    fn extract_diag_status_429() {
        assert_eq!(
            extract_diag_status("foo (HTTP 429 body: {\"error\": ...})"),
            Some(429)
        );
    }

    #[test]
    fn decode_body_error_no_diag_is_transient() {
        // Pre-this-fix production case: reqwest body decoder glitched
        // mid-stream, diag wasn't issued (already past Event::Open).
        let e = err("SSE stream error: error decoding response body");
        assert_eq!(classify_retry(&e), RetryClass::Transient);
    }

    #[test]
    fn invalid_status_no_diag_is_terminal() {
        // No diag suffix and "Invalid status code" → pre-stream rejection
        // with no recoverable body. Retry would hit the same wall.
        let e = err("SSE error: Invalid status code: 401 Unauthorized");
        assert_eq!(classify_retry(&e), RetryClass::Terminal);
    }

    #[test]
    fn error_sending_request_is_transient() {
        // Stale-connection-from-pool failure. Without a diag suffix it
        // still matches the "error sending request" pattern.
        let e = err("SSE stream error: error sending request for url \
             (https://api.deepseek.com/chat/completions)");
        assert_eq!(classify_retry(&e), RetryClass::Transient);
    }

    #[test]
    fn extract_diag_status_basic() {
        assert_eq!(extract_diag_status("foo (HTTP 200 body: bar)"), Some(200));
        assert_eq!(
            extract_diag_status("foo (HTTP 400 body: {\"error\": ...})"),
            Some(400)
        );
        assert_eq!(extract_diag_status("foo (HTTP 503 body: x)"), Some(503));
    }

    #[test]
    fn extract_diag_status_missing_returns_none() {
        assert_eq!(extract_diag_status("plain error"), None);
        assert_eq!(extract_diag_status("(HTTP "), None);
        assert_eq!(extract_diag_status("(HTTP abc body:)"), None);
    }

    #[test]
    fn extract_retry_after_basic() {
        assert_eq!(
            extract_retry_after("SSE error (HTTP 429 retry-after: 30s body: {...})"),
            Some(30)
        );
    }

    #[test]
    fn extract_retry_after_missing_returns_none() {
        assert_eq!(
            extract_retry_after("SSE error (HTTP 429 body: {...})"),
            None
        );
        assert_eq!(extract_retry_after("plain error"), None);
        assert_eq!(extract_retry_after("retry-after: "), None);
    }
}

#[cfg(test)]
mod dangling_tool_call_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_complete_context_not_modified() {
        // Assistant with tool_calls + matching tool result → kept
        let msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "ok"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let result = filter_valid_messages(&msgs);
        assert_eq!(result.len(), 4, "complete context should be unchanged");
    }

    #[test]
    fn openai_dangling_tool_call_dropped() {
        // Assistant with tool_calls but NO matching tool result → dropped
        // along with all subsequent messages
        let msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "working", "tool_calls": [{"id": "bash:57", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "assistant", "content": "this should also be dropped"}),
        ];
        let result = filter_valid_messages(&msgs);
        assert_eq!(
            result.len(),
            1,
            "dangling assistant + subsequent should be dropped"
        );
        assert_eq!(result[0].get("role").and_then(|r| r.as_str()), Some("user"));
    }

    #[test]
    fn openai_partial_tool_results_dropped() {
        // Assistant with 2 tool_calls, only 1 tool result → dangling
        let msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
                {"id": "call_2", "type": "function", "function": {"name": "read", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "ok"}),
            // call_2 has no tool result
        ];
        let result = filter_valid_messages(&msgs);
        assert_eq!(result.len(), 1, "partial results → assistant dropped");
    }

    #[test]
    fn openai_all_tool_results_present_kept() {
        // Assistant with 2 tool_calls, both have results → kept
        let msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
                {"id": "call_2", "type": "function", "function": {"name": "read", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "ok"}),
            json!({"role": "tool", "tool_call_id": "call_2", "content": "file content"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        let result = filter_valid_messages(&msgs);
        assert_eq!(result.len(), 5, "complete context should be unchanged");
    }

    #[test]
    fn no_tool_calls_not_affected() {
        // Regular assistant message (no tool_calls) → not affected
        let msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi there"}),
            json!({"role": "user", "content": "bye"}),
        ];
        let result = filter_valid_messages(&msgs);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn multiple_assistant_messages_only_dangling_dropped() {
        // First assistant has complete tool results, second is dangling
        let msgs = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "ok"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "call_2", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}),
            // call_2 has no result
            json!({"role": "assistant", "content": "should be dropped"}),
        ];
        let result = filter_valid_messages(&msgs);
        assert_eq!(
            result.len(),
            3,
            "first assistant kept, dangling + after dropped"
        );
        // Verify the first assistant is still there
        assert!(result[1].get("tool_calls").is_some());
        // Verify tool result is there
        assert_eq!(
            result[2].get("tool_call_id").and_then(|t| t.as_str()),
            Some("call_1")
        );
    }

    #[test]
    fn extract_ids_openai_format() {
        let msg = json!({"role": "assistant", "tool_calls": [
            {"id": "call_a", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
            {"id": "call_b", "type": "function", "function": {"name": "read", "arguments": "{}"}}
        ]});
        let ids = extract_tool_call_ids(&msg);
        assert_eq!(ids, vec!["call_a", "call_b"]);
    }

    #[test]
    fn extract_ids_anthropic_format() {
        let msg = json!({"role": "assistant", "content": [
            {"type": "text", "text": "thinking..."},
            {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {}}
        ]});
        let ids = extract_tool_call_ids(&msg);
        assert_eq!(ids, vec!["toolu_1"]);
    }

    #[test]
    fn extract_ids_no_tool_calls() {
        let msg = json!({"role": "assistant", "content": "just text"});
        let ids = extract_tool_call_ids(&msg);
        assert!(ids.is_empty());
    }
}

#[cfg(test)]
mod model_id_tests {
    use super::*;
    use crate::types::{ModelConfig, ProviderConfig};
    use std::collections::HashMap;

    fn model_config(model_id: Option<&str>) -> ModelConfig {
        ModelConfig {
            model_id: model_id.map(|s| s.to_string()),
            context_window: None,
            supports_images: None,
            params: None,
            user_agent: None,
        }
    }

    fn providers_with(models: HashMap<String, ModelConfig>) -> HashMap<String, ProviderConfig> {
        let mut providers = HashMap::new();
        providers.insert(
            "kimi".to_string(),
            ProviderConfig {
                provider_type: "openai-compatible".to_string(),
                base_url: Some("https://api.moonshot.cn/v1".to_string()),
                api_key_env: None,
                context_window: None,
                supports_images: None,
                params: None,
                user_agent: None,
                models,
            },
        );
        providers
    }

    #[test]
    fn model_id_override_is_sent_to_wire() {
        // Alias "k3-high" maps to the real remote id "k3" (#389).
        let mut models = HashMap::new();
        models.insert("k3-high".to_string(), model_config(Some("k3")));
        let providers = providers_with(models);

        let provider = create_provider("kimi/k3-high", &providers).unwrap();
        assert_eq!(provider.model(), "k3");
    }

    #[test]
    fn missing_model_id_falls_back_to_map_key() {
        let mut models = HashMap::new();
        models.insert("k3".to_string(), model_config(None));
        let providers = providers_with(models);

        let provider = create_provider("kimi/k3", &providers).unwrap();
        assert_eq!(provider.model(), "k3");
    }

    #[test]
    fn model_without_config_entry_falls_back_to_map_key() {
        let providers = providers_with(HashMap::new());

        let provider = create_provider("kimi/k3", &providers).unwrap();
        assert_eq!(provider.model(), "k3");
    }

    #[test]
    fn multiple_aliases_share_one_wire_id() {
        // The issue-389 use case: same remote model, different params
        // per alias. Both aliases must resolve to the same wire id.
        let mut models = HashMap::new();
        models.insert("k3-high".to_string(), model_config(Some("k3")));
        models.insert("k3-low".to_string(), model_config(Some("k3")));
        let providers = providers_with(models);

        let high = create_provider("kimi/k3-high", &providers).unwrap();
        let low = create_provider("kimi/k3-low", &providers).unwrap();
        assert_eq!(high.model(), "k3");
        assert_eq!(low.model(), "k3");
    }
}
