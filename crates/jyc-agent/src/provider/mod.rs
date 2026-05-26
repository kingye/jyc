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
    raw_messages.iter()
        .filter(|m| {
            if m.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                return true;
            }
            // OpenAI format: content as non-empty string
            let has_string_content = m.get("content")
                .and_then(|c| c.as_str())
                .is_some_and(|s| !s.is_empty());
            // Anthropic format: content as array with meaningful blocks
            let has_array_content = m.get("content")
                .and_then(|c| c.as_array())
                .is_some_and(|blocks| blocks.iter().any(|b| {
                    let t = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    t == "tool_use" || (t == "text" && b.get("text").and_then(|x| x.as_str()).is_some_and(|s| !s.is_empty()))
                }));
            // OpenAI format: tool_calls array
            let has_tool_calls = m.get("tool_calls")
                .and_then(|t| t.as_array())
                .is_some_and(|a| !a.is_empty());

            has_string_content || has_array_content || has_tool_calls
        })
        .cloned()
        .collect()
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
    fn supports_images(&self) -> bool { false }

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
    fn format_tool_result(&self, tool_call_id: &str, content: &str, is_error: bool) -> serde_json::Value;

    /// Build the raw assistant message JSON from a collected streaming response.
    /// This captures provider-specific fields (e.g., DeepSeek's reasoning_content)
    /// that must be round-tripped in subsequent API calls.
    fn build_raw_assistant_message(
        &self,
        text: &str,
        reasoning: &str,
        tool_calls: &[(String, String, String)],  // (id, name, arguments)
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

    let config = providers
        .get(provider_name)
        .ok_or_else(|| anyhow::anyhow!(
            "Provider '{}' not found in [agent.providers]. Available: {:?}. \
             Add [agent.providers.{}] to config.toml.",
            provider_name,
            providers.keys().collect::<Vec<_>>(),
            provider_name
        ))?;

    // Read API key from environment
    let api_key = if let Some(env_var) = &config.api_key_env {
        std::env::var(env_var).ok()
    } else {
        None
    };

    // Resolve params: model-level overrides provider-level (shallow merge)
    let params = resolve_params(config.params.as_ref(), config.models.get(model_id).and_then(|m| m.params.as_ref()));

    // Resolve supports_images: model-level overrides provider-level; default false.
    let supports_images = config.models.get(model_id)
        .and_then(|m| m.supports_images)
        .or(config.supports_images)
        .unwrap_or(false);

    match config.provider_type.as_str() {
        "anthropic" => {
            let base_url = config.base_url.as_deref()
                .unwrap_or("https://api.anthropic.com/v1");
            Ok(Box::new(anthropic::AnthropicProvider::new(
                base_url,
                model_id,
                api_key.as_deref(),
                params,
                supports_images,
            )?))
        }
        "openai-compatible" | "openai" => {
            let base_url = config.base_url.as_deref()
                .ok_or_else(|| anyhow::anyhow!("OpenAI-compatible provider '{}' requires base_url", provider_name))?;
            Ok(Box::new(openai_compat::OpenAiCompatProvider::new(
                base_url,
                model_id,
                api_key.as_deref(),
                params,
                supports_images,
            )?))
        }
        other => anyhow::bail!("Unknown provider type '{}' for provider '{}'", other, provider_name),
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
pub async fn fetch_error_body<F>(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    apply_auth: F,
) -> Option<(u16, String)>
where
    F: FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
{
    let req = client
        .post(url)
        .header("content-type", "application/json")
        .json(body);
    let req = apply_auth(req);
    let resp = req.send().await.ok()?;
    let status = resp.status().as_u16();
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
    Some((status, trimmed))
}

/// Classify whether an SSE / network error from `complete_raw` is transient
/// and worth retrying.
///
/// Used by `agent_loop` to wrap a single LLM call in a bounded retry loop:
/// transient errors (TCP RST mid-stream, body decode glitch, idle timeout,
/// stream-ended-early, stale-connection send failure) get a few automatic
/// retries with backoff before the thread is failed. Non-transient errors
/// (e.g. HTTP 4xx/5xx with a captured body indicating a structural
/// rejection) propagate immediately.
///
/// The classifier is intentionally string-matching the user-visible error
/// message — `complete_raw` returns `anyhow::Error`, and the underlying
/// `reqwest_eventsource::Error` and `reqwest::Error` types do not provide a
/// stable enum we can match through `anyhow::Error::downcast_ref` (the
/// errors are wrapped via `anyhow!("SSE stream error: {e}")` which loses
/// the source chain). String matching the well-known transient patterns is
/// adequate and easy to extend.
///
/// ## Diagnostic-suffix awareness
///
/// `fetch_error_body` may have appended `(HTTP <code> body: <body>)` to the
/// error after issuing a one-shot diagnostic POST. The status code carried
/// in that suffix is authoritative:
///
/// - `4xx` / `5xx` → the request is structurally rejected (auth, quota,
///   schema, model-not-supported). **Terminal.**
/// - `2xx` → the diagnostic POST succeeded. The original SSE failure was
///   purely a transport-level glitch (stale connection in pool, NAT idle
///   reset, partial-write etc.). The diag confirms the upstream is fine
///   and a fresh attempt will likely succeed. **Transient.**
/// - `3xx` (rare) → treat as transient; safe re-issue.
///
/// Without the diag suffix, fall back to substring matching against the
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
///
/// ## Terminal patterns
///
/// - `"invalid status code"` (no diag suffix) — pre-stream rejection
///   (e.g. 401 with empty body). Retry would hit the same rejection.
pub fn is_transient_sse_error(err: &anyhow::Error) -> bool {
    let msg = format!("{:#}", err);
    let lower = msg.to_lowercase();

    // If the diagnostic POST captured a status code, trust it.
    if let Some(status) = extract_diag_status(&msg) {
        if (400..600).contains(&status) {
            // Structured rejection — retry won't help.
            return false;
        }
        // 2xx/3xx: diag confirmed upstream is healthy. The original SSE
        // failure must have been a transport blip. Retry.
        return true;
    }

    // No diag suffix — fall back to substring matching.
    if lower.contains("invalid status code") {
        return false;
    }

    matches_transient_pattern(&lower)
}

/// Parse the HTTP status code from the `(HTTP <code> body: ...)` suffix
/// appended by `fetch_error_body`. Returns `None` when the suffix is not
/// present or the code is malformed.
fn extract_diag_status(msg: &str) -> Option<u16> {
    let start = msg.find("(HTTP ")? + "(HTTP ".len();
    let rest = msg.get(start..)?;
    let end = rest.find(' ')?;
    rest.get(..end)?.parse().ok()
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
        let e = err(
            "SSE stream error: error sending request for url \
             (https://api.deepseek.com/chat/completions) \
             (HTTP 200 body: data: {\"id\":\"abc\",\"choices\":[...]})"
        );
        assert!(
            is_transient_sse_error(&e),
            "diag-200 confirms upstream healthy → must be transient"
        );
    }

    #[test]
    fn diag_status_4xx_is_terminal() {
        // Diag captured a structured rejection — retrying won't help.
        let e = err(
            "SSE stream error: Invalid status code: 400 Bad Request \
             (HTTP 400 body: {\"error\":{\"message\":\"bad payload\"}})"
        );
        assert!(
            !is_transient_sse_error(&e),
            "diag-400 is a structured rejection → terminal"
        );
    }

    #[test]
    fn diag_status_5xx_is_terminal() {
        // 503 is a server-side failure but the diag confirms structured
        // upstream behavior. We surface it immediately rather than
        // retrying tight against a known-broken upstream.
        let e = err(
            "SSE stream error: Invalid status code: 503 Service Unavailable \
             (HTTP 503 body: {\"error\":\"upstream down\"})"
        );
        assert!(
            !is_transient_sse_error(&e),
            "diag-5xx is terminal — surface to user, retry policy is not the right hammer"
        );
    }

    #[test]
    fn decode_body_error_no_diag_is_transient() {
        // Pre-this-fix production case: reqwest body decoder glitched
        // mid-stream, diag wasn't issued (already past Event::Open).
        let e = err("SSE stream error: error decoding response body");
        assert!(is_transient_sse_error(&e));
    }

    #[test]
    fn invalid_status_no_diag_is_terminal() {
        // No diag suffix and "Invalid status code" → pre-stream rejection
        // with no recoverable body. Retry would hit the same wall.
        let e = err("SSE error: Invalid status code: 401 Unauthorized");
        assert!(!is_transient_sse_error(&e));
    }

    #[test]
    fn error_sending_request_is_transient() {
        // Stale-connection-from-pool failure. Without a diag suffix it
        // still matches the "error sending request" pattern.
        let e = err(
            "SSE stream error: error sending request for url \
             (https://api.deepseek.com/chat/completions)"
        );
        assert!(is_transient_sse_error(&e));
    }

    #[test]
    fn extract_diag_status_basic() {
        assert_eq!(
            extract_diag_status("foo (HTTP 200 body: bar)"),
            Some(200)
        );
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
}
