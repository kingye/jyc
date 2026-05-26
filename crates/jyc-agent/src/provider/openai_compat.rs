//! OpenAI-compatible Chat Completions API provider.
//!
//! Supports any endpoint implementing the OpenAI `/chat/completions` API.
//! Covers: DeepSeek, GPT, Groq, Together AI, etc.

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use serde_json;

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
}

impl OpenAiCompatProvider {
    pub fn new(base_url: &str, model: &str, api_key: Option<&str>, params: Option<serde_json::Value>) -> Result<Self> {
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
        })
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
            "messages": api_messages,
        });

        if !tools.is_empty() {
            let openai_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                }))
                .collect();
            body["tools"] = serde_json::Value::Array(openai_tools);
        }

        // Merge extra params from config (provider-level + model-level)
        if let Some(ref params) = self.params {
            if let Some(params_obj) = params.as_object() {
                if let Some(body_obj) = body.as_object_mut() {
                    for (k, v) in params_obj {
                        body_obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }

        // Build request
        let mut req = self.client
            .post(&url)
            .header("content-type", "application/json");

        if let Some(ref key) = self.api_key {
            req = req.header("authorization", format!("Bearer {key}"));
        }

        tracing::debug!(url = %url, model = %self.model, "Sending OpenAI-compatible request");

        req = req.json(&body);

        // Use EventSource for proper SSE streaming (same as Anthropic provider)
        let es = EventSource::new(req)
            .map_err(|e| anyhow::anyhow!("SSE connection failed: {e}"))?;

        // Transform SSE events into our StreamEvent type
        let stream = futures::stream::unfold(
            (es, OpenAiStreamState::default()),
            |(mut es, mut state)| async move {
                loop {
                    // Drain buffered events first
                    if let Some(event) = state.pending_events.pop() {
                        return Some((Ok(event), (es, state)));
                    }

                    match es.next().await {
                        Some(Ok(Event::Open)) => continue,
                        Some(Ok(Event::Message(msg))) => {
                            let data = &msg.data;

                            if data.trim() == "[DONE]" {
                                state.pending_events.push(StreamEvent::Done);
                                if let Some(event) = state.pending_events.pop() {
                                    return Some((Ok(event), (es, state)));
                                }
                                return None;
                            }

                            if let Some(events) = parse_openai_chunk(data, &mut state) {
                                state.pending_events.extend(events);
                            }

                            if let Some(event) = state.pending_events.pop() {
                                return Some((Ok(event), (es, state)));
                            }
                            // No events from this chunk (e.g., reasoning_content only), continue
                        }
                        Some(Err(e)) => {
                            let err_msg = format!("{e}");
                            if err_msg.contains("Stream ended") {
                                // Drain remaining events
                                if let Some(event) = state.pending_events.pop() {
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
                            if let Some(event) = state.pending_events.pop() {
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

    fn format_user_message(&self, text: &str) -> serde_json::Value {
        serde_json::json!({
            "role": "user",
            "content": text,
        })
    }

    fn format_tool_result(&self, tool_call_id: &str, content: &str, _is_error: bool) -> serde_json::Value {
        serde_json::json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": content,
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
            let tc_json: Vec<serde_json::Value> = tool_calls.iter().map(|(id, name, args)| {
                serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": args,
                    }
                })
            }).collect();
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
            "messages": api_messages,
        });

        if !tools.is_empty() {
            let openai_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                }))
                .collect();
            body["tools"] = serde_json::Value::Array(openai_tools);
        }

        // Merge extra params
        if let Some(ref params) = self.params {
            if let Some(params_obj) = params.as_object() {
                if let Some(body_obj) = body.as_object_mut() {
                    for (k, v) in params_obj {
                        body_obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }

        // Build and send request
        let mut req = self.client
            .post(&url)
            .header("content-type", "application/json");

        if let Some(ref key) = self.api_key {
            req = req.header("authorization", format!("Bearer {key}"));
        }

        tracing::debug!(url = %url, model = %self.model, "Sending OpenAI-compatible request");

        req = req.json(&body);

        // Capture data needed to diagnose 4xx/5xx after the SSE source fails.
        // EventSource's error string is just "Invalid status code: 400 Bad Request"
        // and discards the response body. On the first stream error we issue one
        // diagnostic POST with the same body and surface the body in the error.
        let diag_url = url.clone();
        let diag_body = body.clone();
        let diag_api_key = self.api_key.clone();
        let diag_client = self.client.clone();

        let es = EventSource::new(req)
            .map_err(|e| anyhow::anyhow!("SSE connection failed: {e}"))?;

        let stream = futures::stream::unfold(
            (es, OpenAiStreamState::default(), Some((diag_client, diag_url, diag_body, diag_api_key))),
            |(mut es, mut state, mut diag)| async move {
                loop {
                    if let Some(event) = state.pending_events.pop() {
                        return Some((Ok(event), (es, state, diag)));
                    }

                    match es.next().await {
                        Some(Ok(Event::Open)) => {
                            // Connection succeeded; we no longer need diagnostic
                            // capability. Drop the cloned data to free memory.
                            diag = None;
                            continue;
                        }
                        Some(Ok(Event::Message(msg))) => {
                            let data = &msg.data;
                            if data.trim() == "[DONE]" {
                                state.pending_events.push(StreamEvent::Done);
                                if let Some(event) = state.pending_events.pop() {
                                    return Some((Ok(event), (es, state, diag)));
                                }
                                return None;
                            }
                            if let Some(events) = parse_openai_chunk(data, &mut state) {
                                state.pending_events.extend(events);
                            }
                            if let Some(event) = state.pending_events.pop() {
                                return Some((Ok(event), (es, state, diag)));
                            }
                        }
                        Some(Err(e)) => {
                            let err_msg = format!("{e}");
                            if err_msg.contains("Stream ended") {
                                if let Some(event) = state.pending_events.pop() {
                                    return Some((Ok(event), (es, state, diag)));
                                }
                                return None;
                            }
                            // First stream error: try to capture the HTTP body via
                            // a diagnostic POST with the same body so the caller
                            // sees the provider's actual error (validation message,
                            // rate limit reason, etc.).
                            let diagnosed = if let Some((client, url, body, api_key)) = diag.take() {
                                super::fetch_error_body(&client, &url, &body, |req| {
                                    if let Some(key) = api_key.as_deref() {
                                        req.header("authorization", format!("Bearer {key}"))
                                    } else {
                                        req
                                    }
                                })
                                .await
                            } else {
                                None
                            };
                            let final_msg = match diagnosed {
                                Some((status, body)) => format!(
                                    "SSE stream error: {e} (HTTP {status} body: {body})"
                                ),
                                None => format!("SSE stream error: {e}"),
                            };
                            return Some((
                                Err(anyhow::anyhow!(final_msg)),
                                (es, state, diag),
                            ));
                        }
                        None => {
                            if let Some(event) = state.pending_events.pop() {
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
    /// Events ready to be yielded.
    pending_events: Vec<StreamEvent>,
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

    let choices = match value.get("choices").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => return None,
    };

    let mut events = Vec::new();

    for choice in choices {
        let delta = match choice.get("delta") {
            Some(d) => d,
            None => continue, // skip choices without delta instead of returning None
        };

        // Text content (standard OpenAI field)
        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                events.push(StreamEvent::TextDelta(content.to_string()));
            }
        }

        // Reasoning content (DeepSeek v4-pro style thinking)
        if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
            if !reasoning.is_empty() {
                events.push(StreamEvent::ReasoningDelta(reasoning.to_string()));
            }
        }

        // Check finish_reason and extract usage from the same chunk
        if let Some(finish_reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            if finish_reason == "tool_calls" || finish_reason == "stop" {
                // Emit ToolUseEnd for each accumulated tool call
                for _ in &state.tool_calls {
                    events.push(StreamEvent::ToolUseEnd);
                }
                state.tool_calls.clear();
            }
        }

        // Tool calls
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
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
                    if let Some(args) = function.get("arguments").and_then(|a| a.as_str()) {
                        acc.arguments.push_str(args);
                        events.push(StreamEvent::ToolInputDelta(args.to_string()));
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
        let input = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        let output = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
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
        Role::User => {
            let content = msg.text();
            serde_json::json!({
                "role": "user",
                "content": content,
            })
        }
        Role::Assistant => {
            let mut result = serde_json::json!({ "role": "assistant" });

            let text = msg.text();
            let tool_uses: Vec<_> = msg.content.iter().filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => Some(serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": input.to_string(),
                    }
                })),
                _ => None,
            }).collect();

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
            if let Some(ContentBlock::ToolResult { tool_use_id, content, .. }) = msg.content.first() {
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
