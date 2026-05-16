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
}

impl AnthropicProvider {
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
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
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
        let url = format!("{}/messages", self.base_url);

        // Convert messages to Anthropic format
        let api_messages = messages
            .iter()
            .map(|msg| to_anthropic_message(msg))
            .collect::<Vec<_>>();

        // Build tools array
        let api_tools: Vec<AnthropicTool> = tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
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
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01");

        if let Some(ref key) = self.api_key {
            req = req.header("x-api-key", key);
        }

        req = req.json(&body);

        // Create SSE stream
        let es = EventSource::new(req)
            .map_err(|e| anyhow::anyhow!("SSE connection failed: {e}"))?;

        // Transform SSE events into our StreamEvent type
        let stream = futures::stream::unfold(
            (es, StreamState::default()),
            |(mut es, mut state)| async move {
                loop {
                    match es.next().await {
                        Some(Ok(Event::Open)) => continue,
                        Some(Ok(Event::Message(msg))) => {
                            match parse_anthropic_sse(&msg.data, &mut state) {
                                Some(events) => {
                                    // Return the first event, buffer the rest
                                    let mut iter = events.into_iter();
                                    if let Some(first) = iter.next() {
                                        state.buffered_events.extend(iter);
                                        return Some((Ok(first), (es, state)));
                                    }
                                    continue;
                                }
                                None => continue,
                            }
                        }
                        Some(Err(e)) => {
                            // Check if we have buffered events to drain first
                            if let Some(event) = state.buffered_events.pop() {
                                return Some((Ok(event), (es, state)));
                            }
                            let err_msg = format!("{e}");
                            if err_msg.contains("Stream ended") {
                                return None;
                            }
                            return Some((Err(anyhow::anyhow!("SSE error: {e}")), (es, state)));
                        }
                        None => {
                            // Drain buffered events
                            if let Some(event) = state.buffered_events.pop() {
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
            "content": [{"type": "text", "text": text}],
        })
    }

    fn format_tool_result(&self, tool_use_id: &str, content: &str, is_error: bool) -> serde_json::Value {
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
            let input: serde_json::Value = serde_json::from_str(args)
                .unwrap_or(serde_json::Value::Object(Default::default()));
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
                input_schema: t.input_schema.clone(),
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
        if let Some(ref params) = self.params {
            if let Some(params_obj) = params.as_object() {
                if let Some(body_obj) = body.as_object_mut() {
                    for (k, v) in params_obj {
                        body_obj.insert(k.clone(), v.clone());
                    }
                }
            }
        }

        let mut req = self.client
            .post(&url)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01");

        if let Some(ref key) = self.api_key {
            req = req.header("x-api-key", key);
        }

        req = req.json(&body);

        let es = EventSource::new(req)
            .map_err(|e| anyhow::anyhow!("SSE connection failed: {e}"))?;

        let stream = futures::stream::unfold(
            (es, StreamState::default()),
            |(mut es, mut state)| async move {
                loop {
                    match es.next().await {
                        Some(Ok(Event::Open)) => continue,
                        Some(Ok(Event::Message(msg))) => {
                            match parse_anthropic_sse(&msg.data, &mut state) {
                                Some(events) => {
                                    let mut iter = events.into_iter();
                                    if let Some(first) = iter.next() {
                                        state.buffered_events.extend(iter);
                                        return Some((Ok(first), (es, state)));
                                    }
                                    continue;
                                }
                                None => continue,
                            }
                        }
                        Some(Err(e)) => {
                            if let Some(event) = state.buffered_events.pop() {
                                return Some((Ok(event), (es, state)));
                            }
                            let err_msg = format!("{e}");
                            if err_msg.contains("Stream ended") {
                                return None;
                            }
                            return Some((Err(anyhow::anyhow!("SSE error: {e}")), (es, state)));
                        }
                        None => {
                            if let Some(event) = state.buffered_events.pop() {
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
                let input = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let output = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
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
                let input = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let output = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                if input > 0 {
                    return Some(vec![StreamEvent::Usage {
                        input_tokens: input,
                        output_tokens: output,
                    }]);
                }
            }
            None
        }
        "message_stop" => {
            Some(vec![StreamEvent::Done])
        }
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
            ContentBlock::ToolUse { id, name, input } => serde_json::json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }),
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
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

/// Anthropic tool definition format.
#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}
