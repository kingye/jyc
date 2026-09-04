//! OpenAI Responses API provider (`/responses`).
//!
//! The Responses API is OpenAI's successor to Chat Completions for reasoning
//! models (GPT-5.x, o-series). Key differences from Chat Completions:
//!
//! - System prompt travels as top-level `instructions`, not a message.
//! - Conversation history is an `input` array of typed items: `message`,
//!   `function_call`, `function_call_output`.
//! - Tool definitions are flat (`{type:"function", name, ...}`), not nested
//!   under a `function` key.
//! - Reasoning is controllable via `reasoning: {effort, summary}` and —
//!   unlike Chat Completions — reasoning **summaries** are streamed back as
//!   `response.reasoning_summary_text.delta` events.
//! - Tools and reasoning are NOT mutually exclusive here (they are in Chat
//!   Completions for gpt-5.6+).
//!
//! ## Context persistence format
//!
//! `raw_context` is persisted in **OpenAI chat format** (same as
//! [`super::openai_compat`]), NOT in Responses item format. The session-level
//! `filter_valid_messages` / dangling-tool-call repair only understands chat
//! and Anthropic formats, so persisting chat format keeps that machinery
//! working unchanged. Conversion to Responses input items happens at the
//! wire boundary in [`chat_messages_to_responses_input`].

use super::sse::{Event, stream_sse};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use std::collections::VecDeque;

use super::openai_compat::{
    build_openai_raw_assistant, build_openai_tool_result, build_openai_user_content,
};
use crate::provider::{EventStream, Provider};
use crate::types::{ContentBlock, Message, Role, StreamEvent, ToolDefinition};

/// OpenAI Responses API provider.
pub struct OpenAiResponsesProvider {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    /// Extra parameters to merge into the API request body (e.g.
    /// `reasoning = { effort = "low", summary = "auto" }`).
    params: Option<serde_json::Value>,
    /// Whether the active model accepts image content blocks.
    supports_images: bool,
    /// Optional User-Agent header override.
    user_agent: Option<String>,
}

impl OpenAiResponsesProvider {
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: Option<&str>,
        params: Option<serde_json::Value>,
        supports_images: bool,
        user_agent: Option<&str>,
    ) -> Result<Self> {
        // Same connection-pool hygiene as openai_compat: short idle timeout
        // avoids reusing connections silently dropped by NAT/load-balancers.
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

    /// POST `/responses` with the given input items and return the parsed
    /// event stream.
    fn send(
        &self,
        input: Vec<serde_json::Value>,
        tools: &[ToolDefinition],
        system: &str,
    ) -> Result<EventStream> {
        let url = format!("{}/responses", self.base_url);

        let mut body = serde_json::json!({
            "model": &self.model,
            "stream": true,
            // jyc persists conversation state itself; no server-side storage.
            "store": false,
            "input": input,
        });

        if !system.is_empty() {
            body["instructions"] = serde_json::Value::String(system.to_string());
        }

        if !tools.is_empty() {
            let responses_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(responses_tools);
        }

        // Merge extra params from config (provider-level + model-level)
        crate::provider::merge_params(&mut body, &self.params);

        let req = self
            .client
            .post(&url)
            .header("content-type", "application/json");
        let req = self.apply_headers(req).json(&body);

        tracing::debug!(url = %url, model = %self.model, "Sending OpenAI Responses request");

        let es = stream_sse(req);

        let stream = futures::stream::unfold(
            (es, ResponsesStreamState::default()),
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

                            // Some gateways append a Chat-Completions-style
                            // [DONE] sentinel after response.completed.
                            if data.trim() == "[DONE]" {
                                if !state.done_emitted {
                                    state.done_emitted = true;
                                    state.pending_events.push_back(StreamEvent::Done);
                                }
                                if let Some(event) = state.pending_events.pop_front() {
                                    return Some((Ok(event), (es, state)));
                                }
                                return None;
                            }

                            if let Some(events) = parse_responses_event(data, &mut state) {
                                state.pending_events.extend(events);
                            }

                            if let Some(event) = state.pending_events.pop_front() {
                                return Some((Ok(event), (es, state)));
                            }
                            // No events from this chunk (lifecycle events we
                            // ignore), continue
                        }
                        Some(Err(e)) => {
                            let err_msg = format!("{e}");
                            if err_msg.contains("Stream ended") {
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
}

#[async_trait]
impl Provider for OpenAiResponsesProvider {
    fn name(&self) -> &str {
        "openai-responses"
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
        let input: Vec<serde_json::Value> = messages
            .iter()
            .flat_map(message_to_responses_input)
            .collect();
        self.send(input, tools, system)
    }

    fn format_user_message(&self, blocks: &[ContentBlock]) -> serde_json::Value {
        // Persist in chat format (see module docs).
        build_openai_user_content(blocks)
    }

    fn format_tool_result(
        &self,
        tool_call_id: &str,
        content: &str,
        is_error: bool,
    ) -> serde_json::Value {
        // Persist in chat format (see module docs).
        build_openai_tool_result(tool_call_id, content, is_error)
    }

    fn build_raw_assistant_message(
        &self,
        text: &str,
        reasoning: &str,
        tool_calls: &[(String, String, String)],
    ) -> serde_json::Value {
        // Persist in chat format (see module docs).
        build_openai_raw_assistant(text, reasoning, tool_calls)
    }

    async fn complete_raw(
        &self,
        raw_messages: &[serde_json::Value],
        tools: &[ToolDefinition],
        system: &str,
    ) -> Result<EventStream> {
        // Raw context may contain Anthropic-format messages if the topic
        // previously ran on an Anthropic provider — normalize to chat
        // format first, then convert to Responses input items.
        let converted = super::anthropic_to_chat_messages(raw_messages);
        let filtered = super::filter_valid_messages(&converted);
        let input = chat_messages_to_responses_input(&filtered);
        self.send(input, tools, system)
    }
}

/// Convert an internal [`Message`] to Responses API input items.
///
/// One message can expand to multiple items: an assistant turn with text and
/// tool calls becomes one `message` item plus one `function_call` item per
/// tool use.
fn message_to_responses_input(msg: &Message) -> Vec<serde_json::Value> {
    match msg.role {
        Role::User => vec![user_message_item(&msg.content)],
        Role::Assistant => {
            let mut items = Vec::new();
            let text = msg.text();
            if !text.is_empty() {
                items.push(serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text }],
                }));
            }
            for block in &msg.content {
                if let ContentBlock::ToolUse { id, name, input } = block {
                    items.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": input.to_string(),
                    }));
                }
            }
            items
        }
        Role::Tool => {
            if let Some(ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            }) = msg.content.first()
            {
                vec![serde_json::json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": content,
                })]
            } else {
                vec![user_message_item(&msg.content)]
            }
        }
    }
}

/// Build a Responses `message` input item from user content blocks.
fn user_message_item(content: &[ContentBlock]) -> serde_json::Value {
    let parts: Vec<serde_json::Value> = content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(serde_json::json!({
                "type": "input_text",
                "text": text,
            })),
            ContentBlock::Image { source } => {
                use crate::types::ImageSource;
                let url = match source {
                    ImageSource::Base64 { media_type, data } => {
                        format!("data:{media_type};base64,{data}")
                    }
                    ImageSource::Url { url } => url.clone(),
                };
                Some(serde_json::json!({
                    "type": "input_image",
                    "image_url": url,
                }))
            }
            _ => None,
        })
        .collect();

    serde_json::json!({
        "type": "message",
        "role": "user",
        "content": parts,
    })
}

/// Convert persisted **chat-format** raw context to Responses API input
/// items.
///
/// `raw_context` is stored in OpenAI chat format (see module docs), so this
/// runs on every replay:
/// - `user` / `system` messages → `message` items (`input_text` /
///   `input_image` parts)
/// - `assistant` messages → `message` item (`output_text`) + one
///   `function_call` item per entry in `tool_calls`
/// - `tool` messages → `function_call_output` items
///
/// `reasoning_content` (DeepSeek-style) is dropped: Responses reasoning items
/// cannot be replayed without server-side `encrypted_content`, which we do
/// not request.
pub(crate) fn chat_messages_to_responses_input(
    messages: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut input = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "user" | "system" => {
                let item_role = if role == "system" { "system" } else { "user" };
                match msg.get("content") {
                    Some(serde_json::Value::String(text)) => {
                        input.push(serde_json::json!({
                            "type": "message",
                            "role": item_role,
                            "content": [{ "type": "input_text", "text": text }],
                        }));
                    }
                    Some(serde_json::Value::Array(parts)) => {
                        let converted: Vec<serde_json::Value> = parts
                            .iter()
                            .filter_map(|p| match p.get("type").and_then(|t| t.as_str()) {
                                Some("text") => Some(serde_json::json!({
                                    "type": "input_text",
                                    "text": p.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                                })),
                                Some("image_url") => {
                                    let url = p
                                        .get("image_url")
                                        .and_then(|i| i.get("url"))
                                        .and_then(|u| u.as_str())
                                        .unwrap_or("");
                                    Some(serde_json::json!({
                                        "type": "input_image",
                                        "image_url": url,
                                    }))
                                }
                                _ => None,
                            })
                            .collect();
                        if !converted.is_empty() {
                            input.push(serde_json::json!({
                                "type": "message",
                                "role": item_role,
                                "content": converted,
                            }));
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                if let Some(text) = msg.get("content").and_then(|c| c.as_str())
                    && !text.is_empty()
                {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": text }],
                    }));
                }
                if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tool_calls {
                        let call_id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("");
                        let arguments = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .unwrap_or("{}");
                        if !call_id.is_empty() && !name.is_empty() {
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments,
                            }));
                        }
                    }
                }
            }
            "tool" => {
                let call_id = msg
                    .get("tool_call_id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("");
                let output = match msg.get("content") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                if !call_id.is_empty() {
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": output,
                    }));
                }
            }
            _ => {}
        }
    }

    input
}

/// Internal state for Responses API stream parsing.
#[derive(Default)]
struct ResponsesStreamState {
    /// Events ready to be yielded (FIFO queue).
    pending_events: VecDeque<StreamEvent>,
    /// Number of `function_call` output items started but not yet done.
    open_function_calls: usize,
    /// Whether `StreamEvent::Done` has already been emitted (guards against
    /// a trailing `[DONE]` sentinel after `response.completed`).
    done_emitted: bool,
}

/// Parse a single Responses API SSE event into StreamEvents.
///
/// The event type is carried inside the data JSON's `"type"` field, so the
/// generic SSE layer (which ignores `event:` lines) needs no changes.
/// Unknown event types (lifecycle noise like `response.created`,
/// `response.output_item.added` for message items, `content_part` events,
/// etc.) are ignored for forward compatibility.
fn parse_responses_event(data: &str, state: &mut ResponsesStreamState) -> Option<Vec<StreamEvent>> {
    let value: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::trace!(error = %e, data_preview = %&data[..data.len().min(100)], "Failed to parse Responses SSE event JSON");
            return None;
        }
    };

    let event_type = value.get("type").and_then(|t| t.as_str())?;

    let mut events = Vec::new();

    match event_type {
        "response.output_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(|d| d.as_str())
                && !delta.is_empty()
            {
                events.push(StreamEvent::TextDelta(delta.to_string()));
            }
        }
        // Reasoning summary stream — the only way to surface GPT-5.x
        // thinking (Chat Completions never exposes reasoning content).
        // Requires `reasoning = { summary = "auto" }` (or "detailed") in
        // the model's `params`.
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = value.get("delta").and_then(|d| d.as_str())
                && !delta.is_empty()
            {
                events.push(StreamEvent::ReasoningDelta(delta.to_string()));
            }
        }
        "response.output_item.added" => {
            let item = value.get("item")?;
            if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                let call_id = item
                    .get("call_id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                if !call_id.is_empty() && !name.is_empty() {
                    state.open_function_calls += 1;
                    events.push(StreamEvent::ToolUseStart { id: call_id, name });
                }
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = value.get("delta").and_then(|d| d.as_str())
                && !delta.is_empty()
            {
                events.push(StreamEvent::ToolInputDelta(delta.to_string()));
            }
        }
        "response.output_item.done" => {
            let item = value.get("item")?;
            if item.get("type").and_then(|t| t.as_str()) == Some("function_call")
                && state.open_function_calls > 0
            {
                state.open_function_calls -= 1;
                events.push(StreamEvent::ToolUseEnd);
            }
        }
        "response.completed" => {
            if let Some(usage) = value
                .get("response")
                .and_then(|r| r.get("usage"))
                .filter(|u| u.is_object())
            {
                let input = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let output = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if input > 0 || output > 0 {
                    let cache_hit = usage
                        .get("input_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let reasoning = usage
                        .get("output_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    events.push(StreamEvent::Usage {
                        input_tokens: input,
                        output_tokens: output,
                        cache_hit_tokens: cache_hit,
                        cache_creation_tokens: 0,
                        reasoning_tokens: reasoning,
                    });
                }
            }
            if !state.done_emitted {
                state.done_emitted = true;
                events.push(StreamEvent::Done);
            }
        }
        "response.failed" | "response.incomplete" => {
            let msg = value
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or(event_type);
            events.push(StreamEvent::Error(msg.to_string()));
        }
        "error" => {
            let msg = value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown Responses API error");
            events.push(StreamEvent::Error(msg.to_string()));
        }
        _ => {}
    }

    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    #[test]
    fn parse_output_text_delta() {
        let mut state = ResponsesStreamState::default();
        let event = serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "Hello",
        });
        let events = parse_responses_event(&event.to_string(), &mut state).unwrap();
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "Hello"));
    }

    #[test]
    fn parse_completed_emits_usage_and_done() {
        let mut state = ResponsesStreamState::default();
        let event = serde_json::json!({
            "type": "response.completed",
            "response": {
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 50,
                    "input_tokens_details": { "cached_tokens": 40 },
                    "output_tokens_details": { "reasoning_tokens": 20 },
                }
            }
        });
        let events = parse_responses_event(&event.to_string(), &mut state).unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_hit_tokens: 40,
                reasoning_tokens: 20,
                ..
            }
        )));
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Done)));

        // A trailing [DONE]-equivalent completed event must not re-emit Done.
        let events2 = parse_responses_event(&event.to_string(), &mut state).unwrap();
        assert!(!events2.iter().any(|e| matches!(e, StreamEvent::Done)));
    }

    #[test]
    fn parse_reasoning_summary_delta() {
        let mut state = ResponsesStreamState::default();
        let event = serde_json::json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "Comparing the two numbers…",
        });
        let events = parse_responses_event(&event.to_string(), &mut state).unwrap();
        assert!(
            matches!(&events[0], StreamEvent::ReasoningDelta(t) if t == "Comparing the two numbers…")
        );
    }

    #[test]
    fn parse_function_call_streaming_sequence() {
        let mut state = ResponsesStreamState::default();

        let added = serde_json::json!({
            "type": "response.output_item.added",
            "item": { "type": "function_call", "call_id": "call_1", "name": "read" },
        });
        let events = parse_responses_event(&added.to_string(), &mut state).unwrap();
        assert!(matches!(
            &events[0],
            StreamEvent::ToolUseStart { id, name } if id == "call_1" && name == "read"
        ));

        let delta = serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_1",
            "delta": "{\"path\":",
        });
        let events = parse_responses_event(&delta.to_string(), &mut state).unwrap();
        assert!(matches!(&events[0], StreamEvent::ToolInputDelta(d) if d == "{\"path\":"));

        // arguments.done is ignored — output_item.done is the single
        // ToolUseEnd source (avoids double-End).
        let args_done = serde_json::json!({
            "type": "response.function_call_arguments.done",
            "item_id": "fc_1",
            "arguments": "{\"path\":\"a\"}",
        });
        assert!(parse_responses_event(&args_done.to_string(), &mut state).is_none());

        let item_done = serde_json::json!({
            "type": "response.output_item.done",
            "item": { "type": "function_call", "call_id": "call_1", "name": "read",
                      "arguments": "{\"path\":\"a\"}" },
        });
        let events = parse_responses_event(&item_done.to_string(), &mut state).unwrap();
        assert!(matches!(&events[0], StreamEvent::ToolUseEnd));

        // A stray output_item.done without a matching added emits nothing.
        let events = parse_responses_event(&item_done.to_string(), &mut state);
        assert!(events.is_none());
    }

    #[test]
    fn parse_failed_emits_error() {
        let mut state = ResponsesStreamState::default();
        let event = serde_json::json!({
            "type": "response.failed",
            "response": { "error": { "message": "model overloaded" } }
        });
        let events = parse_responses_event(&event.to_string(), &mut state).unwrap();
        assert!(matches!(&events[0], StreamEvent::Error(m) if m == "model overloaded"));
    }

    #[test]
    fn unknown_events_ignored() {
        let mut state = ResponsesStreamState::default();
        for ty in [
            "response.created",
            "response.in_progress",
            "response.content_part.added",
            "response.output_text.done",
            "response.reasoning_summary_part.added",
        ] {
            let event = serde_json::json!({ "type": ty });
            assert!(parse_responses_event(&event.to_string(), &mut state).is_none());
        }
    }

    #[test]
    fn convert_chat_user_string_content() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let input = chat_messages_to_responses_input(&msgs);
        assert_eq!(
            input,
            vec![serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "hello" }],
            })]
        );
    }

    #[test]
    fn convert_chat_assistant_with_tool_calls_and_results() {
        let msgs = vec![
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"a\"}"},
                }],
            }),
            serde_json::json!({
                "role": "tool",
                "tool_call_id": "call_1",
                "content": "[SUCCESS] file body",
            }),
        ];
        let input = chat_messages_to_responses_input(&msgs);
        assert_eq!(
            input,
            vec![
                serde_json::json!({
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read",
                    "arguments": "{\"path\":\"a\"}",
                }),
                serde_json::json!({
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "[SUCCESS] file body",
                }),
            ]
        );
    }

    #[test]
    fn convert_chat_drops_reasoning_content() {
        let msgs = vec![serde_json::json!({
            "role": "assistant",
            "content": "answer",
            "reasoning_content": "thinking...",
        })];
        let input = chat_messages_to_responses_input(&msgs);
        assert_eq!(input.len(), 1);
        assert!(input[0].get("reasoning_content").is_none());
    }

    #[test]
    fn message_to_input_expands_assistant_tool_use() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "checking".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_9".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ],
        };
        let items = message_to_responses_input(&msg);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["role"], "assistant");
        assert_eq!(items[0]["content"][0]["type"], "output_text");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[1]["call_id"], "call_9");
        assert_eq!(items[1]["arguments"], "{\"command\":\"ls\"}");
    }
}
