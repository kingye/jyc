//! LLM provider abstraction.
//!
//! Defines the `Provider` trait and implementations for:
//! - Anthropic Messages API (native)
//! - OpenAI-compatible Chat Completions API

pub mod anthropic;
pub mod openai_compat;
pub mod openai_responses;
pub mod sse;
pub mod usage;

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
    /// Resolved at construction time from config (`ModelDef.supports_images`
    /// overrides `ProviderDef.supports_images`; default false).
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
/// Merge provider/model-level extra params into a request body.
///
/// `params` is the optional `{ "extra": { ... } }` config value; when it is
/// an object, each key/value is inserted into `body` (overriding defaults).
pub fn merge_params(body: &mut serde_json::Value, params: &Option<serde_json::Value>) {
    if let Some(params) = params
        && let Some(params_obj) = params.as_object()
        && let Some(body_obj) = body.as_object_mut()
    {
        for (k, v) in params_obj {
            body_obj.insert(k.clone(), v.clone());
        }
    }
}

/// Expand `{channel}` and `{topic}` placeholders in every string value
/// of each provider's `params` (provider- and model-level, recursively).
///
/// Lets request-level fields vary per session without a static config
/// value per topic — e.g. OpenAI's opt-in prompt caching:
/// `params = { prompt_cache_key = "jyc-{channel}-{topic}", ... }` gives
/// every topic its own cache-affinity bucket. Cache *safety* never
/// depends on the key (hits are matched by prompt-prefix content); the
/// key only keeps different workloads from evicting each other's
/// entries.
pub fn expand_params_placeholders(
    providers: &mut std::collections::HashMap<String, jyc_types::ProviderDef>,
    channel: &str,
    topic: &str,
) {
    fn expand(value: &mut serde_json::Value, channel: &str, topic: &str) {
        match value {
            serde_json::Value::String(s) if s.contains('{') => {
                *s = s.replace("{channel}", channel).replace("{topic}", topic);
            }
            serde_json::Value::Array(items) => {
                for v in items {
                    expand(v, channel, topic);
                }
            }
            serde_json::Value::Object(map) => {
                for v in map.values_mut() {
                    expand(v, channel, topic);
                }
            }
            _ => {}
        }
    }
    for def in providers.values_mut() {
        if let Some(params) = &mut def.params {
            expand(params, channel, topic);
        }
        for model in def.models.values_mut() {
            if let Some(params) = &mut model.params {
                expand(params, channel, topic);
            }
        }
    }
}

pub fn create_provider(
    model: &str,
    providers: &std::collections::HashMap<String, jyc_types::ProviderDef>,
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

    // Read API key from environment.
    // Order: api_key_env (legacy, late-bound) → api_key (already-expanded
    // by the TOML loader from ${VAR} syntax). Both fields are accepted
    // for backward compatibility; see ProviderDef::resolve_api_key.
    let api_key = config.resolve_api_key();

    // Warn if both fields are set — the user probably didn't mean to.
    if config.api_key.is_some() && config.api_key_env.is_some() {
        tracing::warn!(
            provider = %provider_name,
            "Both `api_key` and `api_key_env` are set on this provider; \
             `api_key_env` wins (legacy precedence). Remove `api_key_env` \
             to silence this warning."
        );
    }

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
            // Resolve cache_ttl: model-level overrides provider-level.
            let cache_ttl_1h = match config
                .models
                .get(model_id)
                .and_then(|m| m.cache_ttl.as_deref())
                .or(config.cache_ttl.as_deref())
            {
                None | Some("5m") => false,
                Some("1h") => true,
                Some(other) => anyhow::bail!(
                    "Invalid cache_ttl '{}' for provider '{}': expected \"5m\" or \"1h\"",
                    other,
                    provider_name
                ),
            };
            Ok(Box::new(anthropic::AnthropicProvider::new(
                base_url,
                wire_model_id,
                api_key.as_deref(),
                params,
                supports_images,
                user_agent,
                cache_ttl_1h,
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
        "openai-responses" => {
            let base_url = config.base_url.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "OpenAI Responses provider '{}' requires base_url",
                    provider_name
                )
            })?;
            Ok(Box::new(openai_responses::OpenAiResponsesProvider::new(
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

/// Retry classification for a failed LLM call (#391).
///
/// Used by `agent_loop` to pick a retry policy per failure:
/// - [`RetryClass::Transient`] — transport-level blips (TCP RST mid-stream,
///   body decode glitch, idle timeout, stale-connection send failure).
///   Fixed schedule (3 attempts, 10s/20s backoff).
/// - [`RetryClass::Throttled`] — rate-limited or overloaded upstream
///   (HTTP 429 / 502 / 503 / 504). Slow retry schedule (more attempts,
///   longer backoff), honoring `Retry-After` when captured.
/// - [`RetryClass::Terminal`] — structural rejection (auth, quota, schema,
///   model-not-supported). Propagate immediately; retrying won't help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// Transport-level blip — brief retry (10s/20s backoff).
    Transient,
    /// Rate-limited / overloaded (429/502/503/504) — slow retry.
    Throttled,
    /// Structural rejection — no retry.
    Terminal,
}

/// Classify an SSE / network error from `complete_raw` into a [`RetryClass`].
///
/// The classifier is intentionally string-matching the user-visible error
/// message — `complete_raw` returns `anyhow::Error` with the underlying
/// SSE / reqwest error folded into its Display, so there is no stable enum
/// to match through `anyhow::Error::downcast_ref`. String matching the
/// well-known patterns is adequate and easy to extend.
///
/// ## Status-code awareness
///
/// The SSE client embeds the upstream HTTP status (and `Retry-After`, when
/// present) directly in the error, e.g.
/// `Invalid status code: 429 retry-after: 30s body: {...}`. The code is
/// authoritative:
///
/// - `429` / `502` / `503` / `504` → rate-limit or overloaded gateway;
///   resolves after a wait window. **Throttled.**
/// - Other `4xx` / `5xx` → the request is structurally rejected (auth, quota,
///   schema, model-not-supported). **Terminal.**
/// - Anything else → transient; safe to re-issue.
///
/// An `"Invalid status code: NNN"` rejection is classified by the embedded
/// code; any other message falls back to substring matching against the
/// well-known transient patterns.
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
/// (the SSE client's pre-stream rejection text).
fn extract_invalid_status(lower_msg: &str) -> Option<u16> {
    let start = lower_msg.find("invalid status code:")? + "invalid status code:".len();
    let rest = lower_msg.get(start..)?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Parse the HTTP status code from a `(HTTP <code> ...)` suffix, if any
/// provider error body happens to carry one. Returns `None` when absent.
fn extract_diag_status(msg: &str) -> Option<u16> {
    let start = msg.find("(HTTP ")? + "(HTTP ".len();
    let rest = msg.get(start..)?;
    let end = rest.find(' ')?;
    rest.get(..end)?.parse().ok()
}

/// Parse the `Retry-After` value (whole seconds) from an SSE error message
/// produced by the SSE client (`... retry-after: Ns body: ...`).
/// Returns `None` when the header was not captured.
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
    use jyc_types::{ModelDef, ProviderDef};
    use std::collections::HashMap;

    fn model_config(model_id: Option<&str>) -> ModelDef {
        ModelDef {
            model_id: model_id.map(|s| s.to_string()),
            context_window: None,
            supports_images: None,
            params: None,
            user_agent: None,
            cache_ttl: None,
            pricing: None,
        }
    }

    fn providers_with(models: HashMap<String, ModelDef>) -> HashMap<String, ProviderDef> {
        let mut providers = HashMap::new();
        providers.insert(
            "kimi".to_string(),
            ProviderDef {
                provider_type: "openai-compatible".to_string(),
                base_url: Some("https://api.moonshot.cn/v1".to_string()),
                api_key: None,
                api_key_env: None,
                context_window: None,
                supports_images: None,
                params: None,
                user_agent: None,
                cache_ttl: None,
                pricing: None,
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

#[cfg(test)]
mod params_placeholder_tests {
    use super::*;
    use jyc_types::{ModelDef, ProviderDef};
    use std::collections::HashMap;

    fn providers() -> HashMap<String, ProviderDef> {
        let mut models = HashMap::new();
        models.insert(
            "gpt-5.6-sol".to_string(),
            ModelDef {
                model_id: None,
                context_window: None,
                supports_images: None,
                params: Some(serde_json::json!({
                    "prompt_cache_key": "model-{topic}",
                })),
                user_agent: None,
                cache_ttl: None,
                pricing: None,
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderDef {
                provider_type: "openai-compatible".to_string(),
                base_url: None,
                api_key: None,
                api_key_env: None,
                context_window: None,
                supports_images: None,
                params: Some(serde_json::json!({
                    "prompt_cache_key": "jyc-{channel}-{topic}",
                    "prompt_cache_options": { "mode": "implicit", "ttl": "30m" },
                    "tags": ["{channel}", "static"],
                    "untouched": 42,
                })),
                user_agent: None,
                cache_ttl: None,
                pricing: None,
                models,
            },
        );
        providers
    }

    #[test]
    fn placeholders_expand_recursively_in_provider_and_model_params() {
        let mut providers = providers();
        expand_params_placeholders(&mut providers, "feishu_work", "proj-a");

        let def = &providers["openai"];
        let params = def.params.as_ref().unwrap();
        assert_eq!(params["prompt_cache_key"], "jyc-feishu_work-proj-a");
        // Nested objects keep non-placeholder strings untouched.
        assert_eq!(
            params["prompt_cache_options"],
            serde_json::json!({ "mode": "implicit", "ttl": "30m" })
        );
        assert_eq!(params["tags"], serde_json::json!(["feishu_work", "static"]));
        assert_eq!(params["untouched"], 42);
        // Model-level params expand too.
        assert_eq!(
            def.models["gpt-5.6-sol"].params.as_ref().unwrap()["prompt_cache_key"],
            "model-proj-a"
        );
    }

    #[test]
    fn strings_without_placeholders_are_untouched() {
        let mut providers = providers();
        for def in providers.values_mut() {
            if let Some(p) = &mut def.params {
                p["prompt_cache_key"] = serde_json::json!("no-placeholders");
            }
        }
        expand_params_placeholders(&mut providers, "feishu_work", "proj-a");
        assert_eq!(
            providers["openai"].params.as_ref().unwrap()["prompt_cache_key"],
            "no-placeholders"
        );
    }
}
