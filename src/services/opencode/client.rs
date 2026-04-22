use anyhow::{Context, Result};
use chrono::Utc;
use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use std::time::Instant;
use tokio::sync::mpsc;

use super::types::*;
use crate::core::thread_event::ThreadEvent;
use crate::core::thread_event_bus::ThreadEventBusRef;
use crate::core::thread_manager::QueueItem;
use crate::utils::constants::*;

/// OpenCode HTTP + SSE client.
///
/// Wraps all HTTP calls to the OpenCode server and provides
/// SSE event streaming with activity-based timeout.
pub struct OpenCodeClient {
    http: reqwest::Client,
    base_url: String,
    /// Optional event bus for publishing thread events.
    /// If provided, the client will publish appropriate events.
    event_bus: Option<ThreadEventBusRef>,
}

impl OpenCodeClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.to_string(),
            event_bus: None,
        }
    }
    
    /// Create a new client with an event bus for publishing thread events.
    #[allow(dead_code)]
    pub fn with_event_bus(base_url: &str, event_bus: Option<ThreadEventBusRef>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.to_string(),
            event_bus,
        }
    }

    /// Create a client reusing an existing reqwest::Client (shares connection pool).
    #[allow(dead_code)]
    pub fn with_http_client(base_url: &str, http: reqwest::Client) -> Self {
        Self {
            http,
            base_url: base_url.to_string(),
            event_bus: None,
        }
    }
    
    /// Create a client with both HTTP client and event bus.
    pub fn with_http_client_and_event_bus(
        base_url: &str,
        http: reqwest::Client,
        event_bus: Option<ThreadEventBusRef>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.to_string(),
            event_bus,
        }
    }
    
    /// Helper method to publish an event if event bus is available.
    #[allow(dead_code)]
    async fn publish_event(&self, event: ThreadEvent) {
        if let Some(event_bus) = &self.event_bus {
            match event_bus.publish(event).await {
                Ok(_) => tracing::trace!("Event published successfully"),
                Err(e) => tracing::warn!("Failed to publish event: {}", e),
            }
        }
    }
    
    /// Helper method to publish an event asynchronously without blocking.
    /// Spawns a task to publish the event in the background.
    fn publish_event_async(&self, event: ThreadEvent) {
        if let Some(event_bus) = self.event_bus.clone() {
            // Log event details for debugging
            let event_type = match &event {
                ThreadEvent::Heartbeat { .. } => "Heartbeat",
                ThreadEvent::ProcessingStarted { .. } => "ProcessingStarted",
                ThreadEvent::ProcessingProgress { .. } => "ProcessingProgress",
                ThreadEvent::ProcessingCompleted { .. } => "ProcessingCompleted",
                ThreadEvent::ToolStarted { .. } => "ToolStarted",
                ThreadEvent::ToolCompleted { .. } => "ToolCompleted",
                ThreadEvent::Thinking { .. } => "Thinking",
                ThreadEvent::SessionStatus { .. } => "SessionStatus",
            };
            let thread_name = event.thread_name();
            
            tracing::trace!(
                event_type = %event_type,
                thread_name = %thread_name,
                "Spawning async task to publish thread event"
            );
            
            tokio::spawn(async move {
                match event_bus.publish(event).await {
                    Ok(_) => tracing::trace!("Event published asynchronously"),
                     Err(e) => tracing::trace!("Failed to publish event asynchronously: {}", e),
                }
            });
        } else {
            tracing::trace!("No event bus available, skipping event publication");
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
    pub async fn get_session(&self, session_id: &str, directory: &Path) -> Result<Option<SessionInfo>> {
        let url = format!("{}/session/{}", self.base_url, session_id);
        let (hdr_name, hdr_val) = Self::directory_header(directory);

        let resp = self
            .http
            .get(&url)
            .header(hdr_name, &hdr_val)
            .timeout(OPENCODE_HEALTH_CHECK_TIMEOUT)
            .send()
            .await
            .context("get_session request failed")?;

        let status = resp.status();
        tracing::debug!(session_id = %session_id, status = %status, "get_session response");

        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("get_session failed ({}): {}", status, body);
        }

        match resp.json::<SessionInfo>().await {
            Ok(session) => Ok(Some(session)),
            Err(e) => {
                tracing::debug!(error = %e, "get_session JSON parse failed");
                Ok(None)
            }
        }
    }

    /// Get all available providers and their models.
    pub async fn get_providers(&self, directory: &Path) -> Result<ProvidersResponse> {
        let url = format!("{}/provider", self.base_url);
        let (hdr_name, hdr_val) = Self::directory_header(directory);

        let resp = self
            .http
            .get(&url)
            .header(hdr_name, &hdr_val)
            .timeout(OPENCODE_HEALTH_CHECK_TIMEOUT)
            .send()
            .await
            .context("get_providers request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("get_providers failed ({}): {}", status, body);
        }

        let providers: ProvidersResponse = resp.json().await.context("parse providers response")?;
        Ok(providers)
    }

    /// Look up the context window limit for a specific model.
    ///
    /// Returns the context token limit if found, or None.
    /// The model string should be in "provider/model-id" format.
    pub async fn get_model_context_limit(&self, directory: &Path, model: &str) -> Option<u64> {
        let (provider_id, model_id) = model.split_once('/')?;
        let providers = self.get_providers(directory).await.ok()?;

        for provider in &providers.all {
            if provider.id == provider_id {
                tracing::debug!(
                    provider = %provider_id,
                    available_models = ?provider.models.keys().collect::<Vec<_>>(),
                    looking_for = %model_id,
                    "Searching for model in provider"
                );
                if let Some(model_info) = provider.models.get(model_id) {
                    tracing::debug!(
                        model = %model,
                        limit = ?model_info.limit,
                        "Model found in provider"
                    );
                    if let Some(ref limit) = model_info.limit {
                        if limit.context > 0 {
                            tracing::info!(
                                model = %model,
                                context_limit = limit.context,
                                "Model context limit discovered"
                            );
                            return Some(limit.context);
                        }
                    }
                }
            }
        }
        None
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
        mode_label: &str,
        pending_rx: &mut mpsc::Receiver<QueueItem>,
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

        // 2.5. Publish ProcessingStarted event if event bus is available
        // Note: we don't have message_id here, so we use session_id as a proxy
        let thread_name = directory
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        
        self.publish_event_async(ThreadEvent::ProcessingStarted {
            thread_name: thread_name.clone(),
            message_id: session_id.to_string(), // Use session_id as proxy for message_id
            timestamp: Utc::now(),
        });

        // 3. Process SSE events
        let mut result = SseResult::default();

        // If model is known upfront (from config or /model override), record on span immediately.
        // Set model_recorded flag to prevent SSE handler from adding a duplicate.
        if let Some(ref model_ref) = request.model {
            let model_str = format!("{}/{}:{}", model_ref.provider_id, model_ref.model_id, mode_label);
            tracing::Span::current().record("m", &model_str);
            result.model_recorded = true;
        }
        let mut parts: HashMap<String, ResponsePart> = HashMap::new();
        let mut last_activity = Instant::now();
        let mut last_progress_log = Instant::now();
        let mut last_tool_name: Option<String> = None;
        let mut last_tool_input: Option<String> = None;
        let mut last_status_type: Option<String> = None;
        let start_time = Instant::now();
        let mut done = false;
        let mut logged_tools: HashSet<(String, String)> = HashSet::new();
        let mut model_updated = false;
        let mut tool_start_times: HashMap<String, Instant> = HashMap::new();
        let mut reply_tool_completed = false;

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

                            // Handle server.heartbeat events - these are just connection keep-alive,
                            // not meaningful progress events. Thread Manager will control actual heartbeat timing.
                            if event_type == "server.heartbeat" {
                                tracing::trace!("SSE: server.heartbeat (connection keep-alive)");
                                continue;
                            }

                                let event_result = self.handle_sse_event(
                                    &sse_event,
                                    session_id,
                                    &thread_name,
                                    directory,
                                    mode_label,
                                    &mut parts,
                                    &mut result,
                                    &mut last_activity,
                                    &mut last_tool_name,
                                    &mut last_tool_input,
                                    &mut last_status_type,
                                    &mut logged_tools,
                                    &start_time,
                                    &mut tool_start_times,
                                    &mut reply_tool_completed,
                                ).await;

                                match event_result {
                                    SseAction::Continue => {}
                                    SseAction::Done => { done = true; }
                                    SseAction::Error { technical, user_message } => {
                                        result.error = Some(technical);
                                        result.error_message = Some(user_message);
                                        done = true;
                                    }
                                }

                                // Update reply-context.json when model is first discovered
                                // (needed by both MCP reply tool and fallback for footer display)
                                if result.model_id.is_some() && !model_updated {
                                    if let Ok(mut ctx) = crate::mcp::context::load_reply_context(directory).await {
                                        let combined_model = if let (Some(provider), Some(model)) = (&result.provider_id, &result.model_id) {
                                            Some(format!("{}/{}", provider, model))
                                        } else {
                                            result.model_id.clone()
                                        };
                                        ctx.model = combined_model;
                                        // Update mode with the real value from OpenCode
                                        if result.mode.is_some() {
                                            ctx.mode = result.mode.clone();
                                        }
                                        crate::mcp::context::save_reply_context(directory, &ctx).await.ok();
                                    }
                                    model_updated = true;
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

                        let tool_detail = last_tool_input
                            .as_deref()
                            .unwrap_or("");

                        let output_len: usize = parts.values()
                            .filter_map(|p| p.text.as_ref())
                            .map(|t| t.len())
                            .sum();

                        tracing::info!(
                            elapsed_secs = elapsed.as_secs(),
                            parts = parts.len(),
                            activity = %activity,
                            silence_secs = silence.as_secs(),
                            output_len,
                            tool_detail = %tool_detail,
                            "Progress"
                        );

                        // Publish ProcessingProgress event if event bus is available
                        let progress_summary = format!("{} parts, {} chars", parts.len(), output_len);
                        self.publish_event_async(ThreadEvent::ProcessingProgress {
                            thread_name: thread_name.clone(),
                            elapsed_secs: elapsed.as_secs(),
                            activity: activity.to_string(),
                            progress: Some(progress_summary),
                            parts_count: parts.len(),
                            output_length: output_len,
                            timestamp: Utc::now(),
                        });

                        last_progress_log = Instant::now();
                    }
                }

                // Live message injection: new message arrived while AI is processing
                new_item = pending_rx.recv() => {
                    if let Some(item) = new_item {
                        tracing::info!(
                            sender = %item.message.sender_address,
                            topic = %item.message.topic,
                            "Live message injection — follow-up received during AI processing"
                        );

                        // Store the injected message
                        let storage = crate::core::message_storage::MessageStorage::new(
                            directory.parent().unwrap_or(directory),
                        );
                        let thread_name = directory
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();

                        if let Ok(store_result) = storage
                            .store(&item.message, &thread_name, item.attachment_config.as_ref())
                            .await
                        {
                            // Process commands from injected message
                            let raw_body = item.message.content.text.as_deref()
                                .or(item.message.content.markdown.as_deref())
                                .unwrap_or("");

                            let cleaned_body = crate::core::email_parser::strip_quoted_history(raw_body);

                            // Update reply-context.json with the new message dir
                            if let Ok(mut ctx) = crate::mcp::context::load_reply_context(directory).await {
                                ctx.incoming_message_dir = store_result.message_dir.clone();
                                crate::mcp::context::save_reply_context(directory, &ctx).await.ok();
                            }

                            // Inject the body into the AI session (if non-empty)
                            // Just send the raw body — same as OpenCode TUI does
                            if !cleaned_body.trim().is_empty() {
                                let injection_request = PromptRequest {
                                    system: String::new(),
                                    model: None,
                                    agent: None,
                                    parts: vec![PromptPart::Text { text: cleaned_body.trim().to_string() }],
                                };

                                match self.prompt_async(session_id, directory, &injection_request).await {
                                    Ok(()) => {
                                        tracing::info!("Follow-up message injected into AI session");
                                        last_activity = Instant::now();
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "Failed to inject follow-up message");
                                    }
                                }
                            }
                        }
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

        // Publish ProcessingCompleted event if event bus is available
        let duration = start_time.elapsed();
        self.publish_event_async(ThreadEvent::ProcessingCompleted {
            thread_name,
            message_id: session_id.to_string(), // Use session_id as proxy for message_id
            success: true, // We only get here if no error occurred
            duration_secs: duration.as_secs(),
            timestamp: Utc::now(),
        });

        Ok(result)
    }

    /// Handle a single SSE event. Returns action to take.
    async fn handle_sse_event(
        &self,
        event: &SseEvent,
        session_id: &str,
        thread_name: &str,
        directory: &Path,
        mode_label: &str,
        parts: &mut HashMap<String, ResponsePart>,
        result: &mut SseResult,
        last_activity: &mut Instant,
        last_tool_name: &mut Option<String>,
        last_tool_input: &mut Option<String>,
        last_status_type: &mut Option<String>,
        logged_tools: &mut HashSet<(String, String)>,
        _start_time: &Instant,
        tool_start_times: &mut HashMap<String, Instant>,
        reply_tool_completed: &mut bool,
    ) -> SseAction {
        match event.event_type.as_str() {
            "server.connected" => {
                tracing::debug!("SSE: server.connected");
                SseAction::Continue
            }

            "server.heartbeat" => {
                // SSE heartbeat events are just connection keep-alive, not meaningful progress events.
                // They are handled in the prompt_with_sse loop and not converted to ThreadEvent.
                tracing::trace!("SSE: server.heartbeat");
                SseAction::Continue
            }

            "message.updated" => {
                if let Ok(info) = serde_json::from_value::<MessageInfoWrapper>(
                    event.properties.clone(),
                 ) {
                    if let Some(ref info) = info.info {
                        if info.session_id.as_deref() == Some(session_id) {
                            // Only update if new value is Some (don't overwrite with None)
                            if info.model_id.is_some() {
                                result.model_id = info.model_id.clone();
                            }
                            if info.provider_id.is_some() {
                                result.provider_id = info.provider_id.clone();
                            }

                            // When we first learn the model, record on the parent ai span (once only).
                            // Check model_id AFTER setting it above to prevent duplicate recording.
                            if result.model_id.is_some() && !result.model_recorded {
                                let combined_model = match (&result.provider_id, &result.model_id) {
                                    (Some(provider), Some(model_id)) => format!("{}/{}", provider, model_id),
                                    (_, Some(model_id)) => model_id.clone(),
                                    _ => "unknown".to_string(),
                                };
                                let m_value = format!("{}:{}", combined_model, mode_label);
                                tracing::Span::current().record("m", &m_value);
                                tracing::info!("AI model selected");
                                result.model_recorded = true;
                            }
                            if info.mode.is_some() {
                                result.mode = info.mode.clone();
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

                    // Track tool state (deduplicated per step)
                    if part.part_type == "tool" {
                        if let Some(ref tool_name) = part.tool {
                            if let Some(ref state) = part.state {
                                let status = &state.status;
                                let dedup_key = (tool_name.clone(), status.clone());

                                match status.as_str() {
                                    "running" => {
                                        *last_tool_name = Some(tool_name.clone());
                                        if logged_tools.insert(dedup_key) {
                                            // Extract tool input
                                            let input_preview = state.input.as_ref()
                                                .and_then(|v| {
                                                    v.as_str().map(|s| s.to_string())
                                                        .or_else(|| Some(v.to_string()))
                                                })
                                                .unwrap_or_default();
                                            
                                            // Save for progress logging
                                            *last_tool_input = Some(input_preview.clone());
                                            tracing::info!(
                                                tool = %tool_name,
                                                input = %input_preview,
                                                "Tool running"
                                            );
                                            
                                            // Record tool start time
                                            tool_start_times.insert(tool_name.clone(), Instant::now());
                                            
                                            // Publish ToolStarted event asynchronously
                                            self.publish_event_async(ThreadEvent::ToolStarted {
                                                thread_name: thread_name.to_string(),
                                                tool_name: tool_name.clone(),
                                                input: if input_preview.is_empty() { None } else { Some(input_preview.clone()) },
                                                timestamp: Utc::now(),
                                            });
                                        }
                                    }
                                    "completed" => {
                                        *last_tool_name = None;
                                        *last_tool_input = None;
                                        if logged_tools.insert(dedup_key) {
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
                                            } else {
                                                tracing::info!(
                                                    tool = %tool_name,
                                                    "Tool completed"
                                                );
                                            }
                                            
                                            // Calculate tool duration
                                            let duration_secs = tool_start_times
                                                .remove(tool_name)
                                                .map(|start_time| start_time.elapsed().as_secs())
                                                .unwrap_or(0);
                                            
                                            // Detect tool errors from output
                                            let has_error = state.output.as_ref()
                                                .is_some_and(|o| o.starts_with("Error:"));
                                            let error_preview = if has_error {
                                                state.output.as_ref().map(|o| {
                                                    if o.len() > 200 {
                                                        format!("{}...", &o[..o.floor_char_boundary(200)])
                                                    } else {
                                                        o.clone()
                                                    }
                                                })
                                            } else {
                                                None
                                            };

                                            // Publish ToolCompleted event asynchronously
                                            self.publish_event_async(ThreadEvent::ToolCompleted {
                                                thread_name: thread_name.to_string(),
                                                tool_name: tool_name.clone(),
                                                success: !has_error,
                                                duration_secs,
                                                output: error_preview,
                                                timestamp: Utc::now(),
                                            });

                                            // Early exit: if the reply tool completed successfully,
                                            // flag it so we can break out of the SSE loop on the
                                            // next step-finish — no need to wait for the full session.
                                            if tool_name.contains("reply_message") || tool_name.contains("ask_user") {
                                                let has_error = state.output.as_ref()
                                                    .is_some_and(|o| o.starts_with("Error:"));
                                                if !has_error {
                                                    tracing::info!("Reply tool completed — will exit SSE after current step");
                                                    *reply_tool_completed = true;
                                                }
                                            }
                                        }
                                    }
                                    "error" => {
                                        *last_tool_name = None;
                                        *last_tool_input = None;
                                        tracing::error!(
                                            tool = %tool_name,
                                            error = ?state.error,
                                            "Tool error"
                                        );
                                        
                                        // Calculate tool duration even on error
                                        let duration_secs = tool_start_times
                                            .remove(tool_name)
                                            .map(|start_time| start_time.elapsed().as_secs())
                                            .unwrap_or(0);
                                        
                                        // Publish ToolCompleted event asynchronously with success=false
                                        let error_preview = state.error.as_ref().map(|e| {
                                            let s = format!("{e}");
                                            if s.len() > 200 {
                                                format!("{}...", &s[..s.floor_char_boundary(200)])
                                            } else {
                                                s
                                            }
                                        });
                                        self.publish_event_async(ThreadEvent::ToolCompleted {
                                            thread_name: thread_name.to_string(),
                                            tool_name: tool_name.clone(),
                                            success: false,
                                            duration_secs,
                                            output: error_preview,
                                            timestamp: Utc::now(),
                                        });
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    // Log step events
                    if part.part_type == "step-start" {
                        logged_tools.clear(); // Reset tool dedup for new step
                        tracing::info!("Step started");
                    }
                    if part.part_type == "step-finish" {
                        // Try to parse tokens if available
                        if let Some(ref tokens_json) = part.tokens {
                            match serde_json::from_value::<TokenInfo>(tokens_json.clone()) {
                                Ok(token_info) => {
                                    // Save token information to result
                                    result.input_tokens = Some(token_info.input);
                                    result.output_tokens = Some(token_info.output);
                                    result.reasoning_tokens = Some(token_info.reasoning);
                                    result.cache_read_tokens = Some(token_info.cache.read);
                                    result.cache_write_tokens = Some(token_info.cache.write);
                                    result.total_cost = part.cost;

                                    // Persist input tokens immediately per step — don't wait
                                    // for the prompt to complete. This ensures tokens are saved
                                    // even if the SSE stream exits early (reply tool, timeout).
                                    crate::services::opencode::session::add_input_tokens(
                                        directory, token_info.input
                                    ).await.ok();
                                    
                                    tracing::info!(
                                        reason = ?part.reason,
                                        cost = ?part.cost,
                                        input_tokens = token_info.input,
                                        output_tokens = token_info.output,
                                        reasoning_tokens = token_info.reasoning,
                                        cache_read_tokens = token_info.cache.read,
                                        cache_write_tokens = token_info.cache.write,
                                        total_tokens = token_info.input + token_info.output + token_info.reasoning,
                                        "Step finished with token details"
                                    );
                                }
                                Err(e) => {
                                    // Fallback to showing raw tokens if parsing fails
                                    tracing::debug!(
                                        reason = ?part.reason,
                                        cost = ?part.cost,
                                        raw_tokens = ?tokens_json,
                                        "Step finished (failed to parse tokens: {})", e
                                    );
                                }
                            }
                        } else {
                            tracing::debug!(
                                reason = ?part.reason,
                                cost = ?part.cost,
                                "Step finished (no token information)"
                            );
                        }

                        // Reply tool completed: mark for result tracking but do NOT exit early.
                        // The AI may have additional steps after reply (e.g., deploy command).
                        // Let OpenCode finish all steps naturally.
                        if *reply_tool_completed {
                            tracing::info!("Reply tool completed — continuing to process remaining steps");
                        }
                    }

                    // Log AI text content at debug level (skip empty)
                    if part.part_type == "text" {
                        if let Some(ref text) = part.text {
                            if !text.is_empty() && !text.trim().starts_with("<system-reminder>") {
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

                    // Log reasoning/thinking content and publish to Activity panel
                    if part.part_type == "reasoning" {
                        if let Some(ref text) = part.text {
                            if !text.is_empty() {
                                let preview = if text.len() > 300 {
                                    format!("{}...", &text[..text.floor_char_boundary(300)])
                                } else {
                                    text.clone()
                                };
                                tracing::debug!(
                                    len = text.len(),
                                    text = %preview,
                                    "AI thinking"
                                );
                                self.publish_event_async(ThreadEvent::Thinking {
                                    thread_name: thread_name.to_string(),
                                    text: preview,
                                    full_length: text.len(),
                                    timestamp: Utc::now(),
                                });
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
                                message = ?status.status.message,
                                "Session status"
                            );
                            *last_status_type = Some(new_type.clone());

                            // Publish notable status changes to Activity panel
                            // (retries, errors, rate limits — not routine status like "started")
                            if matches!(new_type.as_str(), "retry" | "error" | "rate_limit" | "timeout") {
                                self.publish_event_async(ThreadEvent::SessionStatus {
                                    thread_name: thread_name.to_string(),
                                    status_type: new_type.clone(),
                                    attempt: status.status.attempt,
                                    message: status.status.message.as_ref().map(|m| {
                                        if m.len() > 200 {
                                            format!("{}...", &m[..m.floor_char_boundary(200)])
                                        } else {
                                            m.clone()
                                        }
                                    }),
                                    timestamp: Utc::now(),
                                });
                            }
                        }
                        // Clear tool dedup on retry so retried tool calls are logged
                        if new_type == "retry" {
                            logged_tools.clear();
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
                        let user_message = user_friendly_error_message(error_name);
                        tracing::error!(
                            error = %error_name,
                            session_id = %err.session_id,
                            error_detail = ?err.error.data,
                            raw_event = %event.properties,
                            user_message = %user_message,
                            "Session error"
                        );
                        return SseAction::Error {
                            technical: error_name.clone(),
                            user_message,
                        };
                    }
                } else {
                    // Fallback: try to extract error name from raw properties
                    let error_name = event.properties
                        .get("error")
                        .and_then(|e| e.get("name"))
                        .and_then(|n| n.as_str())
                        .or_else(|| event.properties.get("error").and_then(|e| e.as_str()))
                        .unwrap_or("UnknownError");
                    let user_message = user_friendly_error_message(&error_name);
                    let sid = event.properties
                        .get("sessionID")
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    if sid == session_id || sid.is_empty() {
                        tracing::error!(
                            error = %error_name,
                            session_id = %sid,
                            raw_event = %event.properties,
                            user_message = %user_message,
                            "Session error (fallback parsing)"
                        );
                        return SseAction::Error {
                            technical: error_name.to_string(),
                            user_message,
                        };
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
    Error {
        technical: String,
        user_message: String,
    },
}

/// Convert technical error names to user-friendly messages.
fn user_friendly_error_message(error_name: &str) -> String {
    match error_name {
        "APIError" => "Process encountered a server error. Please try again.".to_string(),
        "ContextOverflow" => "Process exceeded context limits. Please try again.".to_string(),
        "TimeoutError" => "Process timed out. Please try again.".to_string(),
        "AuthenticationError" => "Process encountered an authentication error. Please try again.".to_string(),
        "RateLimitError" => "Process encountered rate limiting. Please try again later.".to_string(),
        _ => "Process encountered an error. Please try again.".to_string(),
    }
}

/// Accumulated result from SSE streaming.
#[derive(Debug, Default)]
pub struct SseResult {
    pub parts: Vec<ResponsePart>,
    pub reply_sent_by_tool: bool,
    pub model_id: Option<String>,
    pub provider_id: Option<String>,
    /// Whether the model has been recorded on the ai span (prevent duplicate m=)
    pub model_recorded: bool,
    /// The actual mode OpenCode used (from SSE message.updated)
    pub mode: Option<String>,
    pub error: Option<String>,
    pub error_message: Option<String>,
    pub timed_out: bool,
    // Token usage information from step-finish events
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub total_cost: Option<f64>,
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
