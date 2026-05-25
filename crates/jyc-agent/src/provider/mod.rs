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

    /// Send messages and get a streaming response.
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
    ) -> Result<EventStream>;

    /// Format a user message as raw provider JSON (for context persistence).
    fn format_user_message(&self, text: &str) -> serde_json::Value;

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

    match config.provider_type.as_str() {
        "anthropic" => {
            let base_url = config.base_url.as_deref()
                .unwrap_or("https://api.anthropic.com/v1");
            Ok(Box::new(anthropic::AnthropicProvider::new(
                base_url,
                model_id,
                api_key.as_deref(),
                params,
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
/// stream-ended-early) get a few automatic retries with backoff before the
/// thread is failed. Non-transient errors (e.g. HTTP 4xx with a captured
/// body) propagate immediately.
///
/// The classifier is intentionally string-matching the user-visible error
/// message — `complete_raw` returns `anyhow::Error`, and the underlying
/// `reqwest_eventsource::Error` and `reqwest::Error` types do not provide a
/// stable enum we can match through `anyhow::Error::downcast_ref` (the
/// errors are wrapped via `anyhow!("SSE stream error: {e}")` which loses
/// the source chain). String matching the well-known transient patterns is
/// adequate and easy to extend.
///
/// Treated as TRANSIENT (retryable):
/// - "error decoding response body" — reqwest's body decoder hit a
///   chunked-encoding glitch, malformed UTF-8, or premature EOF. Almost
///   always a network/provider blip.
/// - "Stream ended" / "stream ended" — provider closed the SSE before
///   `[DONE]`. Treated as transient mid-flight failure.
/// - "connection reset" / "connection closed" / "broken pipe" — TCP-level
///   transport interruption.
/// - "operation timed out" / "request timed out" — the 300s reqwest
///   timeout fired or an SSE idle-read timed out. Worth one fresh attempt.
/// - "dns error" / "tcp connect error" — pre-connection failures during a
///   retry wave; transient by nature.
///
/// Treated as TERMINAL (NOT retryable):
/// - Anything containing `"HTTP "` — the diagnostic-body capture path
///   in `openai_compat::complete_raw` only injects an `(HTTP {status} body:
///   {body})` suffix when the provider returned a real status code, which
///   means the request was structurally rejected (auth / quota / bad
///   payload). Retrying won't help.
/// - "Invalid status code" without an HTTP body suffix — usually a
///   pre-stream rejection (e.g. 401 / 429 with empty body). The retry
///   would just hit the same rejection. Surface immediately.
pub fn is_transient_sse_error(err: &anyhow::Error) -> bool {
    let msg = format!("{:#}", err);
    let lower = msg.to_lowercase();

    // Strong terminal signal: a captured HTTP status+body means the
    // provider answered with a structured error. Don't retry.
    if msg.contains("HTTP ") && msg.contains("body:") {
        return false;
    }
    // A bare "Invalid status code" (no captured body) is also a
    // structured rejection at connection time — non-transient.
    if lower.contains("invalid status code") {
        return false;
    }

    const TRANSIENT_PATTERNS: &[&str] = &[
        "error decoding response body",
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
    TRANSIENT_PATTERNS.iter().any(|p| lower.contains(p))
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
