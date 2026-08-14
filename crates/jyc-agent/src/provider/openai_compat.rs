//! OpenAI-compatible Chat Completions API provider.
//!
//! Supports any endpoint implementing the OpenAI `/chat/completions` API.
//! Covers: DeepSeek, GPT, Groq, Together AI, etc.

use super::sse::{Event, stream_sse};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json;
use std::collections::VecDeque;

use crate::provider::usage::extract_cache_hit_tokens;
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
        crate::provider::merge_params(&mut body, &self.params);

        // Build request
        let req = self
            .client
            .post(&url)
            .header("content-type", "application/json");
        let req = self.apply_headers(req).json(&body);

        tracing::debug!(url = %url, model = %self.model, "Sending OpenAI-compatible request");

        // Use the in-process SSE stream (same as Anthropic provider)
        let es = stream_sse(req);

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
                                // Flush any remaining tag-buffered content
                                // (e.g. response text after </think> that
                                // didn't fit into a complete chunk boundary).
                                if let Some(event) = flush_think_buffer(&mut state) {
                                    state.pending_events.push_back(event);
                                }
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
                                // Drain remaining events, flushing any
                                // tag-buffered tail so we don't lose the
                                // last few characters of the response.
                                if let Some(event) = flush_think_buffer(&mut state) {
                                    state.pending_events.push_back(event);
                                }
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
                            // Drain remaining events, flushing any
                            // tag-buffered tail so we don't lose the
                            // last few characters of the response.
                            if let Some(event) = flush_think_buffer(&mut state) {
                                state.pending_events.push_back(event);
                            }
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

        // Content: when both reasoning and response text are present, inline
        // the reasoning as `<think>...</think>` tags so providers that expect
        // thinking inline (e.g. MiniMax M3) receive it in the right shape on
        // replay. Providers that use a separate `reasoning_content` field
        // (e.g. DeepSeek) still get the field below, so this is additive and
        // does not break them.
        if !reasoning.is_empty() && !text.is_empty() {
            msg["content"] =
                serde_json::Value::String(format!("<think>{reasoning}</think>\n\n{text}"));
        } else if !text.is_empty() {
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
                    // Validate arguments JSON before embedding. Some models
                    // (e.g. MiniMax M3) occasionally emit malformed tool call
                    // arguments that survive the current turn but poison the
                    // `raw_context` — replaying them on the next request
                    // triggers a 400 with "invalid function arguments json
                    // string". Fall back to "{}" to match the Anthropic path
                    // and keep the conversation consistent.
                    let safe_args = if serde_json::from_str::<serde_json::Value>(args).is_ok() {
                        args.clone()
                    } else {
                        tracing::warn!(
                            tool_name = %name,
                            args = %args,
                            "Malformed tool call arguments from model, replacing with empty object for replay"
                        );
                        "{}".to_string()
                    };
                    serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": safe_args,
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
        crate::provider::merge_params(&mut body, &self.params);

        // Build and send request
        let req = self
            .client
            .post(&url)
            .header("content-type", "application/json");
        let req = self.apply_headers(req).json(&body);

        tracing::debug!(url = %url, model = %self.model, "Sending OpenAI-compatible request");

        let es = stream_sse(req);

        let stream = futures::stream::unfold(
            (es, OpenAiStreamState::default()),
            |(mut es, mut state)| async move {
                loop {
                    if let Some(event) = state.pending_events.pop_front() {
                        return Some((Ok(event), (es, state)));
                    }

                    match es.next().await {
                        Some(Ok(Event::Open)) => continue,
                        Some(Ok(Event::Message(msg))) => {
                            let data = &msg.data;
                            if data.trim() == "[DONE]" {
                                if let Some(event) = flush_think_buffer(&mut state) {
                                    state.pending_events.push_back(event);
                                }
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
                        }
                        Some(Err(e)) => {
                            let err_msg = format!("{e}");
                            if err_msg.contains("Stream ended") {
                                if let Some(event) = flush_think_buffer(&mut state) {
                                    state.pending_events.push_back(event);
                                }
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
                            if let Some(event) = flush_think_buffer(&mut state) {
                                state.pending_events.push_back(event);
                            }
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

/// Internal state for OpenAI stream parsing.
#[derive(Default)]
struct OpenAiStreamState {
    /// Tool calls being assembled from deltas.
    tool_calls: Vec<ToolCallAccumulator>,
    /// Events ready to be yielded (FIFO queue).
    pending_events: VecDeque<StreamEvent>,
    /// Whether the parser is currently inside a `<think>...</think>` block.
    /// Providers like MiniMax M3 emit thinking content inline in the `content`
    /// field wrapped in `<think>...</think>` tags rather than in a separate
    /// `reasoning_content` field.
    in_think_block: bool,
    /// Unparsed tail of the `content` stream. Holds the substring from the last
    /// `<` onwards when no complete tag is present, so we can detect tags that
    /// arrive split across chunks.
    tag_buffer: String,
    /// When `</think>` is consumed but the `\n\n` separator that MiniMax M3
    /// emits after it is not present in the same chunk, this flag is set so
    /// the next chunk's leading `\n\n` (if any) is stripped before being
    /// emitted as `TextDelta`.
    pending_strip_newlines: bool,
}

#[derive(Default, Clone)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

/// Split the incoming `content` fragment by `<think>` / `</think>` tags,
/// emitting `TextDelta` for text outside think blocks and `ReasoningDelta`
/// for text inside them. Handles tags split across chunks by buffering the
/// last unparsed `<` onwards in `state.tag_buffer`.
///
/// When a `</think>` tag is followed by `\n\n`, the newlines are consumed as
/// a separator and not emitted — this matches MiniMax M3's output format.
///
/// **Known limitation:** if the model's response text contains the literal
/// substring `<think>` outside a think block (e.g. the user asked "what does
/// `<think>` mean?"), the parser will incorrectly treat subsequent text as
/// reasoning. This is inherent to stateless tag parsing without context
/// awareness and is acceptable for the current scope — in practice models do
/// not emit `<think>` as literal text in their responses.
fn split_think_tags(content: &str, state: &mut OpenAiStreamState) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let mut text = std::mem::take(&mut state.tag_buffer) + content;

    // If a previous chunk ended with `</think>` and the `\n\n` separator
    // wasn't present, strip it from this chunk's leading bytes if present.
    if state.pending_strip_newlines {
        state.pending_strip_newlines = false;
        if let Some(stripped) = text.strip_prefix("\n\n") {
            text = stripped.to_string();
        }
    }

    loop {
        let tag = if state.in_think_block {
            "</think>"
        } else {
            "<think>"
        };

        match text.find(tag) {
            Some(pos) => {
                let segment = &text[..pos];
                if !segment.is_empty() {
                    events.push(if state.in_think_block {
                        StreamEvent::ReasoningDelta(segment.to_string())
                    } else {
                        StreamEvent::TextDelta(segment.to_string())
                    });
                }
                text = text[pos + tag.len()..].to_string();
                state.in_think_block = !state.in_think_block;
                if !state.in_think_block {
                    // Strip the "\n\n" separator that MiniMax M3 emits between
                    // the think block and the actual response. If it isn't in
                    // the same chunk, defer the strip to the next chunk.
                    if let Some(stripped) = text.strip_prefix("\n\n") {
                        text = stripped.to_string();
                    } else {
                        state.pending_strip_newlines = true;
                    }
                }
            }
            None => {
                // No complete tag found. Buffer everything from the last `<`
                // onwards — that substring could still become a tag once the
                // next chunk arrives. Text before that point is safe to emit.
                if let Some(last_angle) = text.rfind('<') {
                    let emit = &text[..last_angle];
                    if !emit.is_empty() {
                        events.push(if state.in_think_block {
                            StreamEvent::ReasoningDelta(emit.to_string())
                        } else {
                            StreamEvent::TextDelta(emit.to_string())
                        });
                    }
                    state.tag_buffer = text[last_angle..].to_string();
                } else {
                    if !text.is_empty() {
                        events.push(if state.in_think_block {
                            StreamEvent::ReasoningDelta(text.clone())
                        } else {
                            StreamEvent::TextDelta(text.clone())
                        });
                    }
                    state.tag_buffer.clear();
                }
                break;
            }
        }
    }

    events
}

/// Flush any remaining buffered content from `split_think_tags`. Called when
/// the stream ends (SSE `[DONE]`) to ensure the last partial chunk is not lost.
fn flush_think_buffer(state: &mut OpenAiStreamState) -> Option<StreamEvent> {
    if state.tag_buffer.is_empty() {
        return None;
    }
    let buf = std::mem::take(&mut state.tag_buffer);
    Some(if state.in_think_block {
        StreamEvent::ReasoningDelta(buf)
    } else {
        StreamEvent::TextDelta(buf)
    })
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
        //
        // Some providers (e.g. MiniMax M3) embed thinking content inline in
        // the `content` field wrapped in `<think>...</think>` tags. Route the
        // content through `split_think_tags` so thinking and response text are
        // emitted as separate event types.
        if let Some(content) = delta.get("content").and_then(|c| c.as_str())
            && !content.is_empty()
        {
            events.extend(split_think_tags(content, state));
        }

        // Reasoning content (DeepSeek v4-pro style thinking)
        //
        // Providers like DeepSeek emit thinking in a separate `reasoning_content`
        // field, which is independent of any `<think>` tags in `content`.
        if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str())
            && !reasoning.is_empty()
        {
            events.push(StreamEvent::ReasoningDelta(reasoning.to_string()));
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

        // Check finish_reason and emit ToolUseEnd for each accumulated tool
        // call. This MUST run after the tool_calls delta processing above:
        // some providers (notably MiniMax M3) send the final argument
        // fragment in the same SSE chunk as `finish_reason: "tool_calls"`.
        // Processing finish_reason first would clear `state.tool_calls` and
        // emit `ToolUseEnd` before the last fragment is accumulated,
        // truncating the arguments (e.g., the `file_path` parameter at the
        // tail of a `read` tool call's JSON would be lost).
        if let Some(finish_reason) = choice.get("finish_reason").and_then(|f| f.as_str())
            && (finish_reason == "tool_calls" || finish_reason == "stop")
        {
            // Emit ToolUseEnd for each accumulated tool call
            for _ in &state.tool_calls {
                events.push(StreamEvent::ToolUseEnd);
            }
            state.tool_calls.clear();
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
            let cache_hit = extract_cache_hit_tokens(usage);
            events.push(StreamEvent::Usage {
                input_tokens: input,
                output_tokens: output,
                cache_hit_tokens: cache_hit,
                // OpenAI-compat providers (OpenAI / DeepSeek / Kimi /
                // 火山引擎 / MiniMax) surface only a single cache
                // bucket; no creation/write field exists for them.
                cache_creation_tokens: 0,
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
    async fn sse_error_embeds_response_body_on_4xx() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("user-agent", "opencode/1.15.13"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": { "message": "bad request", "type": "invalid_request_error" }
            })))
            .expect(1) // only the SSE POST — the diagnostic re-POST is gone
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

        let mut found: Option<anyhow::Error> = None;
        while let Some(item) = stream.next().await {
            if let Err(e) = item {
                found = Some(e);
                break;
            }
        }

        let err = found.expect("expected an error from the SSE stream after 4xx");
        let msg = format!("{:#}", err);
        assert!(msg.contains("400"), "expected status code, got: {msg}");
        assert!(
            msg.contains("bad request"),
            "expected embedded response body, got: {msg}"
        );

        // wiremock verifies at drop that exactly 1 request was made — no
        // diagnostic re-POST (the SSE client embeds the body directly).
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

    /// Regression test: MiniMax M3 sometimes sends the final argument fragment
    /// in the same SSE chunk as `finish_reason: "tool_calls"`. The parser must
    /// accumulate the fragment first, then emit `ToolUseEnd` — otherwise the
    /// tail of the arguments JSON (e.g., the `file_path` parameter of a `read`
    /// tool call) is truncated and the tool call fails with "Missing
    /// 'file_path' parameter".
    #[test]
    fn parse_tool_call_finish_reason_with_last_fragment() {
        let mut state = OpenAiStreamState::default();

        // Chunk 1: id + name + first fragment of arguments
        let chunk1 = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": {
                            "name": "read",
                            "arguments": r#"{"file_path":"/foo/bar""#
                        }
                    }]
                }
            }]
        })
        .to_string();
        let events1 = parse_openai_chunk(&chunk1, &mut state).expect("should parse chunk 1");
        // Expect ToolUseStart + ToolInputDelta
        assert!(
            events1
                .iter()
                .any(|e| matches!(e, StreamEvent::ToolUseStart { name, .. } if name == "read")),
            "expected ToolUseStart for 'read', got {events1:?}"
        );
        assert!(
            !events1.iter().any(|e| matches!(e, StreamEvent::ToolUseEnd)),
            "no ToolUseEnd should fire yet"
        );
        assert_eq!(state.tool_calls.len(), 1);
        assert_eq!(state.tool_calls[0].arguments, r#"{"file_path":"/foo/bar""#);

        // Chunk 2: last fragment of arguments + finish_reason="tool_calls"
        // in the same SSE message. This is the MiniMax M3 quirk.
        let chunk2 = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {
                            "arguments": "}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
        .to_string();
        let events2 = parse_openai_chunk(&chunk2, &mut state).expect("should parse chunk 2");

        // ToolInputDelta must come BEFORE ToolUseEnd so the agent loop appends
        // the fragment to the accumulator before saving the tool call.
        let delta_pos = events2
            .iter()
            .position(|e| matches!(e, StreamEvent::ToolInputDelta(_)));
        let end_pos = events2
            .iter()
            .position(|e| matches!(e, StreamEvent::ToolUseEnd));
        assert!(
            delta_pos.is_some() && end_pos.is_some() && delta_pos.unwrap() < end_pos.unwrap(),
            "ToolInputDelta must precede ToolUseEnd so the final fragment is \
             accumulated before the tool call is finalized; got events={events2:?}"
        );

        // State must reflect the complete arguments — the `}` fragment must
        // be present, and the accumulator must be cleared after finish_reason.
        assert_eq!(
            state.tool_calls.len(),
            0,
            "state cleared after finish_reason"
        );
    }

    /// Regression test: standard OpenAI streaming where the final chunk has
    /// `finish_reason: "tool_calls"` but no `tool_calls` delta. The reorder
    /// (process tool_calls delta before finish_reason) must not change
    /// behavior for this case — `ToolUseEnd` still fires once.
    #[test]
    fn parse_tool_call_finish_reason_without_arguments() {
        let mut state = OpenAiStreamState::default();

        let chunk1 = serde_json::json!({
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
        parse_openai_chunk(&chunk1, &mut state);

        let chunk2 = serde_json::json!({
            "choices": [{
                "delta": {},
                "finish_reason": "tool_calls"
            }]
        })
        .to_string();
        let events = parse_openai_chunk(&chunk2, &mut state).expect("should parse");

        // Exactly one ToolUseEnd should fire (from finish_reason), no
        // ToolInputDelta (no arguments in this chunk).
        let end_count = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolUseEnd))
            .count();
        let delta_count = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolInputDelta(_)))
            .count();
        assert_eq!(end_count, 1, "expected exactly one ToolUseEnd");
        assert_eq!(delta_count, 0, "no ToolInputDelta expected");
        assert!(
            state.tool_calls.is_empty(),
            "state cleared after finish_reason"
        );
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

    // Helper to extract just the string content from a sequence of events.
    fn texts(events: &[StreamEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn reasonings(events: &[StreamEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ReasoningDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    /// MiniMax M3 streaming layout (real captured example):
    ///   chunk 1: role only
    ///   chunk 2: <think>... (thinking begins)
    ///   chunk 3: ...rest of thinking</think>\n\nFour
    /// The thinking must not leak into the reply text.
    #[test]
    fn split_think_tags_minimax_layout() {
        let mut state = OpenAiStreamState::default();

        // chunk 1: role only — no events
        let chunk1 = serde_json::json!({"choices": [{"delta": {"role": "assistant"}}]}).to_string();
        let events = parse_openai_chunk(&chunk1, &mut state).unwrap_or_default();
        assert!(texts(&events).is_empty() && reasonings(&events).is_empty());

        // chunk 2: thinking content opens
        let chunk2 = serde_json::json!({
            "choices": [{"delta": {"content": "<think>The user asks 2+2. Answer is Four.", "role": "assistant"}}]
        })
        .to_string();
        let events = parse_openai_chunk(&chunk2, &mut state).unwrap_or_default();
        assert!(
            texts(&events).is_empty(),
            "no text yet, got {:?}",
            texts(&events)
        );
        assert_eq!(
            reasonings(&events),
            vec!["The user asks 2+2. Answer is Four."],
            "thinking should be ReasoningDelta"
        );
        assert!(state.in_think_block);

        // chunk 3: think block closes, response follows (note "\n\n" separator)
        let chunk3 = serde_json::json!({
            "choices": [{"delta": {"content": "</think>\n\nFour", "role": "assistant"}}]
        })
        .to_string();
        let events = parse_openai_chunk(&chunk3, &mut state).unwrap_or_default();
        assert_eq!(
            texts(&events),
            vec!["Four"],
            "response should be plain TextDelta with no think tags or leading newlines"
        );
        assert!(
            reasonings(&events).is_empty(),
            "no reasoning leaked into chunk 3"
        );
        assert!(!state.in_think_block, "should have exited think block");
    }

    /// When the content has neither think tags nor `<` chars, the existing
    /// OpenAI behavior must be preserved (entire content -> TextDelta).
    #[test]
    fn split_think_tags_plain_text_passthrough() {
        let mut state = OpenAiStreamState::default();
        let chunk = serde_json::json!({
            "choices": [{"delta": {"content": "Hello, world!"}}]
        })
        .to_string();
        let events = parse_openai_chunk(&chunk, &mut state).unwrap_or_default();
        assert_eq!(texts(&events), vec!["Hello, world!"]);
        assert!(reasonings(&events).is_empty());
        assert!(state.tag_buffer.is_empty());
    }

    /// Tags may straddle chunk boundaries. The partial "<thi" must be held
    /// back until the next chunk completes the `<think>` token.
    #[test]
    fn split_think_tags_split_across_chunks() {
        let mut state = OpenAiStreamState::default();

        let chunk1 = serde_json::json!({"choices": [{"delta": {"content": "<thi"}}]}).to_string();
        let events = parse_openai_chunk(&chunk1, &mut state).unwrap_or_default();
        assert!(
            events.is_empty(),
            "partial tag must be buffered, not emitted"
        );
        assert_eq!(state.tag_buffer, "<thi");
        assert!(!state.in_think_block);

        let chunk2 =
            serde_json::json!({"choices": [{"delta": {"content": "nk>thinking"}}]}).to_string();
        let events = parse_openai_chunk(&chunk2, &mut state).unwrap_or_default();
        assert_eq!(
            reasonings(&events),
            vec!["thinking"],
            "thinking content emitted after buffered tag completes"
        );
        assert!(state.in_think_block);
        assert!(state.tag_buffer.is_empty());
    }

    /// The `\n\n` separator MiniMax emits after `</think>` must be stripped
    /// from the response text — otherwise the user sees a leading blank line.
    #[test]
    fn split_think_tags_strips_separator_after_close() {
        let mut state = OpenAiStreamState::default();
        // pre-seed as if we are mid-think
        let chunk1 =
            serde_json::json!({"choices": [{"delta": {"content": "<think>stuff"}}]}).to_string();
        parse_openai_chunk(&chunk1, &mut state).unwrap_or_default();

        let chunk2 = serde_json::json!({"choices": [{"delta": {"content": "</think>\n\nAnswer"}}]})
            .to_string();
        let events = parse_openai_chunk(&chunk2, &mut state).unwrap_or_default();
        assert_eq!(texts(&events), vec!["Answer"]);
    }

    /// Text containing a bare `<` (not a think tag) is still emitted promptly
    /// rather than buffered forever. Only the suffix after the last `<` is
    /// held back.
    #[test]
    fn split_think_tags_buffer_only_after_last_angle_bracket() {
        let mut state = OpenAiStreamState::default();
        let chunk =
            serde_json::json!({"choices": [{"delta": {"content": "Hello <world"}}]}).to_string();
        let events = parse_openai_chunk(&chunk, &mut state).unwrap_or_default();
        assert_eq!(texts(&events), vec!["Hello "]);
        assert_eq!(state.tag_buffer, "<world");
    }

    /// When the stream ends with content still in the tag buffer, the flush
    /// helper must surface it so it isn't dropped on the floor.
    #[test]
    fn flush_think_buffer_emits_text_when_outside_think() {
        let mut state = OpenAiStreamState {
            tag_buffer: "trailing".to_string(),
            ..Default::default()
        };
        let event = flush_think_buffer(&mut state).expect("should produce event");
        match event {
            StreamEvent::TextDelta(t) => assert_eq!(t, "trailing"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
        assert!(state.tag_buffer.is_empty());
    }

    #[test]
    fn flush_think_buffer_emits_reasoning_when_inside_think() {
        let mut state = OpenAiStreamState {
            in_think_block: true,
            tag_buffer: "still thinking".to_string(),
            ..Default::default()
        };
        let event = flush_think_buffer(&mut state).expect("should produce event");
        match event {
            StreamEvent::ReasoningDelta(t) => assert_eq!(t, "still thinking"),
            other => panic!("expected ReasoningDelta, got {other:?}"),
        }
        assert!(state.tag_buffer.is_empty());
    }

    #[test]
    fn flush_think_buffer_noop_when_empty() {
        let mut state = OpenAiStreamState::default();
        assert!(flush_think_buffer(&mut state).is_none());
    }

    /// DeepSeek-style `reasoning_content` must still work alongside the new
    /// `<think>` tag handling — they are independent code paths.
    #[test]
    fn parse_openai_chunk_combines_reasoning_content_and_content() {
        let mut state = OpenAiStreamState::default();
        let chunk = serde_json::json!({
            "choices": [{
                "delta": {
                    "content": "hello",
                    "reasoning_content": "thinking..."
                }
            }]
        })
        .to_string();
        let events = parse_openai_chunk(&chunk, &mut state).unwrap_or_default();
        assert_eq!(texts(&events), vec!["hello"]);
        assert_eq!(reasonings(&events), vec!["thinking..."]);
    }

    /// When `</think>` arrives in one chunk and the `\n\n` separator in the
    /// next, the separator must still be stripped so the reply does not start
    /// with a blank line. Regression test for the cross-chunk separator case.
    #[test]
    fn split_think_tags_separator_split_across_chunks() {
        let mut state = OpenAiStreamState::default();
        // pre-seed as if we are mid-think
        let chunk1 =
            serde_json::json!({"choices": [{"delta": {"content": "<think>thinking"}}]}).to_string();
        parse_openai_chunk(&chunk1, &mut state).unwrap_or_default();

        // </think> alone — no separator in this chunk
        let chunk2 =
            serde_json::json!({"choices": [{"delta": {"content": "</think>"}}]}).to_string();
        let events = parse_openai_chunk(&chunk2, &mut state).unwrap_or_default();
        assert!(events.is_empty(), "no events after closing tag alone");
        assert!(state.pending_strip_newlines);

        // separator + response in the next chunk
        let chunk3 =
            serde_json::json!({"choices": [{"delta": {"content": "\n\nAnswer"}}]}).to_string();
        let events = parse_openai_chunk(&chunk3, &mut state).unwrap_or_default();
        assert_eq!(
            texts(&events),
            vec!["Answer"],
            "leading \\n\\n must be stripped even when it arrives in a separate chunk"
        );
        assert!(!state.pending_strip_newlines);
    }

    /// Multiple `<think>` blocks in one response must each be parsed
    /// independently — the loop must not get stuck after the first close.
    #[test]
    fn split_think_tags_multiple_think_blocks() {
        let mut state = OpenAiStreamState::default();
        let chunk = serde_json::json!({
            "choices": [{"delta": {"content": "<think>a</think>middle<think>b</think>end"}}]
        })
        .to_string();
        let events = parse_openai_chunk(&chunk, &mut state).unwrap_or_default();

        let r = reasonings(&events);
        assert_eq!(r, vec!["a", "b"], "two separate reasoning segments");
        let t = texts(&events);
        assert_eq!(t, vec!["middle", "end"], "two separate text segments");
        assert!(!state.in_think_block);
        assert!(state.tag_buffer.is_empty());
    }

    /// `build_raw_assistant_message` must inline reasoning as `<think>` tags
    /// in `content` so providers like MiniMax M3 receive it in the shape they
    /// expect on replay, while still emitting `reasoning_content` for providers
    /// like DeepSeek that use a separate field.
    #[test]
    fn build_raw_assistant_message_inlines_think_tags() {
        let provider = OpenAiCompatProvider::new(
            "https://api.example.com",
            "test",
            Some("k"),
            None,
            false,
            None,
        )
        .unwrap();
        let msg = provider.build_raw_assistant_message("the answer", "the thinking", &[]);
        let content = msg["content"].as_str().expect("content should be string");
        assert_eq!(content, "<think>the thinking</think>\n\nthe answer");
        let reasoning = msg["reasoning_content"]
            .as_str()
            .expect("reasoning_content set");
        assert_eq!(reasoning, "the thinking");
    }

    /// Without reasoning, `content` should stay a plain string (no tag wrapping).
    #[test]
    fn build_raw_assistant_message_plain_text_when_no_reasoning() {
        let provider = OpenAiCompatProvider::new(
            "https://api.example.com",
            "test",
            Some("k"),
            None,
            false,
            None,
        )
        .unwrap();
        let msg = provider.build_raw_assistant_message("just text", "", &[]);
        assert_eq!(msg["content"].as_str().unwrap(), "just text");
        assert!(msg.get("reasoning_content").is_none());
    }

    /// Valid JSON arguments pass through untouched.
    #[test]
    fn build_raw_assistant_message_valid_args_passthrough() {
        let provider = OpenAiCompatProvider::new(
            "https://api.example.com",
            "test",
            Some("k"),
            None,
            false,
            None,
        )
        .unwrap();
        let calls = vec![(
            "call_1".to_string(),
            "bash".to_string(),
            r#"{"command": "ls -la"}"#.to_string(),
        )];
        let msg = provider.build_raw_assistant_message("", "", &calls);
        let args = msg["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments is a JSON string");
        assert_eq!(args, r#"{"command": "ls -la"}"#);
    }

    /// Malformed JSON arguments from the model (e.g. truncated stream from
    /// MiniMax M3) are replaced with `"{}"` so replaying the conversation
    /// context does not trigger a 400 "invalid function arguments json
    /// string" on the next API call.
    #[test]
    fn build_raw_assistant_message_replaces_malformed_args() {
        let provider = OpenAiCompatProvider::new(
            "https://api.example.com",
            "test",
            Some("k"),
            None,
            false,
            None,
        )
        .unwrap();
        // Truncated mid-stream: missing closing brace.
        let calls = vec![(
            "call_1".to_string(),
            "bash".to_string(),
            r#"{"command": "ls"#.to_string(),
        )];
        let msg = provider.build_raw_assistant_message("", "", &calls);
        let args = msg["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments is a JSON string");
        assert_eq!(
            args, "{}",
            "malformed args must be replaced with empty object to avoid 400 on replay"
        );
    }

    /// Whitespace-only / empty argument strings are also invalid JSON and
    /// should fall back to `"{}"`.
    #[test]
    fn build_raw_assistant_message_replaces_empty_args() {
        let provider = OpenAiCompatProvider::new(
            "https://api.example.com",
            "test",
            Some("k"),
            None,
            false,
            None,
        )
        .unwrap();
        let calls = vec![("call_1".to_string(), "bash".to_string(), "   ".to_string())];
        let msg = provider.build_raw_assistant_message("", "", &calls);
        let args = msg["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments is a JSON string");
        assert_eq!(args, "{}");
    }
}
