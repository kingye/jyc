//! OpenAI-compatible Chat Completions API provider.
//!
//! Supports any endpoint implementing the OpenAI `/chat/completions` API.
//! Covers: DeepSeek, GPT, Groq, Together AI, etc.

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use serde_json;
use std::collections::VecDeque;

use crate::provider::{EventStream, Provider};
use crate::types::{ContentBlock, Message, Role, StreamEvent, ToolDefinition};

/// OpenAI-compatible provider.
pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    /// Extra parameters to merge into the API request body.
    params: Option<serde_json::Value>,
    /// Whether the active model accepts image content blocks.
    supports_images: bool,
    /// Optional User-Agent header override.
    user_agent: Option<String>,
}

impl OpenAiCompatProvider {
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: Option<&str>,
        params: Option<serde_json::Value>,
        supports_images: bool,
        user_agent: Option<&str>,
    ) -> Result<Self> {
        // Connection pool hygiene:
        //
        // - `pool_idle_timeout(30s)` ensures we never reuse a connection
        //   that has been idle longer than the typical NAT/load-balancer
        //   silent-drop window. Reqwest's default is 90s, which is large
        //   enough for the peer to forget the connection while we still
        //   think it's healthy — that manifests as
        //   `error sending request for url (...)` on the next use, even
        //   though a fresh diagnostic POST against the same URL succeeds.
        //   Observed in production on bare-metal where DeepSeek SSE calls
        //   intermittently failed despite the upstream being healthy.
        //
        // - `pool_max_idle_per_host(2)` bounds how many warm connections
        //   we keep around per provider. JYC issues at most a handful of
        //   concurrent requests per provider so 2 is a comfortable cap;
        //   prevents unbounded pool growth under bursts.
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
            user_agent: user_agent.map(|s| s.to_string()),
        })
    }

    /// Apply common headers (authorization, user-agent) to a request builder.
    fn apply_headers(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut req = req;
        if let Some(ref key) = self.api_key {
            req = req.header("authorization", format!("Bearer {key}"));
        }
        if let Some(ref ua) = self.user_agent {
            req = req.header("user-agent", ua.as_str());
        }
        req
    }
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        "openai-compatible"
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
        let url = format!("{}/chat/completions", self.base_url);

        // Build messages array (prepend system message)
        let mut api_messages: Vec<serde_json::Value> = Vec::new();

        if !system.is_empty() {
            api_messages.push(serde_json::json!({
                "role": "system",
                "content": system,
            }));
        }

        for msg in messages {
            api_messages.push(to_openai_message(msg));
        }

        // Build request body
        let mut body = serde_json::json!({
            "model": &self.model,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": api_messages,
        });

        if !tools.is_empty() {
            let openai_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(openai_tools);
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
        let req = self
            .client
            .post(&url)
            .header("content-type", "application/json");
        let req = self.apply_headers(req).json(&body);

        tracing::debug!(url = %url, model = %self.model, "Sending OpenAI-compatible request");

        // Use EventSource for proper SSE streaming (same as Anthropic provider)
        let es =
            EventSource::new(req).map_err(|e| anyhow::anyhow!("SSE connection failed: {e}"))?;

        // Transform SSE events into our StreamEvent type
        let stream = futures::stream::unfold(
            (es, OpenAiStreamState::default()),
            |(mut es, mut state)| async move {
                loop {
                    // Drain buffered events first (FIFO)
                    if let Some(event) = state.pending_events.pop_front() {
                        return Some((Ok(event), (es, state)));
                    }

                    match es.next().await {
                        Some(Ok(Event::Open)) => continue,
                        Some(Ok(Event::Message(msg))) => {
                            let data = &msg.data;

                            if data.trim() == "[DONE]" {
                                state.pending_events.push_back(StreamEvent::Done);
                                if let Some(event) = state.pending_events.pop_front() {
                                    return Some((Ok(event), (es, state)));
                                }
                                return None;
                            }

                            if let Some(events) = parse_openai_chunk(data, &mut state) {
                                state.pending_events.extend(events);
                            }

                            if let Some(event) = state.pending_events.pop_front() {
                                return Some((Ok(event), (es, state)));
                            }
                            // No events from this chunk (e.g., reasoning_content only), continue
                        }
                        Some(Err(e)) => {
                            let err_msg = format!("{e}");
                            if err_msg.contains("Stream ended") {
                                // Drain remaining events
                                if let Some(event) = state.pending_events.pop_front() {
                                    return Some((Ok(event), (es, state)));
                                }
                                return None;
                            }
                            return Some((
                                Err(anyhow::anyhow!("SSE stream error: {e}")),
                                (es, state),
                            ));
                        }
                        None => {
                            // Drain remaining events
                            if let Some(event) = state.pending_events.pop_front() {
                                return Some((Ok(event), (es, state)));
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
        build_openai_user_content(blocks)
    }

    fn format_tool_result(
        &self,
        tool_call_id: &str,
        content: &str,
        is_error: bool,
    ) -> serde_json::Value {
        let labeled = if is_error {
            format!("[ERROR] {content}")
        } else {
            format!("[SUCCESS] {content}")
        };
        serde_json::json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": labeled,
        })
    }

    fn build_raw_assistant_message(
        &self,
        text: &str,
        reasoning: &str,
        tool_calls: &[(String, String, String)],
    ) -> serde_json::Value {
        let mut msg = serde_json::json!({ "role": "assistant" });

        // Content
        if !text.is_empty() {
            msg["content"] = serde_json::Value::String(text.to_string());
        } else {
            msg["content"] = serde_json::Value::Null;
        }

        // Reasoning content (DeepSeek v4-pro)
        if !reasoning.is_empty() {
            msg["reasoning_content"] = serde_json::Value::String(reasoning.to_string());
        }

        // Tool calls
        if !tool_calls.is_empty() {
            let tc_json: Vec<serde_json::Value> = tool_calls
                .iter()
                .map(|(id, name, args)| {
                    serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": args,
                        }
                    })
                })
                .collect();
            msg["tool_calls"] = serde_json::Value::Array(tc_json);
        }

        msg
    }

    async fn complete_raw(
        &self,
        raw_messages: &[serde_json::Value],
        tools: &[ToolDefinition],
        system: &str,
    ) -> Result<EventStream> {
        let url = format!("{}/chat/completions", self.base_url);

        // Build messages array: system + raw messages (filtered)
        let mut api_messages: Vec<serde_json::Value> = Vec::new();
        if !system.is_empty() {
            api_messages.push(serde_json::json!({
                "role": "system",
                "content": system,
            }));
        }
        api_messages.extend(super::filter_valid_messages(raw_messages));

        // Build request body
        let mut body = serde_json::json!({
            "model": &self.model,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": api_messages,
        });

        if !tools.is_empty() {
            let openai_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(openai_tools);
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

        // Build and send request
        let req = self
            .client
            .post(&url)
            .header("content-type", "application/json");
        let req = self.apply_headers(req).json(&body);

        tracing::debug!(url = %url, model = %self.model, "Sending OpenAI-compatible request");

        // Capture data needed to diagnose 4xx/5xx after the SSE source fails.
        // EventSource's error string is just "Invalid status code: 400 Bad Request"
        // and discards the response body. On the first stream error we issue one
        // diagnostic POST with the same body and surface the body in the error.
        let diag_url = url.clone();
        let diag_body = body.clone();
        let diag_api_key = self.api_key.clone();
        let diag_user_agent = self.user_agent.clone();
        let diag_client = self.client.clone();

        let es =
            EventSource::new(req).map_err(|e| anyhow::anyhow!("SSE connection failed: {e}"))?;

        let stream = futures::stream::unfold(
            (
                es,
                OpenAiStreamState::default(),
                Some((
                    diag_client,
                    diag_url,
                    diag_body,
                    diag_api_key,
                    diag_user_agent,
                )),
            ),
            |(mut es, mut state, mut diag)| async move {
                loop {
                    if let Some(event) = state.pending_events.pop_front() {
                        return Some((Ok(event), (es, state, diag)));
                    }

                    match es.next().await {
                        Some(Ok(Event::Open)) => {
                            // Keep diag alive — mid-stream errors (body decode
                            // failure, hung connection, TCP reset) still benefit
                            // from a diagnostic POST to capture the server's
                            // error body.
                            continue;
                        }
                        Some(Ok(Event::Message(msg))) => {
                            let data = &msg.data;
                            if data.trim() == "[DONE]" {
                                state.pending_events.push_back(StreamEvent::Done);
                                if let Some(event) = state.pending_events.pop_front() {
                                    return Some((Ok(event), (es, state, diag)));
                                }
                                return None;
                            }
                            if let Some(events) = parse_openai_chunk(data, &mut state) {
                                state.pending_events.extend(events);
                            }
                            if let Some(event) = state.pending_events.pop_front() {
                                return Some((Ok(event), (es, state, diag)));
                            }
                        }
                        Some(Err(e)) => {
                            let err_msg = format!("{e}");
                            if err_msg.contains("Stream ended") {
                                if let Some(event) = state.pending_events.pop_front() {
                                    return Some((Ok(event), (es, state, diag)));
                                }
                                return None;
                            }
                            // First stream error: try to capture the HTTP body via
                            // a diagnostic POST with the same body so the caller
                            // sees the provider's actual error (validation message,
                            // rate limit reason, etc.).
                            let diagnosed = if let Some((client, url, body, api_key, user_agent)) =
                                diag.take()
                            {
                                super::fetch_error_body(&client, &url, &body, |req| {
                                    let mut req = req;
                                    if let Some(key) = api_key.as_deref() {
                                        req = req.header("authorization", format!("Bearer {key}"));
                                    }
                                    if let Some(ua) = user_agent.as_deref() {
                                        req = req.header("user-agent", ua);
                                    }
                                    req
                                })
                                .await
                            } else {
                                None
                            };
                            let final_msg = match diagnosed {
                                Some(diag) => {
                                    format!(
                                        "SSE stream error: {e} {}",
                                        super::format_diag_suffix(&diag)
                                    )
                                }
                                None => format!("SSE stream error: {e}"),
                            };
                            return Some((Err(anyhow::anyhow!(final_msg)), (es, state, diag)));
                        }
                        None => {
                            if let Some(event) = state.pending_events.pop_front() {
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

/// Internal state for OpenAI stream parsing.
#[derive(Default)]
struct OpenAiStreamState {
    /// Tool calls being assembled from deltas.
    tool_calls: Vec<ToolCallAccumulator>,
    /// Events ready to be yielded (FIFO queue).
    pending_events: VecDeque<StreamEvent>,
}

#[derive(Default, Clone)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

/// Parse a single OpenAI SSE chunk into StreamEvents.
fn parse_openai_chunk(data: &str, state: &mut OpenAiStreamState) -> Option<Vec<StreamEvent>> {
    let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::trace!(error = %e, data_preview = %&data[..data.len().min(100)], "Failed to parse SSE chunk JSON");
            return None;
        }
    };

    let choices = value.get("choices").and_then(|c| c.as_array())?;

    let mut events = Vec::new();

    for choice in choices {
        let delta = match choice.get("delta") {
            Some(d) => d,
            None => continue, // skip choices without delta instead of returning None
        };

        // Text content (standard OpenAI field)
        if let Some(content) = delta.get("content").and_then(|c| c.as_str())
            && !content.is_empty()
        {
            events.push(StreamEvent::TextDelta(content.to_string()));
        }

        // Reasoning content (DeepSeek v4-pro style thinking)
        if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str())
            && !reasoning.is_empty()
        {
            events.push(StreamEvent::ReasoningDelta(reasoning.to_string()));
        }

        // Check finish_reason and extract usage from the same chunk
        if let Some(finish_reason) = choice.get("finish_reason").and_then(|f| f.as_str())
            && (finish_reason == "tool_calls" || finish_reason == "stop")
        {
            // Emit ToolUseEnd for each accumulated tool call
            for _ in &state.tool_calls {
                events.push(StreamEvent::ToolUseEnd);
            }
            state.tool_calls.clear();
        }

        // Tool calls
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            tracing::trace!(
                tool_calls_json = %serde_json::to_string(tool_calls).unwrap_or_default(),
                "SSE chunk contains tool_calls"
            );
            for tc in tool_calls {
                let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;

                // Ensure accumulator exists
                while state.tool_calls.len() <= index {
                    state.tool_calls.push(ToolCallAccumulator::default());
                }

                let acc = &mut state.tool_calls[index];

                // ID (first chunk only)
                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                    acc.id = id.to_string();
                }

                // Function name and arguments
                if let Some(function) = tc.get("function") {
                    if let Some(name) = function.get("name").and_then(|n| n.as_str()) {
                        acc.name = name.to_string();
                    }
                    // Arguments: standard OpenAI sends a string (possibly
                    // chunked across deltas); some providers (e.g., GLM-5.2
                    // via Ark) send a JSON object directly. Handle both.
                    if let Some(args_val) = function.get("arguments") {
                        match args_val {
                            serde_json::Value::String(s) => {
                                acc.arguments.push_str(s);
                                events.push(StreamEvent::ToolInputDelta(s.to_string()));
                            }
                            serde_json::Value::Object(_) => {
                                let serialized =
                                    serde_json::to_string(args_val).unwrap_or_default();
                                acc.arguments.push_str(&serialized);
                                events.push(StreamEvent::ToolInputDelta(serialized));
                            }
                            _ => {
                                tracing::trace!(
                                    args_value = %args_val,
                                    "Unexpected arguments type in tool_calls delta"
                                );
                            }
                        }
                    }
                }

                // Emit ToolUseStart on first chunk with name
                if !acc.started && !acc.name.is_empty() && !acc.id.is_empty() {
                    acc.started = true;
                    events.insert(
                        events.len().saturating_sub(1), // Insert before the delta
                        StreamEvent::ToolUseStart {
                            id: acc.id.clone(),
                            name: acc.name.clone(),
                        },
                    );
                }
            }
        }
    }

    // Usage info (some providers include it in stream)
    if let Some(usage) = value.get("usage") {
        let input = usage
            .get("prompt_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let output = usage
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if input > 0 || output > 0 {
            events.push(StreamEvent::Usage {
                input_tokens: input,
                output_tokens: output,
            });
        }
    }

    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

/// Convert internal Message to OpenAI API format.
fn to_openai_message(msg: &Message) -> serde_json::Value {
    match msg.role {
        Role::User => build_openai_user_content(&msg.content),
        Role::Assistant => {
            let mut result = serde_json::json!({ "role": "assistant" });

            let text = msg.text();
            let tool_uses: Vec<_> = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => Some(serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": input.to_string(),
                        }
                    })),
                    _ => None,
                })
                .collect();

            // Always set content (some APIs require it even when tool_calls are present)
            if !text.is_empty() {
                result["content"] = serde_json::Value::String(text);
            } else {
                result["content"] = serde_json::Value::Null;
            }

            if !tool_uses.is_empty() {
                result["tool_calls"] = serde_json::Value::Array(tool_uses);
            }

            result
        }
        Role::Tool => {
            // Tool results in OpenAI format
            if let Some(ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            }) = msg.content.first()
            {
                serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                })
            } else {
                serde_json::json!({
                    "role": "user",
                    "content": msg.text(),
                })
            }
        }
    }
}

/// Build an OpenAI-compatible user message from content blocks.
///
/// When the message contains only text, emits the legacy string-content form
/// (`"content": "..."`). When images are present, emits the array-content form
/// (`"content": [{"type":"text",...}, {"type":"image_url",...}]`).
///
/// Why the dual form: many OpenAI-compatible servers (especially older ones)
/// reject array content for purely textual user messages, so we keep the
/// minimal-friction string form for the common case and only escalate to the
/// array form when actually needed for multimodal input.
fn build_openai_user_content(content: &[ContentBlock]) -> serde_json::Value {
    let has_image = content
        .iter()
        .any(|b| matches!(b, ContentBlock::Image { .. }));

    if !has_image {
        // Legacy string-content form
        let text = content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        return serde_json::json!({
            "role": "user",
            "content": text,
        });
    }

    // Array-content form (multimodal)
    let parts: Vec<serde_json::Value> = content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(serde_json::json!({
                "type": "text",
                "text": text,
            })),
            ContentBlock::Image { source } => Some(image_block_openai(source)),
            _ => None,
        })
        .collect();

    serde_json::json!({
        "role": "user",
        "content": parts,
    })
}

/// Build an OpenAI-compatible `image_url` content part from an `ImageSource`.
///
/// Both base64 and remote URL share the same `image_url.url` field; base64
/// is encoded as a `data:` URL.
fn image_block_openai(source: &crate::types::ImageSource) -> serde_json::Value {
    use crate::types::ImageSource;
    let url = match source {
        ImageSource::Base64 { media_type, data } => {
            format!("data:{media_type};base64,{data}")
        }
        ImageSource::Url { url } => url.clone(),
    };
    serde_json::json!({
        "type": "image_url",
        "image_url": { "url": url },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;
    use futures::StreamExt;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn complete_sends_custom_user_agent() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("content-type", "application/json"))
            .and(header("authorization", "Bearer test-key"))
            .and(header("user-agent", "opencode/1.15.13"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let provider = OpenAiCompatProvider::new(
            &server.uri(),
            "test-model",
            Some("test-key"),
            None,
            false,
            Some("opencode/1.15.13"),
        )
        .expect("provider construction");

        let messages = vec![Message::user("hello")];
        let mut stream = provider
            .complete(&messages, &[], "")
            .await
            .expect("complete should return a stream");

        // Drive the stream to completion (the mock returns 204, so it will end quickly).
        while stream.next().await.is_some() {}

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].headers.get("user-agent").unwrap(),
            "opencode/1.15.13"
        );
    }

    #[tokio::test]
    async fn diagnostic_post_sends_custom_user_agent_on_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("user-agent", "opencode/1.15.13"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": { "message": "bad request", "type": "invalid_request_error" }
            })))
            .expect(2)
            .mount(&server)
            .await;

        let provider = OpenAiCompatProvider::new(
            &server.uri(),
            "test-model",
            Some("test-key"),
            None,
            false,
            Some("opencode/1.15.13"),
        )
        .expect("provider construction");

        let raw_messages = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let mut stream = provider
            .complete_raw(&raw_messages, &[], "")
            .await
            .expect("complete_raw should return a stream");

        while stream.next().await.is_some() {}

        // wiremock will panic at drop if the request count expectation is not met.
    }

    #[test]
    fn parse_tool_call_string_arguments() {
        let mut state = OpenAiStreamState::default();
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"command\":\"ls\"}"
                        }
                    }]
                }
            }]
        })
        .to_string();
        let events = parse_openai_chunk(&chunk, &mut state).expect("should parse");

        assert!(!events.is_empty());
        assert_eq!(state.tool_calls.len(), 1);
        assert_eq!(state.tool_calls[0].name, "bash");
        assert_eq!(state.tool_calls[0].arguments, r#"{"command":"ls"}"#);
    }

    #[test]
    fn parse_tool_call_object_arguments() {
        // GLM-5.2 via Ark sends arguments as a JSON object, not a string.
        let mut state = OpenAiStreamState::default();
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {
                            "name": "bash",
                            "arguments": {"command": "ls"}
                        }
                    }]
                }
            }]
        })
        .to_string();
        let events = parse_openai_chunk(&chunk, &mut state).expect("should parse");

        assert!(!events.is_empty());
        assert_eq!(state.tool_calls.len(), 1);
        assert_eq!(state.tool_calls[0].name, "bash");
        assert_eq!(state.tool_calls[0].arguments, r#"{"command":"ls"}"#);
    }

    #[test]
    fn parse_tool_call_chunked_string_arguments() {
        // Standard OpenAI streaming: arguments arrive as string fragments
        // across multiple SSE chunks and must be accumulated.
        let mut state = OpenAiStreamState::default();

        let chunk1 = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"comm"
                        }
                    }]
                }
            }]
        })
        .to_string();
        parse_openai_chunk(&chunk1, &mut state);

        let chunk2 = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {
                            "arguments": "and\":\"ls\"}"
                        }
                    }]
                }
            }]
        })
        .to_string();
        parse_openai_chunk(&chunk2, &mut state);

        assert_eq!(state.tool_calls.len(), 1);
        assert_eq!(state.tool_calls[0].arguments, r#"{"command":"ls"}"#);
    }

    #[test]
    fn format_tool_result_prefixed_with_success() {
        let provider = OpenAiCompatProvider::new(
            "https://api.example.com",
            "test",
            Some("k"),
            None,
            false,
            None,
        )
        .unwrap();
        let result =
            provider.format_tool_result("id1", "Command completed with exit code 0", false);
        let content = result["content"].as_str().unwrap();
        assert!(
            content.starts_with("[SUCCESS] "),
            "expected [SUCCESS] prefix, got: {content}"
        );
        assert!(content.contains("Command completed with exit code 0"));
    }

    #[test]
    fn format_tool_result_prefixed_with_error() {
        let provider = OpenAiCompatProvider::new(
            "https://api.example.com",
            "test",
            Some("k"),
            None,
            false,
            None,
        )
        .unwrap();
        let result = provider.format_tool_result("id1", "something failed", true);
        let content = result["content"].as_str().unwrap();
        assert!(
            content.starts_with("[ERROR] "),
            "expected [ERROR] prefix, got: {content}"
        );
        assert!(content.contains("something failed"));
    }
}
