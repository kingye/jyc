use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use super::types::*;
use crate::utils::constants::*;

/// OpenCode HTTP + SSE client.
///
/// Wraps all HTTP calls to the OpenCode server and provides
/// SSE event streaming with activity-based timeout.
pub struct OpenCodeClient {
    http: reqwest::Client,
    base_url: String,
}

impl OpenCodeClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.to_string(),
        }
    }

    /// Build a request with the x-opencode-directory header.
    fn directory_header(directory: &Path) -> (&'static str, String) {
        (
            "x-opencode-directory",
            urlencoding_encode(&directory.to_string_lossy()),
        )
    }

    // --- Session API ---

    /// Create a new session.
    pub async fn create_session(
        &self,
        directory: &Path,
        title: &str,
    ) -> Result<SessionInfo> {
        let url = format!("{}/session", self.base_url);
        let (hdr_name, hdr_val) = Self::directory_header(directory);

        let body = CreateSessionRequest {
            title: title.to_string(),
        };

        let resp = self
            .http
            .post(&url)
            .header(hdr_name, &hdr_val)
            .json(&body)
            .send()
            .await
            .context("create_session request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("create_session failed ({}): {}", status, body);
        }

        let session: SessionInfo = resp.json().await.context("parse session response")?;
        tracing::debug!(session_id = %session.id, "Session created");
        Ok(session)
    }

    /// Get a session by ID. Returns None if not found.
    pub async fn get_session(&self, session_id: &str) -> Result<Option<SessionInfo>> {
        let url = format!("{}/session/{}", self.base_url, session_id);

        let resp = self
            .http
            .get(&url)
            .timeout(OPENCODE_HEALTH_CHECK_TIMEOUT)
            .send()
            .await
            .context("get_session request failed")?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("get_session failed ({}): {}", status, body);
        }

        let session: SessionInfo = resp.json().await.context("parse session response")?;
        Ok(Some(session))
    }

    // --- Prompt API ---

    /// Send an async prompt (returns immediately, results via SSE).
    pub async fn prompt_async(
        &self,
        session_id: &str,
        directory: &Path,
        request: &PromptRequest,
    ) -> Result<()> {
        let url = format!(
            "{}/session/{}/prompt_async",
            self.base_url, session_id
        );
        let (hdr_name, hdr_val) = Self::directory_header(directory);

        let resp = self
            .http
            .post(&url)
            .header(hdr_name, &hdr_val)
            .query(&[("directory", &directory.to_string_lossy().to_string())])
            .json(request)
            .send()
            .await
            .context("prompt_async request failed")?;

        let status = resp.status();

        if !status.is_success() && status != reqwest::StatusCode::NO_CONTENT {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("prompt_async failed ({}): {}", status, body);
        }

        tracing::debug!(status = %status, "prompt_async accepted");

        Ok(())
    }

    /// Send a blocking prompt (waits for completion).
    pub async fn prompt_blocking(
        &self,
        session_id: &str,
        directory: &Path,
        request: &PromptRequest,
    ) -> Result<PromptResponse> {
        let url = format!(
            "{}/session/{}/message",
            self.base_url, session_id
        );
        let (hdr_name, hdr_val) = Self::directory_header(directory);

        let resp = self
            .http
            .post(&url)
            .header(hdr_name, &hdr_val)
            .json(request)
            .timeout(BLOCKING_PROMPT_TIMEOUT)
            .send()
            .await
            .context("prompt_blocking request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("prompt_blocking failed ({}): {}", status, body);
        }

        let result: PromptResponse = resp.json().await.context("parse prompt response")?;
        Ok(result)
    }

    // --- SSE Streaming ---

    /// Process a prompt via SSE streaming with activity-based timeout.
    ///
    /// 1. Subscribe to SSE events
    /// 2. Fire async prompt
    /// 3. Process events until session.idle or timeout
    /// 4. Return accumulated result
    pub async fn prompt_with_sse(
        &self,
        session_id: &str,
        directory: &Path,
        request: &PromptRequest,
    ) -> Result<SseResult> {
        // 1. Subscribe to SSE events scoped to the thread directory
        let sse_url = format!(
            "{}/event?directory={}",
            self.base_url,
            urlencoding_encode(&directory.to_string_lossy())
        );
        let (hdr_name, hdr_val) = Self::directory_header(directory);
        let req = self.http.get(&sse_url).header(hdr_name, &hdr_val);
        let mut es = EventSource::new(req)
            .map_err(|e| anyhow::anyhow!("SSE subscription failed: {e}"))?;

        // 2. Fire async prompt
        self.prompt_async(session_id, directory, request).await?;

        // 3. Process SSE events
        let mut result = SseResult::default();
        let mut parts: HashMap<String, ResponsePart> = HashMap::new();
        let mut last_activity = Instant::now();
        let mut last_progress_log = Instant::now();
        let mut last_tool_name: Option<String> = None;
        let mut last_status_type: Option<String> = None;
        let start_time = Instant::now();
        let mut done = false;

        let mut check_interval = tokio::time::interval(ACTIVITY_CHECK_INTERVAL);

        loop {
            if done {
                break;
            }

            tokio::select! {
                event = es.next() => {
                    match event {
                        Some(Ok(Event::Open)) => {
                            tracing::debug!("SSE stream opened");
                        }
                        Some(Ok(Event::Message(msg))) => {
                            let sse_event_field = &msg.event;
                            let data = &msg.data;

                            // Parse JSON data
                            let parsed: serde_json::Value = serde_json::from_str(data)
                                .unwrap_or(serde_json::Value::Object(Default::default()));

                            // Determine the actual event type:
                            // Some SSE servers put the type in the SSE `event:` field,
                            // others put it inside the JSON `type` field with `properties`.
                            let (event_type, properties) = if sse_event_field != "message"
                                && !sse_event_field.is_empty()
                            {
                                // Type is in the SSE event field, data is the properties
                                (sse_event_field.clone(), parsed)
                            } else if let Some(t) = parsed.get("type").and_then(|v| v.as_str()) {
                                // Type is inside the JSON data
                                let props = parsed
                                    .get("properties")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Object(Default::default()));
                                (t.to_string(), props)
                            } else {
                                tracing::trace!(
                                    sse_event = %sse_event_field,
                                    data = %data,
                                    "SSE event with unknown structure"
                                );
                                continue;
                            };

                            let sse_event = SseEvent {
                                event_type: event_type.clone(),
                                properties,
                            };

                            let event_result = self.handle_sse_event(
                                    &sse_event,
                                    session_id,
                                    &mut parts,
                                    &mut result,
                                    &mut last_activity,
                                    &mut last_tool_name,
                                    &mut last_status_type,
                                );

                                match event_result {
                                    SseAction::Continue => {}
                                    SseAction::Done => { done = true; }
                                    SseAction::Error(err) => {
                                        result.error = Some(err);
                                        done = true;
                                    }
                                }
                        }
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "SSE stream error");
                            done = true;
                        }
                        None => {
                            tracing::debug!("SSE stream ended");
                            done = true;
                        }
                    }
                }

                _ = check_interval.tick() => {
                    let elapsed = start_time.elapsed();
                    let silence = last_activity.elapsed();

                    // Activity-based timeout
                    let timeout = if last_tool_name.is_some() {
                        TOOL_ACTIVITY_TIMEOUT
                    } else {
                        ACTIVITY_TIMEOUT
                    };

                    if silence > timeout {
                        tracing::warn!(
                            silence_secs = silence.as_secs(),
                            timeout_secs = timeout.as_secs(),
                            tool = ?last_tool_name,
                            "Activity timeout"
                        );
                        result.timed_out = true;
                        done = true;
                        continue;
                    }

                    // Progress logging
                    if last_progress_log.elapsed() >= PROGRESS_LOG_INTERVAL {
                        let activity = last_tool_name
                            .as_deref()
                            .unwrap_or("generating");

                        tracing::info!(
                            elapsed_secs = elapsed.as_secs(),
                            parts = parts.len(),
                            model = ?result.model_id,
                            activity = %activity,
                            silence_secs = silence.as_secs(),
                            "Progress"
                        );

                        // TODO: on_progress callback (Phase 6)

                        last_progress_log = Instant::now();
                    }
                }
            }
        }

        // Close SSE stream
        es.close();

        // Collect accumulated parts into result
        result.parts = parts.into_values().collect();

        // Check if reply_message tool was used
        result.reply_sent_by_tool = check_tool_used(&result.parts);

        Ok(result)
    }

    /// Handle a single SSE event. Returns action to take.
    fn handle_sse_event(
        &self,
        event: &SseEvent,
        session_id: &str,
        parts: &mut HashMap<String, ResponsePart>,
        result: &mut SseResult,
        last_activity: &mut Instant,
        last_tool_name: &mut Option<String>,
        last_status_type: &mut Option<String>,
    ) -> SseAction {
        match event.event_type.as_str() {
            "server.connected" => {
                tracing::debug!("SSE: server.connected");
                SseAction::Continue
            }

            "server.heartbeat" => {
                // Heartbeat keeps the connection alive but is not session activity
                tracing::trace!("SSE: server.heartbeat");
                SseAction::Continue
            }

            "message.updated" => {
                if let Ok(info) = serde_json::from_value::<MessageInfoWrapper>(
                    event.properties.clone(),
                ) {
                    if let Some(ref info) = info.info {
                        if info.session_id.as_deref() == Some(session_id) {
                            // Log when we first learn the model
                            if result.model_id.is_none() {
                                if let Some(ref model) = info.model_id {
                                    tracing::info!(
                                        model = %model,
                                        provider = ?info.provider_id,
                                        "AI model selected"
                                    );
                                }
                            }
                            // Only update if new value is Some (don't overwrite with None)
                            if info.model_id.is_some() {
                                result.model_id = info.model_id.clone();
                            }
                            if info.provider_id.is_some() {
                                result.provider_id = info.provider_id.clone();
                            }
                        }
                    }
                }
                *last_activity = Instant::now();
                SseAction::Continue
            }

            "message.part.updated" => {
                if let Ok(wrapper) = serde_json::from_value::<PartWrapper>(
                    event.properties.clone(),
                ) {
                    let part = wrapper.part;

                    // Filter by session ID
                    if part.session_id.as_deref() != Some(session_id) {
                        return SseAction::Continue;
                    }

                    *last_activity = Instant::now();

                    // Track tool state
                    if part.part_type == "tool" {
                        if let Some(ref tool_name) = part.tool {
                            if let Some(ref state) = part.state {
                                let status = &state.status;

                                match status.as_str() {
                                    "running" => {
                                        *last_tool_name = Some(tool_name.clone());
                                        tracing::info!(
                                            tool = %tool_name,
                                            "Tool running"
                                        );
                                    }
                                    "completed" => {
                                        *last_tool_name = None;
                                        if let Some(ref output) = state.output {
                                            if output.starts_with("Error:") {
                                                tracing::error!(
                                                    tool = %tool_name,
                                                    output = %output,
                                                    "Tool completed with error"
                                                );
                                            } else {
                                                tracing::info!(
                                                    tool = %tool_name,
                                                    "Tool completed"
                                                );
                                            }
                                        }
                                    }
                                    "error" => {
                                        *last_tool_name = None;
                                        tracing::error!(
                                            tool = %tool_name,
                                            error = ?state.error,
                                            "Tool error"
                                        );
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    // Log step events
                    if part.part_type == "step-start" {
                        tracing::info!(
                            model = ?result.model_id,
                            "Step started"
                        );
                    }
                    if part.part_type == "step-finish" {
                        tracing::debug!(
                            reason = ?part.reason,
                            cost = ?part.cost,
                            "Step finished"
                        );
                    }

                    // Log AI text content at debug level (skip empty)
                    if part.part_type == "text" {
                        if let Some(ref text) = part.text {
                            if !text.is_empty() {
                            let preview = if text.len() > 200 {
                                format!("{}...", &text[..text.floor_char_boundary(200)])
                            } else {
                                text.clone()
                            };
                            tracing::debug!(
                                len = text.len(),
                                text = %preview,
                                "AI response text"
                            );
                            }
                        }
                    }

                    // Accumulate / replace part by ID (deduplication)
                    if let Some(ref id) = part.id {
                        parts.insert(id.clone(), part);
                    }
                }
                SseAction::Continue
            }

            "session.status" => {
                if let Ok(status) = serde_json::from_value::<SessionStatus>(
                    event.properties.clone(),
                ) {
                    if status.session_id == session_id {
                        *last_activity = Instant::now();
                        let new_type = status.status.status_type.clone();
                        if last_status_type.as_deref() != Some(&new_type) {
                            tracing::debug!(
                                status = %new_type,
                                attempt = ?status.status.attempt,
                                "Session status"
                            );
                            *last_status_type = Some(new_type);
                        }
                    }
                }
                SseAction::Continue
            }

            "session.idle" => {
                if let Some(sid) = event.properties.get("sessionID").and_then(|v| v.as_str()) {
                    if sid == session_id {
                        tracing::info!("Session idle — prompt complete");
                        return SseAction::Done;
                    }
                }
                SseAction::Continue
            }

            "session.error" => {
                if let Ok(err) = serde_json::from_value::<SessionError>(
                    event.properties.clone(),
                ) {
                    if err.session_id == session_id {
                        let error_name = &err.error.name;
                        tracing::error!(
                            error = %error_name,
                            "Session error"
                        );
                        return SseAction::Error(error_name.clone());
                    }
                } else {
                    // Fallback: try to extract error name from raw properties
                    let error_name = event.properties
                        .get("error")
                        .and_then(|e| e.get("name"))
                        .and_then(|n| n.as_str())
                        .or_else(|| event.properties.get("error").and_then(|e| e.as_str()))
                        .unwrap_or("UnknownError");
                    let sid = event.properties
                        .get("sessionID")
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    if sid == session_id || sid.is_empty() {
                        tracing::error!(
                            error = %error_name,
                            raw = %event.properties,
                            "Session error (raw)"
                        );
                        return SseAction::Error(error_name.to_string());
                    }
                }
                SseAction::Continue
            }

            _ => {
                tracing::trace!(event_type = %event.event_type, "Unknown SSE event");
                SseAction::Continue
            }
        }
    }
}

/// Action to take after processing an SSE event.
enum SseAction {
    Continue,
    Done,
    Error(String),
}

/// Accumulated result from SSE streaming.
#[derive(Debug, Default)]
pub struct SseResult {
    pub parts: Vec<ResponsePart>,
    pub reply_sent_by_tool: bool,
    pub model_id: Option<String>,
    pub provider_id: Option<String>,
    pub error: Option<String>,
    pub timed_out: bool,
}

/// Check if the reply_message tool was used successfully in the accumulated parts.
fn check_tool_used(parts: &[ResponsePart]) -> bool {
    parts.iter().any(|p| {
        p.part_type == "tool"
            && p.tool.as_deref().map(|t| t.contains("reply_message")).unwrap_or(false)
            && p.state.as_ref().is_some_and(|s| {
                s.status == "completed"
                    && s.output
                        .as_ref()
                        .is_some_and(|o| !o.starts_with("Error:"))
            })
    })
}

/// Helpers for deserializing nested SSE properties.
#[derive(Debug, serde::Deserialize)]
struct MessageInfoWrapper {
    info: Option<MessageInfo>,
}

#[derive(Debug, serde::Deserialize)]
struct PartWrapper {
    part: ResponsePart,
}

/// URL-encode a string for headers/query parameters.
fn urlencoding_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                format!("{}", b as char)
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}
