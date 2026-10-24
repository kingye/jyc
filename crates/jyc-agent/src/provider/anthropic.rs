//! Native Anthropic Messages API provider.
//!
//! Implements streaming via SSE to the `/messages` endpoint.
//! Supports custom base_url (for proxies) and API key authentication.

use super::sse::{Event, stream_sse};
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use serde::Serialize;

use crate::provider::usage::{anthropic_total_input_tokens, extract_anthropic_cache_split};
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
    /// Optional User-Agent header override.
    user_agent: Option<String>,
    /// Use 1-hour prompt-cache TTL (extended-cache-ttl beta) instead of the
    /// default 5 minutes. Every breakpoint is written with `"ttl": "1h"`,
    /// billed at 2× input price instead of the 1.25× 5-minute write rate.
    cache_ttl_1h: bool,
}

impl AnthropicProvider {
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: Option<&str>,
        params: Option<serde_json::Value>,
        supports_images: bool,
        user_agent: Option<&str>,
        cache_ttl_1h: bool,
    ) -> Result<Self> {
        let client = super::http_client()?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key: api_key.map(|s| s.to_string()),
            params,
            supports_images,
            user_agent: user_agent.map(|s| s.to_string()),
            cache_ttl_1h,
        })
    }

    /// Send one Messages API request and decode its SSE stream.
    ///
    /// `complete` and `complete_raw` differ only in how the request body is
    /// built (typed messages vs. the journal wire format); everything from
    /// the POST onwards is the same, so it lives here once.
    ///
    /// The parser yields at most one event per SSE message, so the stream
    /// needs no buffering of its own.
    fn send(&self, body: serde_json::Value) -> Result<EventStream> {
        let url = format!("{}/messages", self.base_url);

        // Build request
        let mut req = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01");

        if self.cache_ttl_1h {
            req = req.header("anthropic-beta", "extended-cache-ttl-2025-04-11");
        }

        if let Some(ref key) = self.api_key {
            req = req.header("x-api-key", key);
        }

        if let Some(ref ua) = self.user_agent {
            req = req.header("user-agent", ua.as_str());
        }

        req = req.json(&body);

        // Create SSE stream
        let es = stream_sse(req);

        tracing::debug!(url = %url, model = %self.model, "Sending Anthropic request");

        // Transform SSE events into our StreamEvent type
        let stream = futures::stream::unfold(
            (es, StreamState::default()),
            |(mut es, mut state)| async move {
                loop {
                    match es.next().await {
                        Some(Ok(Event::Open)) => continue,
                        Some(Ok(Event::Message(msg))) => {
                            match parse_anthropic_sse(&msg.data, &mut state) {
                                Some(event) => return Some((Ok(event), (es, state))),
                                None => continue,
                            }
                        }
                        Some(Err(e)) => {
                            let err_msg = format!("{e}");
                            if err_msg.contains("Stream ended") {
                                return None;
                            }
                            return Some((Err(anyhow::anyhow!("SSE error: {e}")), (es, state)));
                        }
                        None => return None,
                    }
                }
            },
        );

        Ok(Box::pin(stream))
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
        // Convert messages to Anthropic format
        let api_messages = merge_consecutive_user_messages(
            messages
                .iter()
                .map(to_anthropic_message)
                .collect::<Vec<_>>(),
        );

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
        crate::provider::merge_params(&mut body, &self.params);

        // Prompt-cache breakpoints. See `complete_raw` above for why this
        // runs after the params merge.
        apply_cache_breakpoints(&mut body, self.cache_ttl_1h);

        self.send(body)
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
        let api_tools: Vec<AnthropicTool> = tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: sanitize_input_schema(t.input_schema.clone()),
            })
            .collect();

        // The journal is written in the active provider's wire format, so a
        // topic that ran on an OpenAI-family model holds `tool_calls` /
        // `role:"tool"` turns. Normalize them first — the Messages API
        // rejects those fields (HTTP 400 at the first such message) and
        // needs the `tool_use`/`tool_result` pairing instead.
        let converted = super::chat_to_anthropic_messages(raw_messages);
        let filtered_messages =
            merge_consecutive_user_messages(super::filter_valid_messages(&converted));

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
        crate::provider::merge_params(&mut body, &self.params);

        // Prompt-cache breakpoints. Applied last so a `system` or `tools`
        // override coming from `params` is marked too, and cannot clobber
        // the markers by being merged over them.
        apply_cache_breakpoints(&mut body, self.cache_ttl_1h);

        self.send(body)
    }
}

/// Internal state for parsing the SSE stream.
#[derive(Default)]
struct StreamState {
    current_tool_id: Option<String>,
    current_tool_name: Option<String>,
    tool_input_buffer: String,
}

/// Parse a single Anthropic SSE event into at most one StreamEvent.
fn parse_anthropic_sse(data: &str, state: &mut StreamState) -> Option<StreamEvent> {
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
                return Some(StreamEvent::ToolUseStart { id, name });
            }
            if block_type == "thinking" {
                // Anthropic extended thinking: the block may carry initial
                // thinking text in `thinking`. Without this arm the opening
                // chunk of the thinking block would be lost (only the
                // subsequent `thinking_delta` events would be captured).
                if let Some(text) = block.get("thinking").and_then(|t| t.as_str())
                    && !text.is_empty()
                {
                    return Some(StreamEvent::ReasoningDelta(text.to_string()));
                }
                return None;
            }
            None
        }
        "content_block_delta" => {
            let delta = value.get("delta")?;
            let delta_type = delta.get("type")?.as_str()?;
            match delta_type {
                "text_delta" => {
                    let text = delta.get("text")?.as_str()?.to_string();
                    Some(StreamEvent::TextDelta(text))
                }
                "input_json_delta" => {
                    let partial = delta.get("partial_json")?.as_str()?.to_string();
                    state.tool_input_buffer.push_str(&partial);
                    Some(StreamEvent::ToolInputDelta(partial))
                }
                "thinking_delta" => {
                    // Anthropic extended thinking: incremental thinking text.
                    // Without this arm, the thinking content is silently dropped
                    // (falls to `_ => None`) and never reaches the chat pane.
                    let text = delta.get("thinking")?.as_str()?.to_string();
                    Some(StreamEvent::ReasoningDelta(text))
                }
                _ => None,
            }
        }
        "content_block_stop" => {
            if state.current_tool_id.is_some() {
                state.current_tool_id = None;
                state.current_tool_name = None;
                state.tool_input_buffer.clear();
                return Some(StreamEvent::ToolUseEnd);
            }
            None
        }
        "message_delta" => {
            // May contain usage info
            if let Some(usage) = value.get("usage") {
                // Normalized to include cached tokens — see
                // `anthropic_total_input_tokens` for why the raw field is wrong
                // for cost accounting.
                let input = anthropic_total_input_tokens(usage);
                let output = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if input > 0 || output > 0 {
                    // Anthropic reports reads and writes as separate
                    // buckets; surface both so `compute_cost_split` can
                    // bill each at its configured rate. `cache_hit_tokens`
                    // carries the cache-**read** bucket only — writes
                    // accumulate separately in `cache_creation_tokens`.
                    // Non-Anthropic providers fill this with their single
                    // bucket via `extract_cache_hit_tokens` and
                    // `cache_creation_tokens = 0`.
                    let (cache_read, cache_creation) = extract_anthropic_cache_split(usage);
                    return Some(StreamEvent::Usage {
                        input_tokens: input,
                        output_tokens: output,
                        cache_hit_tokens: cache_read,
                        cache_creation_tokens: cache_creation,
                        // Anthropic does not break out reasoning tokens.
                        reasoning_tokens: 0,
                    });
                }
            }
            None
        }
        "message_start" => {
            // Extract initial usage
            if let Some(usage) = value.get("message").and_then(|m| m.get("usage")) {
                let input = anthropic_total_input_tokens(usage);
                let output = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if input > 0 {
                    let (cache_read, cache_creation) = extract_anthropic_cache_split(usage);
                    return Some(StreamEvent::Usage {
                        input_tokens: input,
                        output_tokens: output,
                        cache_hit_tokens: cache_read,
                        cache_creation_tokens: cache_creation,
                        // Anthropic does not break out reasoning tokens.
                        reasoning_tokens: 0,
                    });
                }
            }
            None
        }
        "message_stop" => Some(StreamEvent::Done),
        "error" => {
            let error_msg = value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown error")
                .to_string();
            Some(StreamEvent::Error(error_msg))
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

/// Merge consecutive `user` messages into one.
///
/// The Anthropic protocol carries a tool result as a `user` message, so
/// whatever the turn adds between two calls — a live injection, an image
/// a tool loaded — puts two `user` turns back to back on the wire. The
/// Messages API combines same-role turns itself, but the strict routes
/// built on it (Bedrock, Vertex, and the gateways that re-implement the
/// validation) reject the pair outright:
/// `messages: roles must alternate between "user" and "assistant"`.
///
/// Combining them here is what the API does anyway, and it keeps the
/// request valid on every route. The blocks keep their order, so a
/// `tool_result` stays ahead of the text that follows it.
fn merge_consecutive_user_messages(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut merged: Vec<serde_json::Value> = Vec::with_capacity(messages.len());

    for message in messages {
        let is_user = message.get("role").and_then(|r| r.as_str()) == Some("user");
        let previous_is_user = merged
            .last()
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            == Some("user");

        if !(is_user && previous_is_user) {
            merged.push(message);
            continue;
        }

        // The two turns share one: the blocks of the previous turn
        // first, then the ones this message carries. A previous turn
        // whose content is a plain string becomes a text block, so
        // merging never drops what the turn already said.
        let last_index = merged.len() - 1;
        let mut blocks = content_blocks(&merged[last_index]);
        blocks.extend(content_blocks(&message));
        merged[last_index]["content"] = serde_json::Value::Array(blocks);
    }

    merged
}

/// The content blocks of a message.
///
/// Anthropic carries them as an array. A message whose content is a
/// plain string becomes a text block, so a merged message keeps one
/// shape.
fn content_blocks(message: &serde_json::Value) -> Vec<serde_json::Value> {
    match message.get("content") {
        Some(serde_json::Value::Array(blocks)) => blocks.clone(),
        Some(serde_json::Value::String(text)) => {
            vec![serde_json::json!({"type": "text", "text": text})]
        }
        _ => Vec::new(),
    }
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

/// Attach an `ephemeral` cache-control marker to an array's last element.
///
/// With `ttl_1h` the marker carries `"ttl": "1h"` (extended-cache-ttl beta);
/// otherwise the API default of 5 minutes applies.
///
/// No-ops on an empty array or a non-object last element.
fn mark_last(arr: &mut [serde_json::Value], ttl_1h: bool) {
    if let Some(obj) = arr.last_mut().and_then(|v| v.as_object_mut()) {
        obj.insert(
            "cache_control".to_string(),
            if ttl_1h {
                serde_json::json!({ "type": "ephemeral", "ttl": "1h" })
            } else {
                serde_json::json!({ "type": "ephemeral" })
            },
        );
    }
}

/// Whether any `cache_control` marker already exists anywhere in the body.
///
/// Used to detect caller-supplied markers (a `system` array or tool passed
/// through provider `params`) so this module doesn't add its own on top and
/// blow Anthropic's 4-breakpoint ceiling.
fn has_cache_control(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(map) => {
            map.contains_key("cache_control") || map.values().any(has_cache_control)
        }
        serde_json::Value::Array(items) => items.iter().any(has_cache_control),
        _ => false,
    }
}

/// Attach a `cache_control` marker to a message's **last content block**.
///
/// The marker must sit on a content block, never on the message object
/// itself — `{"role":"user","cache_control":{...}}` is rejected by the API.
/// String content (`"content": "hi"`) is normalized into a single text block
/// first, since a bare string has nowhere to hang the marker.
///
/// No-ops when the message has no content blocks to mark.
fn mark_last_block_cached(msg: &mut serde_json::Value, ttl_1h: bool) {
    // Normalize `content: "text"` → `content: [{"type":"text","text":"text"}]`
    if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
        msg["content"] = serde_json::json!([{ "type": "text", "text": text }]);
    }

    if let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut()) {
        mark_last(blocks, ttl_1h);
    }
}

/// Insert Anthropic prompt-cache breakpoints into a built request body.
///
/// Anthropic allows at most 4 `cache_control` breakpoints per request, and a
/// breakpoint only pays off when it sits on the **last** element of a static
/// span — placed mid-flux, every downstream cache entry is invalidated as
/// soon as the dynamic part changes. The cache prefix is ordered
/// `tools → system → messages`, so the four land on the tools tail, the
/// system tail, and messages `n-3` and `n-2`.
///
/// Breakpoints #1 and #2 are kept separate rather than collapsed into one:
/// the tools array is identical across every topic, while the system prompt
/// varies per topic (working directory, skills, AGENTS.md), so a
/// tools-only prefix stays shareable between topics.
///
/// The newest message is deliberately left unmarked — it changes on every
/// request, so a breakpoint there would be written and immediately orphaned.
///
/// The 4-breakpoint budget is satisfied structurally (1 + 1 + 2). The one way
/// to exceed it is a caller that ships its own `cache_control` via provider
/// `params`; in that case the caller's layout wins and this is a no-op, since
/// a 5th breakpoint is a hard API error. Marking is otherwise skipped where a
/// span is absent or too short. Prompts below the model's minimum cacheable
/// length (1024 tokens for Opus/Sonnet, 2048 for Haiku) are ignored by the
/// API rather than erroring.
fn apply_cache_breakpoints(body: &mut serde_json::Value, ttl_1h: bool) {
    // The caller already placed breakpoints — respect their layout rather
    // than adding to it and overflowing the per-request limit.
    if has_cache_control(body) {
        return;
    }

    // #1 — tools tail.
    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
        mark_last(tools, ttl_1h);
    }

    // #2 — system tail. `system` is built as a plain string; promote it to a
    // single-block array so the marker has a block to attach to. An empty
    // prompt has nothing to cache and is left exactly as it was.
    if let Some(text) = body
        .get("system")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
    {
        body["system"] = serde_json::json!([{ "type": "text", "text": text }]);
    }
    if let Some(blocks) = body.get_mut("system").and_then(|s| s.as_array_mut()) {
        mark_last(blocks, ttl_1h);
    }

    // #3 / #4 — rolling window over history, skipping the newest message:
    // the last two of `messages[..n-1]`, i.e. `n-3` and `n-2`.
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        let n = messages.len();
        for idx in [n.checked_sub(3), n.checked_sub(2)].into_iter().flatten() {
            mark_last_block_cached(&mut messages[idx], ttl_1h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;
    use futures::StreamExt;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Two `user` turns in a row are one turn on the wire: the tool
    /// result and the text that follows it share a message.
    #[test]
    fn consecutive_user_messages_merge_into_one() {
        let merged = merge_consecutive_user_messages(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "working"}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "extra info"}]}),
        ]);

        assert_eq!(merged.len(), 3, "the two user turns became one");
        assert_eq!(merged[2]["role"], "user");
        let blocks = merged[2]["content"].as_array().expect("content blocks");
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            blocks[0]["type"], "tool_result",
            "the tool result stays first"
        );
        assert_eq!(blocks[1]["text"], "extra info");
    }

    /// A user turn whose content is a plain string keeps its words when
    /// the next user turn joins it.
    #[test]
    fn merging_a_string_content_turn_keeps_both_texts() {
        let merged = merge_consecutive_user_messages(vec![
            json!({"role": "user", "content": "plain string"}),
            json!({"role": "user", "content": [{"type": "text", "text": "extra info"}]}),
        ]);

        assert_eq!(merged.len(), 1);
        let blocks = merged[0]["content"].as_array().expect("content blocks");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"], "plain string");
        assert_eq!(blocks[1]["text"], "extra info");
    }

    /// An assistant turn between two user turns keeps them apart.
    #[test]
    fn user_messages_around_an_assistant_turn_stay_apart() {
        let merged = merge_consecutive_user_messages(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "c"}]}),
        ]);

        assert_eq!(merged.len(), 3);
        assert_eq!(merged[2]["content"][0]["text"], "c");
    }

    /// Three user turns in a row collapse into one.
    #[test]
    fn three_consecutive_user_messages_merge_into_one() {
        let merged = merge_consecutive_user_messages(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "b"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "c"}]}),
        ]);

        assert_eq!(merged.len(), 1);
        let blocks = merged[0]["content"].as_array().expect("content blocks");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[2]["text"], "c");
    }

    /// The wire body carries one user turn where the conversation had two:
    /// a tool result followed by the message that arrived mid-turn.
    #[tokio::test]
    async fn complete_raw_sends_one_user_turn_for_a_tool_result_and_an_injection() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
            )
            .mount(&server)
            .await;

        let raw = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "do the thing"}]}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "bash", "input": {}}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "extra info"}]}),
        ];

        let provider = AnthropicProvider::new(
            &server.uri(),
            "claude-test",
            Some("k"),
            None,
            false,
            None,
            false,
        )
        .unwrap();
        let stream = provider.complete_raw(&raw, &[], "").await.expect("stream");
        tokio::pin!(stream);
        while stream.next().await.is_some() {}

        let requests = server.received_requests().await.expect("requests recorded");
        let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
        let messages = sent["messages"].as_array().expect("messages array");

        assert_eq!(messages.len(), 3, "one user turn, not two: {sent:#}");
        assert_eq!(messages[2]["role"], "user");
        let blocks = messages[2]["content"].as_array().expect("content blocks");
        assert_eq!(
            blocks.len(),
            2,
            "the tool result and the text share the turn"
        );
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["text"], "extra info");
    }

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

    /// When Anthropic rejects the request with 400, the SSE client must
    /// surface the response body in the error message seen by the agent
    /// loop.
    ///
    /// This is the exact failure pattern observed in production where the
    /// agent saw `SSE error: Invalid status code: 400 Bad Request` with
    /// no body, hiding the real cause (an unsupported `thinking.type`
    /// param sent via the provider's `params` config merge). The SSE
    /// client now embeds the body directly:
    ///
    ///   `SSE error: Invalid status code: 400 Bad Request body: {...}`
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
            msg.contains("body:"),
            "expected embedded response body in error, got: {msg}"
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

    #[tokio::test]
    async fn complete_sends_custom_user_agent() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/messages"))
            .and(header("content-type", "application/json"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(header("x-api-key", "test-key"))
            .and(header("user-agent", "opencode/1.15.13"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let provider = AnthropicProvider::new(
            &server.uri(),
            "claude-test-model",
            Some("test-key"),
            None,
            false,
            Some("opencode/1.15.13"),
            false,
        )
        .expect("provider construction");

        let messages = vec![Message::user("hello")];
        let mut stream = provider
            .complete(&messages, &[], "")
            .await
            .expect("complete should return a stream");
        while stream.next().await.is_some() {}

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].headers.get("user-agent").unwrap(),
            "opencode/1.15.13"
        );
    }

    /// Recursively count `cache_control` markers anywhere in a JSON value.
    fn count_breakpoints(v: &serde_json::Value) -> usize {
        match v {
            serde_json::Value::Object(map) => {
                let here = usize::from(map.contains_key("cache_control"));
                here + map.values().map(count_breakpoints).sum::<usize>()
            }
            serde_json::Value::Array(items) => items.iter().map(count_breakpoints).sum(),
            _ => 0,
        }
    }

    fn body_with(messages: serde_json::Value, tools: serde_json::Value) -> serde_json::Value {
        json!({
            "model": "claude-test",
            "max_tokens": 16384,
            "stream": true,
            "system": "You are a helpful agent.",
            "tools": tools,
            "messages": messages,
        })
    }

    /// The canonical layout: 4 breakpoints at tools tail, system tail,
    /// and messages n-3 / n-2 — never more than Anthropic's limit of 4.
    #[test]
    fn cache_breakpoints_standard_layout() {
        let mut body = body_with(
            json!([
                {"role": "user", "content": [{"type": "text", "text": "m0"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "m1"}]},
                {"role": "user", "content": [{"type": "text", "text": "m2"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "m3"}]},
                {"role": "user", "content": [{"type": "text", "text": "m4"}]},
            ]),
            json!([
                {"name": "bash", "description": "d", "input_schema": {}},
                {"name": "write", "description": "d", "input_schema": {}},
            ]),
        );
        apply_cache_breakpoints(&mut body, false);

        assert_eq!(
            count_breakpoints(&body),
            4,
            "must place exactly 4 breakpoints (Anthropic's per-request max), got: {body:#}"
        );

        // #1 tools tail — on the LAST tool only.
        let tools = body["tools"].as_array().unwrap();
        assert!(tools[0].get("cache_control").is_none(), "first tool");
        assert!(tools[1].get("cache_control").is_some(), "last tool");

        // #2 system tail — string promoted to a single-block array.
        let system = body["system"].as_array().expect("system became an array");
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["text"], "You are a helpful agent.");
        assert!(system[0].get("cache_control").is_some());

        // #3 / #4 — messages n-3 (idx 2) and n-2 (idx 3); newest (idx 4) clean.
        let msgs = body["messages"].as_array().unwrap();
        for (i, expected) in [(0, false), (1, false), (2, true), (3, true), (4, false)] {
            let marked = msgs[i]["content"][0].get("cache_control").is_some();
            assert_eq!(marked, expected, "message idx {i} marker mismatch");
        }
    }

    /// The marker must land on a message's LAST content block — an assistant
    /// turn ending in `tool_use` gets it on the tool_use, not the text.
    #[test]
    fn cache_breakpoint_marks_last_content_block() {
        let mut msg = json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Let me look."},
                {"type": "tool_use", "id": "toolu_1", "name": "grep", "input": {}},
            ],
        });
        mark_last_block_cached(&mut msg, false);

        assert!(
            msg["content"][0].get("cache_control").is_none(),
            "first block must stay unmarked"
        );
        assert!(
            msg["content"][1].get("cache_control").is_some(),
            "last block must carry the marker"
        );
        assert!(
            msg.get("cache_control").is_none(),
            "marker must never sit on the message object — the API rejects it"
        );
    }

    /// String content has nowhere to hang a marker, so it is normalized into
    /// a single text block first.
    #[test]
    fn cache_breakpoint_normalizes_string_content() {
        let mut msg = json!({ "role": "user", "content": "plain string" });
        mark_last_block_cached(&mut msg, false);

        let blocks = msg["content"].as_array().expect("content became an array");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "plain string");
        assert!(blocks[0].get("cache_control").is_some());
    }

    /// Conversations shorter than the rolling window must not panic or
    /// over-mark. The newest message is never marked, so 1 message yields no
    /// message breakpoints and 2 yields one (at `n-2`, i.e. idx 0). The static
    /// tools + system spans are always marked, hence the +2.
    #[test]
    fn cache_breakpoints_short_conversations() {
        for (count, expected_total) in [(1_usize, 2_usize), (2, 3)] {
            let messages: Vec<serde_json::Value> = (0..count)
                .map(|i| json!({"role": "user", "content": [{"type": "text", "text": format!("m{i}")}]}))
                .collect();
            let mut body = body_with(
                json!(messages),
                json!([{"name": "bash", "description": "d", "input_schema": {}}]),
            );
            apply_cache_breakpoints(&mut body, false);

            assert_eq!(
                count_breakpoints(&body),
                expected_total,
                "{count} message(s) should yield {expected_total} breakpoints, got: {body:#}"
            );

            let msgs = body["messages"].as_array().unwrap();
            assert!(
                msgs[count - 1]["content"][0].get("cache_control").is_none(),
                "newest message must never be marked ({count} message case)"
            );
            if count == 2 {
                assert!(
                    msgs[0]["content"][0].get("cache_control").is_some(),
                    "with 2 messages, n-2 (idx 0) must be marked"
                );
            }
        }
    }

    /// Absent tools and an empty system prompt must be left untouched:
    /// no `tools` key materialized, no empty system array.
    #[test]
    fn cache_breakpoints_no_tools_empty_system() {
        let mut body = json!({
            "model": "claude-test",
            "system": "",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        });
        apply_cache_breakpoints(&mut body, false);

        assert!(body.get("tools").is_none(), "must not invent a tools key");
        assert_eq!(body["system"], "", "empty system stays an empty string");
        assert_eq!(count_breakpoints(&body), 0);
    }

    /// A caller-supplied `system` array (via provider `params`) already has
    /// blocks; the marker goes on its last one instead of overwriting it.
    #[test]
    fn cache_breakpoints_preserves_caller_system_array() {
        let mut body = json!({
            "model": "claude-test",
            "system": [
                {"type": "text", "text": "role"},
                {"type": "text", "text": "knowledge base"},
            ],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        });
        apply_cache_breakpoints(&mut body, false);

        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2, "caller blocks must be preserved");
        assert!(system[0].get("cache_control").is_none());
        assert!(system[1].get("cache_control").is_some());
    }

    /// A caller that ships its own `cache_control` through provider `params`
    /// owns the layout. Adding our four on top would reach 5+ breakpoints,
    /// which Anthropic rejects outright — so this must be a no-op.
    #[test]
    fn cache_breakpoints_defers_to_caller_supplied_markers() {
        let caller_system = json!([{
            "type": "text",
            "text": "caller knows best",
            "cache_control": { "type": "ephemeral" },
        }]);
        let mut body = body_with(
            json!([
                {"role": "user", "content": [{"type": "text", "text": "m0"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "m1"}]},
                {"role": "user", "content": [{"type": "text", "text": "m2"}]},
            ]),
            json!([{"name": "bash", "description": "d", "input_schema": {}}]),
        );
        body["system"] = caller_system.clone();
        let before = body.clone();

        apply_cache_breakpoints(&mut body, false);

        assert_eq!(
            body, before,
            "must not touch a body that already carries cache_control"
        );
        assert_eq!(
            count_breakpoints(&body),
            1,
            "only the caller's single marker survives, never 5"
        );
    }

    /// The same deference applies when the caller's marker is on a tool
    /// rather than the system prompt — the scan is whole-body.
    #[test]
    fn cache_breakpoints_defers_to_caller_marker_on_tools() {
        let mut body = body_with(
            json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}]),
            json!([{
                "name": "bash",
                "description": "d",
                "input_schema": {},
                "cache_control": { "type": "ephemeral" },
            }]),
        );
        let before = body.clone();

        apply_cache_breakpoints(&mut body, false);

        assert_eq!(body, before, "caller's tool marker wins");
        assert_eq!(count_breakpoints(&body), 1);
    }

    /// End-to-end: the breakpoints must survive into the actual HTTP body
    /// sent to Anthropic. The helper-level tests only prove the transform;
    /// this proves it is wired into `complete_raw` (the production path).
    #[tokio::test]
    async fn complete_raw_sends_cache_breakpoints_on_the_wire() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
            )
            .mount(&server)
            .await;

        let tools = vec![
            ToolDefinition {
                name: "bash".to_string(),
                description: "run".to_string(),
                input_schema: json!({"type": "object"}),
            },
            ToolDefinition {
                name: "write".to_string(),
                description: "write".to_string(),
                input_schema: json!({"type": "object"}),
            },
        ];

        let raw = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "m0"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "m1"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "m2"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "m3"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "m4"}]}),
        ];

        let provider = AnthropicProvider::new(
            &server.uri(),
            "claude-test",
            Some("k"),
            None,
            false,
            None,
            false,
        )
        .unwrap();
        let stream = provider
            .complete_raw(&raw, &tools, "system prompt here")
            .await
            .expect("stream");
        // Drain so the request is definitely issued.
        tokio::pin!(stream);
        while stream.next().await.is_some() {}

        let requests = server.received_requests().await.expect("requests recorded");
        let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");

        assert_eq!(
            count_breakpoints(&sent),
            4,
            "wire body must carry exactly 4 breakpoints, got: {sent:#}"
        );
        // system promoted to a block array carrying the marker
        assert_eq!(sent["system"][0]["text"], "system prompt here");
        assert!(sent["system"][0].get("cache_control").is_some());
        // tools sorted upstream by the registry; marker on the last one only
        assert!(sent["tools"][0].get("cache_control").is_none());
        assert!(sent["tools"][1].get("cache_control").is_some());
        // rolling window: idx 2 and 3 marked, newest (idx 4) untouched
        assert!(sent["messages"][2]["content"][0]["cache_control"].is_object());
        assert!(sent["messages"][3]["content"][0]["cache_control"].is_object());
        assert!(
            sent["messages"][4]["content"][0]
                .get("cache_control")
                .is_none()
        );
        // default TTL: no beta header, bare ephemeral markers
        assert!(requests[0].headers.get("anthropic-beta").is_none());
        assert_eq!(
            sent["system"][0]["cache_control"],
            json!({"type": "ephemeral"}),
        );
    }

    /// With `cache_ttl = "1h"` every breakpoint on the wire carries
    /// `"ttl": "1h"` and the request opts into the extended-cache-ttl beta.
    #[tokio::test]
    async fn complete_raw_sends_ttl_1h_breakpoints_and_beta_header() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
            )
            .mount(&server)
            .await;

        let raw = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "m0"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "m1"}]}),
            json!({"role": "user", "content": [{"type": "text", "text": "m2"}]}),
        ];

        let provider = AnthropicProvider::new(
            &server.uri(),
            "claude-test",
            Some("k"),
            None,
            false,
            None,
            true,
        )
        .unwrap();
        let stream = provider
            .complete_raw(&raw, &[], "system prompt here")
            .await
            .expect("stream");
        // Drain so the request is definitely issued.
        tokio::pin!(stream);
        while stream.next().await.is_some() {}

        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(
            requests[0]
                .headers
                .get("anthropic-beta")
                .expect("beta header"),
            "extended-cache-ttl-2025-04-11"
        );

        let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
        // No tools: system tail + messages n-3/n-2 (idx 0 and 1) = 3 markers,
        // all with the 1h TTL; newest (idx 2) stays unmarked.
        assert_eq!(count_breakpoints(&sent), 3);
        assert_eq!(
            sent["system"][0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"}),
        );
        assert_eq!(
            sent["messages"][0]["content"][0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"}),
        );
        assert_eq!(
            sent["messages"][1]["content"][0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"}),
        );
        assert!(
            sent["messages"][2]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    /// The SSE parser must emit the *normalized* input token count (uncached
    /// plus cache read plus cache write), not Anthropic's raw uncached-only
    /// figure, or downstream cost accounting clamps uncached input to zero.
    #[test]
    fn parse_usage_normalizes_input_tokens_to_include_cache() {
        let mut state = StreamState::default();
        let data = json!({
            "type": "message_start",
            "message": {
                "usage": {
                    "input_tokens": 2_800,
                    "output_tokens": 10,
                    "cache_read_input_tokens": 38_400,
                    "cache_creation_input_tokens": 0,
                }
            }
        })
        .to_string();

        let events = parse_anthropic_sse(&data, &mut state)
            .map(|e| vec![e])
            .unwrap_or_default();
        match &events[0] {
            StreamEvent::Usage {
                input_tokens,
                cache_hit_tokens,
                ..
            } => {
                assert_eq!(*input_tokens, 41_200, "must be the normalized total");
                assert_eq!(*cache_hit_tokens, 38_400);
                assert!(
                    *input_tokens >= *cache_hit_tokens,
                    "input must never be less than cache hits, or cost clamps to zero"
                );
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    /// Anthropic extended thinking: the opening content_block_start event
    /// for a `thinking` block carries the initial thinking text inline.
    /// Without this handling, the opening chunk is dropped on the floor and
    /// the chat pane stays blank during thinking.
    #[test]
    fn parse_anthropic_thinking_start_emits_reasoning_delta() {
        let mut state = StreamState::default();
        let data = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "thinking",
                "thinking": "Let me reason about this carefully..."
            }
        })
        .to_string();
        let events = parse_anthropic_sse(&data, &mut state)
            .map(|e| vec![e])
            .unwrap_or_default();
        assert_eq!(events.len(), 1, "one ReasoningDelta expected");
        match &events[0] {
            StreamEvent::ReasoningDelta(text) => {
                assert_eq!(text, "Let me reason about this carefully...");
            }
            other => panic!("expected ReasoningDelta, got {other:?}"),
        }
    }

    /// Anthropic extended thinking: subsequent `thinking_delta` deltas carry
    /// incremental thinking text. These must be emitted as ReasoningDelta so
    /// they flow to the dashboard's chat pane.
    #[test]
    fn parse_anthropic_thinking_delta_emits_reasoning_delta() {
        let mut state = StreamState::default();
        let data = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "thinking_delta",
                "thinking": " more reasoning follows."
            }
        })
        .to_string();
        let events = parse_anthropic_sse(&data, &mut state)
            .map(|e| vec![e])
            .unwrap_or_default();
        assert_eq!(events.len(), 1, "one ReasoningDelta expected");
        match &events[0] {
            StreamEvent::ReasoningDelta(text) => {
                assert_eq!(text, " more reasoning follows.");
            }
            other => panic!("expected ReasoningDelta, got {other:?}"),
        }
    }

    /// `signature_delta` (sent at the end of a thinking block) should be
    /// silently ignored — it's the cryptographic signature, not display text.
    #[test]
    fn parse_anthropic_signature_delta_is_ignored() {
        let mut state = StreamState::default();
        let data = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "signature_delta",
                "signature": "abc123"
            }
        })
        .to_string();
        let events = parse_anthropic_sse(&data, &mut state)
            .map(|e| vec![e])
            .unwrap_or_default();
        assert!(
            events.is_empty(),
            "signature_delta should produce no events, got {events:?}"
        );
    }

    /// Thinking block with empty initial text must not emit an empty
    /// ReasoningDelta (would show a blank line in the chat pane).
    #[test]
    fn parse_anthropic_thinking_start_empty_text_is_ignored() {
        let mut state = StreamState::default();
        let data = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "thinking",
                "thinking": ""
            }
        })
        .to_string();
        let events = parse_anthropic_sse(&data, &mut state)
            .map(|e| vec![e])
            .unwrap_or_default();
        assert!(
            events.is_empty(),
            "empty thinking block should produce no events, got {events:?}"
        );
    }
    /// Message content with the prompt-cache markers stripped.
    ///
    /// `apply_cache_breakpoints` puts a `cache_control` marker on the tail of
    /// messages `n-3` and `n-2`, so a body compared block by block against a
    /// journal entry has to ignore that one key — the conversion is what these
    /// tests are about, not the cache layout.
    fn content_without_cache(msg: &serde_json::Value) -> serde_json::Value {
        let mut content = msg["content"].clone();
        if let Some(blocks) = content.as_array_mut() {
            for block in blocks {
                if let Some(obj) = block.as_object_mut() {
                    obj.remove("cache_control");
                }
            }
        }
        content
    }

    /// A journal written on an OpenAI-family model is normalized before it
    /// reaches `/messages`: the `tool_calls` / `role:"tool"` turns become
    /// `tool_use` / `tool_result`. Before this, the API rejected the body
    /// with `messages.1 ... INVALID_REQUEST_BODY`.
    #[tokio::test]
    async fn complete_raw_normalizes_a_gpt_written_history() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
            )
            .mount(&server)
            .await;

        let raw = vec![
            json!({"role": "user", "content": "read file a"}),
            json!({
                "role": "assistant",
                "content": null,
                "reasoning_content": "the user wants a file read",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"a\"}"},
                }],
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "file body"}),
            json!({"role": "assistant", "content": "it says hi"}),
        ];

        let provider = AnthropicProvider::new(
            &server.uri(),
            "claude-test",
            Some("k"),
            None,
            false,
            None,
            false,
        )
        .unwrap();
        let stream = provider.complete_raw(&raw, &[], "").await.expect("stream");
        tokio::pin!(stream);
        while stream.next().await.is_some() {}

        let requests = server.received_requests().await.expect("requests recorded");
        let body = String::from_utf8(requests[0].body.clone()).expect("utf8 body");
        assert!(
            !body.contains("tool_calls") && !body.contains("reasoning_content"),
            "chat-only fields must not reach the Messages API: {body}"
        );

        let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
        let messages = sent["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 4, "{sent:#}");

        assert_eq!(messages[0]["role"], "user");
        assert_eq!(
            content_without_cache(&messages[1]),
            json!([{"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a"}}]),
            "the tool call travels as a tool_use block: {sent:#}"
        );
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(
            content_without_cache(&messages[2]),
            json!([{"type": "tool_result", "tool_use_id": "call_1", "content": "file body"}])
        );
        for msg in messages {
            let blocks = msg["content"].as_array().expect("content is a block array");
            assert!(!blocks.is_empty(), "empty content is rejected: {sent:#}");
        }
    }

    /// Two tool results in a row are one user turn on the wire — the
    /// Messages API requires the roles to alternate.
    #[test]
    fn two_tool_results_merge_into_one_user_turn() {
        let raw = vec![
            json!({"role": "user", "content": "run both"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "a", "arguments": "{}"}},
                    {"id": "c2", "type": "function", "function": {"name": "b", "arguments": "{}"}},
                ],
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "one"}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "two"}),
        ];
        let converted = super::super::chat_to_anthropic_messages(&raw);
        let merged = merge_consecutive_user_messages(converted);

        assert_eq!(
            merged.len(),
            3,
            "one user turn for both results: {merged:#?}"
        );
        let blocks = merged[2]["content"].as_array().expect("content blocks");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["tool_use_id"], "c1");
        assert_eq!(blocks[1]["tool_use_id"], "c2");
    }
    /// A journal that switched provider and stayed: the chat turns convert, the
    /// Claude turns keep their blocks — `thinking` and its signature included
    /// — so the two halves coexist on the wire.
    #[tokio::test]
    async fn complete_raw_keeps_a_mixed_journal_valid() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
            )
            .mount(&server)
            .await;

        let raw = vec![
            json!({"role": "user", "content": "read file a"}),
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"a\"}"},
                }],
            }),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "file body"}),
            json!({"role": "assistant", "content": "it says hi"}),
            json!({"role": "user", "content": "and now write b"}),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "write it", "signature": "sig-1"},
                {"type": "text", "text": "writing"},
                {"type": "tool_use", "id": "tu_9", "name": "write", "input": {"path": "b"}},
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_9", "content": "written"},
            ]}),
        ];

        let provider = AnthropicProvider::new(
            &server.uri(),
            "claude-test",
            Some("k"),
            None,
            false,
            None,
            false,
        )
        .unwrap();
        let stream = provider.complete_raw(&raw, &[], "").await.expect("stream");
        tokio::pin!(stream);
        while stream.next().await.is_some() {}

        let requests = server.received_requests().await.expect("requests recorded");
        let body = String::from_utf8(requests[0].body.clone()).expect("utf8 body");
        let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
        let messages = sent["messages"].as_array().expect("messages array");

        assert_eq!(messages.len(), 7, "{sent:#}");
        assert!(!body.contains("tool_calls"), "{body}");
        assert_eq!(
            content_without_cache(&messages[1]),
            json!([{"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a"}}])
        );
        assert_eq!(
            content_without_cache(&messages[5]),
            raw[5]["content"],
            "a Claude-written turn travels as recorded: {sent:#}"
        );
        assert_eq!(content_without_cache(&messages[6]), raw[6]["content"]);
    }
}
