//! Native Anthropic Messages API provider.
//!
//! Implements streaming via SSE to the `/messages` endpoint.
//! Supports custom base_url (for proxies) and API key authentication.

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use serde::Serialize;

use crate::provider::{EventStream, Provider};
use crate::types::{ContentBlock, Message, Role, StreamEvent, ToolDefinition};

/// Anthropic Messages API provider.
pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    /// Extra parameters to merge into the API request body.
    params: Option<serde_json::Value>,
    /// Whether the active model accepts image content blocks.
    supports_images: bool,
}

impl AnthropicProvider {
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: Option<&str>,
        params: Option<serde_json::Value>,
        supports_images: bool,
    ) -> Result<Self> {
        // See `openai_compat::OpenAiCompatProvider::new` for the full
        // rationale on connection-pool hygiene. Same defaults are
        // applied here for consistency across providers.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .pool_max_idle_per_host(2)
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key: api_key.map(|s| s.to_string()),
            params,
            supports_images,
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn supports_images(&self) -> bool {
        self.supports_images
    }

    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
    ) -> Result<EventStream> {
        let url = format!("{}/messages", self.base_url);

        // Convert messages to Anthropic format
        let api_messages = messages
            .iter()
            .map(to_anthropic_message)
            .collect::<Vec<_>>();

        // Build tools array
        let api_tools: Vec<AnthropicTool> = tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: sanitize_input_schema(t.input_schema.clone()),
            })
            .collect();

        // Build request body
        let mut body = serde_json::json!({
            "model": &self.model,
            "max_tokens": 16384,
            "stream": true,
            "messages": api_messages,
        });

        if !system.is_empty() {
            body["system"] = serde_json::Value::String(system.to_string());
        }

        if !api_tools.is_empty() {
            body["tools"] = serde_json::to_value(&api_tools)?;
        }

        // Merge extra params from config (provider-level + model-level)
        if let Some(ref params) = self.params
            && let Some(params_obj) = params.as_object()
            && let Some(body_obj) = body.as_object_mut()
        {
            for (k, v) in params_obj {
                body_obj.insert(k.clone(), v.clone());
            }
        }

        // Build request
        let mut req = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01");

        if let Some(ref key) = self.api_key {
            req = req.header("x-api-key", key);
        }

        req = req.json(&body);

        // Capture data needed to diagnose pre-stream HTTP errors (4xx/5xx).
        // EventSource discards the response body; on the first stream error
        // we issue one diagnostic POST with the same body and surface it.
        // Dropped on Event::Open since once the stream is up, mid-stream
        // errors won't have a re-fetchable body.
        let diag_url = url.clone();
        let diag_body = body.clone();
        let diag_api_key = self.api_key.clone();
        let diag_client = self.client.clone();

        // Create SSE stream
        let es =
            EventSource::new(req).map_err(|e| anyhow::anyhow!("SSE connection failed: {e}"))?;

        // Transform SSE events into our StreamEvent type
        let stream = futures::stream::unfold(
            (
                es,
                StreamState::default(),
                Some((diag_client, diag_url, diag_body, diag_api_key)),
            ),
            |(mut es, mut state, mut diag)| async move {
                loop {
                    match es.next().await {
                        Some(Ok(Event::Open)) => {
                            // Keep diag alive for mid-stream error diagnosis.
                            continue;
                        }
                        Some(Ok(Event::Message(msg))) => {
                            match parse_anthropic_sse(&msg.data, &mut state) {
                                Some(events) => {
                                    // Return the first event, buffer the rest
                                    let mut iter = events.into_iter();
                                    if let Some(first) = iter.next() {
                                        state.buffered_events.extend(iter);
                                        return Some((Ok(first), (es, state, diag)));
                                    }
                                    continue;
                                }
                                None => continue,
                            }
                        }
                        Some(Err(e)) => {
                            // Check if we have buffered events to drain first
                            if let Some(event) = state.buffered_events.pop() {
                                return Some((Ok(event), (es, state, diag)));
                            }
                            let err_msg = format!("{e}");
                            if err_msg.contains("Stream ended") {
                                return None;
                            }
                            // First stream error: try to capture the upstream
                            // response body so the caller sees the provider's
                            // actual error (model-not-supported, schema rejection,
                            // rate limit details, etc.).
                            let diagnosed = if let Some((client, url, body, api_key)) = diag.take()
                            {
                                super::fetch_error_body(&client, &url, &body, |req| {
                                    let req = req.header("anthropic-version", "2023-06-01");
                                    if let Some(key) = api_key.as_deref() {
                                        req.header("x-api-key", key)
                                    } else {
                                        req
                                    }
                                })
                                .await
                            } else {
                                None
                            };
                            let final_msg = match diagnosed {
                                Some(diag) => {
                                    format!("SSE error: {e} {}", super::format_diag_suffix(&diag))
                                }
                                None => format!("SSE error: {e}"),
                            };
                            return Some((Err(anyhow::anyhow!(final_msg)), (es, state, diag)));
                        }
                        None => {
                            // Drain buffered events
                            if let Some(event) = state.buffered_events.pop() {
                                return Some((Ok(event), (es, state, diag)));
                            }
                            return None;
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }

    fn format_user_message(&self, blocks: &[ContentBlock]) -> serde_json::Value {
        let content: Vec<serde_json::Value> = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(serde_json::json!({
                    "type": "text",
                    "text": text,
                })),
                ContentBlock::Image { source } => Some(image_block_anthropic(source)),
                // ToolUse / ToolResult are not valid in a user-content array
                // built from the prompt-construction path.
                _ => None,
            })
            .collect();

        serde_json::json!({
            "role": "user",
            "content": content,
        })
    }

    fn format_tool_result(
        &self,
        tool_use_id: &str,
        content: &str,
        is_error: bool,
    ) -> serde_json::Value {
        let mut result = serde_json::json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
            }],
        });
        if is_error {
            result["content"][0]["is_error"] = serde_json::Value::Bool(true);
        }
        result
    }

    fn build_raw_assistant_message(
        &self,
        text: &str,
        _reasoning: &str,
        tool_calls: &[(String, String, String)],
    ) -> serde_json::Value {
        let mut content: Vec<serde_json::Value> = Vec::new();

        if !text.is_empty() {
            content.push(serde_json::json!({"type": "text", "text": text}));
        }

        for (id, name, args) in tool_calls {
            let input: serde_json::Value =
                serde_json::from_str(args).unwrap_or(serde_json::Value::Object(Default::default()));
            content.push(serde_json::json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }

        serde_json::json!({
            "role": "assistant",
            "content": content,
        })
    }

    async fn complete_raw(
        &self,
        raw_messages: &[serde_json::Value],
        tools: &[ToolDefinition],
        system: &str,
    ) -> Result<EventStream> {
        let url = format!("{}/messages", self.base_url);

        let api_tools: Vec<AnthropicTool> = tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: sanitize_input_schema(t.input_schema.clone()),
            })
            .collect();

        let filtered_messages = super::filter_valid_messages(raw_messages);

        let mut body = serde_json::json!({
            "model": &self.model,
            "max_tokens": 16384,
            "stream": true,
            "messages": filtered_messages,
        });

        if !system.is_empty() {
            body["system"] = serde_json::Value::String(system.to_string());
        }

        if !api_tools.is_empty() {
            body["tools"] = serde_json::to_value(&api_tools)?;
        }

        // Merge extra params
        if let Some(ref params) = self.params
            && let Some(params_obj) = params.as_object()
            && let Some(body_obj) = body.as_object_mut()
        {
            for (k, v) in params_obj {
                body_obj.insert(k.clone(), v.clone());
            }
        }

        let mut req = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01");

        if let Some(ref key) = self.api_key {
            req = req.header("x-api-key", key);
        }

        req = req.json(&body);

        // Capture data needed to diagnose pre-stream HTTP errors (4xx/5xx).
        // See `complete()` above for rationale.
        let diag_url = url.clone();
        let diag_body = body.clone();
        let diag_api_key = self.api_key.clone();
        let diag_client = self.client.clone();

        let es =
            EventSource::new(req).map_err(|e| anyhow::anyhow!("SSE connection failed: {e}"))?;

        let stream = futures::stream::unfold(
            (
                es,
                StreamState::default(),
                Some((diag_client, diag_url, diag_body, diag_api_key)),
            ),
            |(mut es, mut state, mut diag)| async move {
                loop {
                    match es.next().await {
                        Some(Ok(Event::Open)) => {
                            // Keep diag alive for mid-stream error diagnosis.
                            continue;
                        }
                        Some(Ok(Event::Message(msg))) => {
                            match parse_anthropic_sse(&msg.data, &mut state) {
                                Some(events) => {
                                    let mut iter = events.into_iter();
                                    if let Some(first) = iter.next() {
                                        state.buffered_events.extend(iter);
                                        return Some((Ok(first), (es, state, diag)));
                                    }
                                    continue;
                                }
                                None => continue,
                            }
                        }
                        Some(Err(e)) => {
                            if let Some(event) = state.buffered_events.pop() {
                                return Some((Ok(event), (es, state, diag)));
                            }
                            let err_msg = format!("{e}");
                            if err_msg.contains("Stream ended") {
                                return None;
                            }
                            let diagnosed = if let Some((client, url, body, api_key)) = diag.take()
                            {
                                super::fetch_error_body(&client, &url, &body, |req| {
                                    let req = req.header("anthropic-version", "2023-06-01");
                                    if let Some(key) = api_key.as_deref() {
                                        req.header("x-api-key", key)
                                    } else {
                                        req
                                    }
                                })
                                .await
                            } else {
                                None
                            };
                            let final_msg = match diagnosed {
                                Some(diag) => {
                                    format!("SSE error: {e} {}", super::format_diag_suffix(&diag))
                                }
                                None => format!("SSE error: {e}"),
                            };
                            return Some((Err(anyhow::anyhow!(final_msg)), (es, state, diag)));
                        }
                        None => {
                            if let Some(event) = state.buffered_events.pop() {
                                return Some((Ok(event), (es, state, diag)));
                            }
                            return None;
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }
}

/// Internal state for parsing the SSE stream.
#[derive(Default)]
struct StreamState {
    current_tool_id: Option<String>,
    current_tool_name: Option<String>,
    tool_input_buffer: String,
    buffered_events: Vec<StreamEvent>,
}

/// Parse a single Anthropic SSE event into StreamEvents.
fn parse_anthropic_sse(data: &str, state: &mut StreamState) -> Option<Vec<StreamEvent>> {
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    let event_type = value.get("type")?.as_str()?;

    match event_type {
        "content_block_start" => {
            let block = value.get("content_block")?;
            let block_type = block.get("type")?.as_str()?;
            if block_type == "tool_use" {
                let id = block.get("id")?.as_str()?.to_string();
                let name = block.get("name")?.as_str()?.to_string();
                state.current_tool_id = Some(id.clone());
                state.current_tool_name = Some(name.clone());
                state.tool_input_buffer.clear();
                return Some(vec![StreamEvent::ToolUseStart { id, name }]);
            }
            None
        }
        "content_block_delta" => {
            let delta = value.get("delta")?;
            let delta_type = delta.get("type")?.as_str()?;
            match delta_type {
                "text_delta" => {
                    let text = delta.get("text")?.as_str()?.to_string();
                    Some(vec![StreamEvent::TextDelta(text)])
                }
                "input_json_delta" => {
                    let partial = delta.get("partial_json")?.as_str()?.to_string();
                    state.tool_input_buffer.push_str(&partial);
                    Some(vec![StreamEvent::ToolInputDelta(partial)])
                }
                _ => None,
            }
        }
        "content_block_stop" => {
            if state.current_tool_id.is_some() {
                state.current_tool_id = None;
                state.current_tool_name = None;
                state.tool_input_buffer.clear();
                return Some(vec![StreamEvent::ToolUseEnd]);
            }
            None
        }
        "message_delta" => {
            // May contain usage info
            if let Some(usage) = value.get("usage") {
                let input = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let output = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if input > 0 || output > 0 {
                    return Some(vec![StreamEvent::Usage {
                        input_tokens: input,
                        output_tokens: output,
                    }]);
                }
            }
            None
        }
        "message_start" => {
            // Extract initial usage
            if let Some(usage) = value.get("message").and_then(|m| m.get("usage")) {
                let input = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let output = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if input > 0 {
                    return Some(vec![StreamEvent::Usage {
                        input_tokens: input,
                        output_tokens: output,
                    }]);
                }
            }
            None
        }
        "message_stop" => Some(vec![StreamEvent::Done]),
        "error" => {
            let error_msg = value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown error")
                .to_string();
            Some(vec![StreamEvent::Error(error_msg)])
        }
        _ => None,
    }
}

/// Convert internal Message to Anthropic API format.
fn to_anthropic_message(msg: &Message) -> serde_json::Value {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user", // Tool results are sent as user messages in Anthropic API
    };

    let content: Vec<serde_json::Value> = msg
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => serde_json::json!({
                "type": "text",
                "text": text,
            }),
            ContentBlock::Image { source } => image_block_anthropic(source),
            ContentBlock::ToolUse { id, name, input } => serde_json::json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let mut result = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                });
                if *is_error {
                    result["is_error"] = serde_json::Value::Bool(true);
                }
                result
            }
        })
        .collect();

    serde_json::json!({
        "role": role,
        "content": content,
    })
}

/// Build an Anthropic `image` content block from an `ImageSource`.
///
/// Anthropic uses different shapes for inline base64 vs remote URL:
/// - Base64: `{"type":"image","source":{"type":"base64","media_type":"image/png","data":"..."}}`
/// - URL:    `{"type":"image","source":{"type":"url","url":"https://..."}}`
fn image_block_anthropic(source: &crate::types::ImageSource) -> serde_json::Value {
    use crate::types::ImageSource;
    match source {
        ImageSource::Base64 { media_type, data } => serde_json::json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            },
        }),
        ImageSource::Url { url } => serde_json::json!({
            "type": "image",
            "source": {
                "type": "url",
                "url": url,
            },
        }),
    }
}

/// Remove `oneOf`/`allOf`/`anyOf` from a JSON Schema object's top level.
///
/// Anthropic Claude Opus 4.6 does not support these JSON Schema
/// composition keywords. External MCP tools may include them in
/// their schema definitions, so we strip them defensively at the
/// provider layer. The tool runtime performs its own parameter
/// validation, so this sanitization is safe.
fn sanitize_input_schema(mut schema: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("oneOf");
        obj.remove("allOf");
        obj.remove("anyOf");
    }
    schema
}

/// Anthropic tool definition format.
#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;
    use futures::StreamExt;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn sanitize_input_schema_removes_one_of() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            },
            "oneOf": [{"required": ["name"]}]
        });
        let result = sanitize_input_schema(schema);
        assert!(result.get("oneOf").is_none(), "oneOf should be removed");
        assert!(result.get("type").is_some(), "type should be preserved");
        assert!(
            result.get("properties").is_some(),
            "properties should be preserved"
        );
    }

    #[test]
    fn sanitize_input_schema_removes_all_of() {
        let schema = json!({
            "type": "object",
            "allOf": [{"type": "object"}]
        });
        let result = sanitize_input_schema(schema);
        assert!(result.get("allOf").is_none(), "allOf should be removed");
        assert!(result.get("type").is_some(), "type should be preserved");
    }

    #[test]
    fn sanitize_input_schema_removes_any_of() {
        let schema = json!({
            "type": "object",
            "anyOf": [{"type": "object"}, {"type": "string"}]
        });
        let result = sanitize_input_schema(schema);
        assert!(result.get("anyOf").is_none(), "anyOf should be removed");
    }

    #[test]
    fn sanitize_input_schema_removes_all_composition_keywords() {
        let schema = json!({
            "type": "object",
            "oneOf": [{"required": ["a"]}],
            "allOf": [{"type": "object"}],
            "anyOf": [{"type": "string"}]
        });
        let result = sanitize_input_schema(schema);
        assert!(result.get("oneOf").is_none());
        assert!(result.get("allOf").is_none());
        assert!(result.get("anyOf").is_none());
    }

    #[test]
    fn sanitize_input_schema_passes_through_non_object() {
        let schema = json!("string_value");
        let result = sanitize_input_schema(schema);
        assert_eq!(result, json!("string_value"));
    }

    #[test]
    fn sanitize_input_schema_passes_through_null() {
        let schema = json!(null);
        let result = sanitize_input_schema(schema);
        assert_eq!(result, json!(null));
    }

    #[test]
    fn sanitize_input_schema_passes_through_clean_object() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            }
        });
        let result = sanitize_input_schema(schema.clone());
        assert_eq!(result, schema, "clean object should pass through unchanged");
    }

    /// When Anthropic rejects the request with 400, the diagnostic POST
    /// must recover the response body and include it in the error message
    /// surfaced to the agent loop.
    ///
    /// This is the exact failure pattern observed in production where the
    /// agent saw `SSE error: Invalid status code: 400 Bad Request` with
    /// no body, hiding the real cause (an unsupported `thinking.type`
    /// param sent via the provider's `params` config merge). With this
    /// fix in place the error becomes:
    ///
    ///   `SSE error: Invalid status code: 400 Bad Request
    ///    (HTTP 400 body: {"type":"error","error":{...}})`
    #[tokio::test]
    async fn complete_error_includes_response_body_on_4xx() {
        let server = MockServer::start().await;

        let error_body = serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "\"thinking.type.enabled\" is not supported for this model. Use \"thinking.type.adaptive\" and \"output_config.effort\" to control thinking behavior."
            }
        });

        Mock::given(method("POST"))
            .and(path("/messages"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(header("x-api-key", "test-key"))
            .respond_with(ResponseTemplate::new(400).set_body_json(&error_body))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(
            &server.uri(),
            "claude-test-model",
            Some("test-key"),
            None,
            false,
        )
        .expect("provider construction");

        let messages = vec![Message::user("hello")];
        let stream = provider
            .complete(&messages, &[], "")
            .await
            .expect("complete() should return a stream — the error surfaces from polling it");

        // Drive the stream until we get the error.
        tokio::pin!(stream);
        let mut found_err: Option<anyhow::Error> = None;
        let mut polls = 0;
        while polls < 16 {
            polls += 1;
            match stream.next().await {
                Some(Err(e)) => {
                    found_err = Some(e);
                    break;
                }
                Some(Ok(_)) => continue,
                None => break,
            }
        }

        let err = found_err.expect("expected an error from the SSE stream after 4xx");
        let msg = format!("{:#}", err);

        assert!(
            msg.contains("400") || msg.contains("Bad Request"),
            "expected status code in error, got: {msg}"
        );
        assert!(
            msg.contains("HTTP 400 body:"),
            "expected captured-body suffix in error, got: {msg}"
        );
        assert!(
            msg.contains("thinking.type.enabled"),
            "expected upstream error message in captured body, got: {msg}"
        );
        assert!(
            msg.contains("invalid_request_error"),
            "expected upstream error type in captured body, got: {msg}"
        );
    }
}
