//! OpenAI-compatible Chat Completions API provider.
//!
//! Supports any endpoint implementing the OpenAI `/chat/completions` API.
//! Covers: DeepSeek, GPT, Groq, Together AI, etc.

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
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
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
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

        // Send request and get streaming response
        let resp = req
            .json(&body)
            .send()
            .await
            .context("Failed to send request to OpenAI-compatible API")?;

        let status = resp.status();
        if !status.is_success() {
            let error_body = resp.text().await.unwrap_or_default();
            tracing::error!(status = %status, body = %error_body, "OpenAI-compatible API error");
            anyhow::bail!("OpenAI-compatible API error ({}): {}", status, error_body);
        }

        // Parse SSE stream from response body
        let byte_stream = resp.bytes_stream();
        let model_name = self.model.clone();
        let stream = futures::stream::unfold(
            (byte_stream, String::new(), OpenAiStreamState::default()),
            move |(mut byte_stream, mut buffer, mut state)| {
                let model_name = model_name.clone();
                async move {
                loop {
                    // Check for buffered events first
                    if let Some(event) = state.pending_events.pop() {
                        return Some((Ok(event), (byte_stream, buffer, state)));
                    }

                    // Read more data
                    match byte_stream.next().await {
                        Some(Ok(chunk)) => {
                            let chunk_str = String::from_utf8_lossy(&chunk);
                            if state.chunks_received == 0 {
                                tracing::debug!(
                                    model = %model_name,
                                    first_chunk_len = chunk_str.len(),
                                    first_chunk_preview = %&chunk_str[..chunk_str.len().min(200)],
                                    "First SSE chunk received"
                                );
                            }
                            state.chunks_received += 1;
                            buffer.push_str(&chunk_str);

                            // Parse SSE lines. Each "data: ..." line is a complete JSON event.
                            let mut lines_parsed = 0;
                            let mut lines_total = 0;
                            while let Some(newline_pos) = buffer.find('\n') {
                                let line = buffer[..newline_pos].to_string();
                                buffer = buffer[newline_pos + 1..].to_string();

                                // Skip empty lines (SSE event separators)
                                let line = line.trim();
                                if line.is_empty() {
                                    continue;
                                }

                                if let Some(data) = line.strip_prefix("data: ") {
                                    lines_total += 1;
                                    if data.trim() == "[DONE]" {
                                        state.pending_events.push(StreamEvent::Done);
                                        continue;
                                    }
                                    if let Some(events) = parse_openai_chunk(data, &mut state) {
                                        state.pending_events.extend(events);
                                        lines_parsed += 1;
                                    }
                                }
                            }

                            if state.chunks_received <= 3 {
                                tracing::debug!(
                                    chunk_num = state.chunks_received,
                                    buffer_len = buffer.len(),
                                    lines_total,
                                    lines_parsed,
                                    pending_events = state.pending_events.len(),
                                    "Chunk processed"
                                );
                            }

                            // Return first pending event if available
                            if let Some(event) = state.pending_events.pop() {
                                return Some((Ok(event), (byte_stream, buffer, state)));
                            }
                        }
                        Some(Err(e)) => {
                            return Some((
                                Err(anyhow::anyhow!("Stream read error: {e}")),
                                (byte_stream, buffer, state),
                            ));
                        }
                        None => {
                            // Stream ended — parse any remaining data in buffer
                            if !buffer.is_empty() {
                                tracing::debug!(
                                    buffer_remaining = buffer.len(),
                                    buffer_preview = %&buffer[..buffer.len().min(300)],
                                    "Stream ended with data in buffer"
                                );
                                for line in buffer.lines() {
                                    if let Some(data) = line.strip_prefix("data: ") {
                                        if data.trim() == "[DONE]" {
                                            state.pending_events.push(StreamEvent::Done);
                                            continue;
                                        }
                                        if let Some(events) = parse_openai_chunk(data, &mut state) {
                                            state.pending_events.extend(events);
                                        }
                                    }
                                }
                                buffer.clear();
                            }
                            // Drain pending events
                            if let Some(event) = state.pending_events.pop() {
                                return Some((Ok(event), (byte_stream, buffer, state)));
                            }
                            if state.chunks_received == 0 {
                                tracing::warn!("OpenAI-compatible stream ended with zero chunks received");
                            }
                            return None;
                        }
                    }
                }
            }},
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
    /// Number of byte chunks received from the stream.
    chunks_received: usize,
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
        // Treat as text for now — the LLM's reasoning is part of its response
        if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
            if !reasoning.is_empty() {
                // Skip reasoning content — it's the model's internal thinking,
                // not the final reply. The actual reply comes in "content".
                // But we still need to consume it to keep the stream flowing.
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

            if !text.is_empty() {
                result["content"] = serde_json::Value::String(text);
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
