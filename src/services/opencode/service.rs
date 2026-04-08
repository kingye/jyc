use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::Instrument;

use super::client::{OpenCodeClient, SseResult};
use super::types::*;
use super::{session, prompt_builder, OpenCodeServer};
use super::session::DEFAULT_MAX_INPUT_TOKENS;
use crate::channels::types::InboundMessage;
use crate::config::types::AgentConfig;
use crate::core::thread_event::ThreadEvent;
use crate::core::thread_event_bus::ThreadEventBusRef;
use crate::core::thread_manager::QueueItem;
use crate::services::agent::{AgentResult, AgentService};
use crate::utils::constants::{HEARTBEAT_INTERVAL, MIN_HEARTBEAT_ELAPSED};
use tokio::sync::mpsc;

/// Encapsulates all OpenCode AI interaction logic.
///
/// Channel-agnostic — does NOT know about email, SMTP, or reply formatting.
/// Returns raw AI text. The outbound adapter handles formatting + sending + storing.
pub struct OpenCodeService {
    server: Arc<OpenCodeServer>,
    agent_config: Arc<AgentConfig>,
    workdir: PathBuf,
    /// Shared HTTP client — reused across all requests to share connection pool.
    http_client: reqwest::Client,
    /// Thread-isolated event bus for publishing events (optional).
    /// Wrapped in Mutex to allow runtime configuration.
    event_bus: Mutex<Option<ThreadEventBusRef>>,
    /// Per-thread event bus mapping for thread isolation.
    event_bus_map: Mutex<std::collections::HashMap<String, ThreadEventBusRef>>,
}

impl OpenCodeService {
    /// Create a new OpenCodeService without event support (backward compatible).
    pub fn new(
        server: Arc<OpenCodeServer>,
        agent_config: Arc<AgentConfig>,
        workdir: PathBuf,
    ) -> Self {
        Self {
            server,
            agent_config,
            workdir,
            http_client: reqwest::Client::new(),
            event_bus: Mutex::new(None),
            event_bus_map: Mutex::new(std::collections::HashMap::new()),
        }
    }
    
    /// Create a new OpenCodeService with optional event bus.
    #[allow(dead_code)]
    pub fn new_with_event_bus(
        server: Arc<OpenCodeServer>,
        agent_config: Arc<AgentConfig>,
        workdir: PathBuf,
        event_bus: Option<ThreadEventBusRef>,
    ) -> Self {
        Self {
            server,
            agent_config,
            workdir,
            http_client: reqwest::Client::new(),
            event_bus: Mutex::new(event_bus),
            event_bus_map: Mutex::new(std::collections::HashMap::new()),
        }
    }
    
    /// Helper method to publish an event if event bus is available.
    #[allow(dead_code)]
    async fn publish_event(&self, event: ThreadEvent) {
        let event_bus_lock = self.event_bus.lock().await;
        if let Some(event_bus) = &*event_bus_lock {
            match event_bus.publish(event).await {
                Ok(_) => tracing::trace!("Event published successfully"),
                Err(e) => tracing::warn!("Failed to publish event: {}", e),
            }
        }
    }
    
    /// Helper method to publish ProcessingCompleted event.
    async fn publish_processing_completed(
        &self,
        thread_name: &str,
        message_id: String,
        start_time: chrono::DateTime<Utc>,
        success: bool,
    ) {
        let duration = Utc::now().signed_duration_since(start_time);
        let duration_secs = duration.num_seconds() as u64;
        
        self.publish_event(ThreadEvent::ProcessingCompleted {
            thread_name: thread_name.to_string(),
            message_id,
            success,
            duration_secs,
            timestamp: Utc::now(),
        }).await;
    }
    
    /// Helper method to publish a heartbeat event.
    /// 
    /// Heartbeat events are sent at regular intervals during long-running
    /// processing to indicate the agent is still working.
    #[allow(dead_code)]
    async fn publish_heartbeat(
        &self,
        thread_name: &str,
        elapsed_secs: u64,
        activity: &str,
        progress: &str,
    ) {
        self.publish_event(ThreadEvent::Heartbeat {
            thread_name: thread_name.to_string(),
            elapsed_secs,
            activity: activity.to_string(),
            progress: progress.to_string(),
            timestamp: Utc::now(),
        }).await;
    }
    
    /// Check if a heartbeat should be sent based on elapsed time.
    /// 
    /// Returns true if enough time has passed since the last heartbeat
    /// and minimum elapsed time has been reached.
    #[allow(dead_code)]
    fn should_send_heartbeat(
        last_heartbeat_time: Option<Instant>,
        elapsed_since_start: Duration,
    ) -> bool {
        // Don't send heartbeat if not enough time has passed since processing started
        if elapsed_since_start < MIN_HEARTBEAT_ELAPSED {
            return false;
        }
        
        // If we've never sent a heartbeat, send one now
        let last_heartbeat = match last_heartbeat_time {
            Some(time) => time,
            None => return true,
        };
        
        // Check if enough time has passed since last heartbeat
        last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL
    }
    
    /// Helper method to publish ProcessingProgress event.
    #[allow(dead_code)]
    async fn publish_processing_progress(
        &self,
        thread_name: &str,
        elapsed_secs: u64,
        activity: &str,
        progress: Option<&str>,
        parts_count: usize,
        output_length: usize,
    ) {
        self.publish_event(ThreadEvent::ProcessingProgress {
            thread_name: thread_name.to_string(),
            elapsed_secs,
            activity: activity.to_string(),
            progress: progress.map(|s| s.to_string()),
            parts_count,
            output_length,
            timestamp: Utc::now(),
        }).await;
    }
    
    /// Set the event bus for this agent.
    /// This allows the event bus to be set after the agent is created.
    #[allow(dead_code)]
    pub async fn set_event_bus(&self, event_bus: Option<ThreadEventBusRef>) {
        let mut event_bus_lock = self.event_bus.lock().await;
        *event_bus_lock = event_bus;
    }

    #[allow(dead_code)]
    async fn set_thread_event_bus(&self, thread_name: &str, event_bus: Option<ThreadEventBusRef>) {
        tracing::debug!(
            thread_name = %thread_name,
            has_event_bus = event_bus.is_some(),
            "Setting thread event bus for agent service"
        );
        
        // Store event bus in per-thread map
        let mut event_bus_map = self.event_bus_map.lock().await;
        if let Some(bus) = event_bus {
            event_bus_map.insert(thread_name.to_string(), bus);
            tracing::debug!(
                thread_name = %thread_name,
                "Event bus stored in per-thread map"
            );
        } else {
            event_bus_map.remove(thread_name);
            tracing::debug!(
                thread_name = %thread_name,
                "Event bus removed from per-thread map"
            );
        }
    }

    /// Internal: generate AI reply via OpenCode SSE streaming.
    async fn generate_reply(
        &self,
        message: &InboundMessage,
        thread_name: &str,
        thread_path: &Path,
        message_dir: &str,
        pending_rx: &mut mpsc::Receiver<QueueItem>,
    ) -> Result<GenerateReplyResult> {
        let _ch = &message.channel;

        // 0. Record start time (events will be published by OpenCodeClient if event bus is available)
        let start_time = Utc::now();

        // 1. Ensure OpenCode server is running
        let base_url = self.server.base_url().await?;
        
        // Get thread-specific event bus for OpenCodeClient
        tracing::debug!(
            thread_name = %thread_name,
            "Getting event bus from map for OpenCodeClient"
        );
        
        let event_bus_map = self.event_bus_map.lock().await;
        let event_bus = event_bus_map.get(thread_name).cloned();
        let has_event_bus = event_bus.is_some();
        drop(event_bus_map); // Release lock immediately
        
        tracing::debug!(
            thread_name = %thread_name,
            has_event_bus = has_event_bus,
            "Event bus retrieved for thread"
        );
        
        let client = OpenCodeClient::with_http_client_and_event_bus(
            &base_url,
            self.http_client.clone(),
            event_bus,
        );

        // 2. Ensure thread has opencode.json
        let config_changed = session::ensure_thread_opencode_setup(
            thread_path,
            &self.agent_config,
            &self.workdir,
        ).await?;

        tracing::debug!(config_changed = config_changed, "opencode.json check");

        if config_changed {
            tracing::info!("opencode.json updated");
        }

        // 3. Read model early (needed for context limit lookup before session creation)
        let model: Option<String> = session::read_model_override(thread_path)
            .await
            .or_else(|| {
                self.agent_config
                    .opencode
                    .as_ref()
                    .and_then(|o| o.model.clone())
            });

        // 4. Get or create session (reuse across messages, mode switches, model switches)
        // Sessions are only deleted for error recovery:
        // - ContextOverflow (handle_sse_result)
        // - Stale session detection (handle_sse_result)
        // - Session token limit (input token based reset)
        //
        // max_input_tokens priority:
        // 1. Config override (agent.opencode.max_input_tokens)
        // 2. 95% of the model's context window (from OpenCode /provider API)
        // 3. Default (120K)
        let config_max_tokens: Option<u64> = self.agent_config
            .opencode
            .as_ref()
            .and_then(|oc| oc.max_input_tokens);

        let max_input_tokens = if config_max_tokens.is_some() {
            tracing::debug!(
                max_input_tokens = ?config_max_tokens,
                "Using config-defined max input tokens"
            );
            config_max_tokens
        } else if let Some(ref m) = model {
            if let Some(context_limit) = client.get_model_context_limit(thread_path, m).await {
                let limit_95 = (context_limit as f64 * 0.95) as u64;
                tracing::info!(
                    model = %m,
                    context_limit = context_limit,
                    max_input_tokens = limit_95,
                    "Using 95% of model context window as input token limit"
                );
                Some(limit_95)
            } else {
                tracing::debug!(
                    model = %m,
                    "Could not get model context limit, falling back to default"
                );
                None
            }
        } else {
            tracing::debug!("No model specified, using default max input tokens");
            None
        };

        let (session_id, session_reset_due_to_tokens) = session::get_or_create_session(
            &client, 
            thread_path,
            max_input_tokens,
        ).await?;

        // Save resolved max_input_tokens to session state for footer display
        let resolved_max = max_input_tokens.unwrap_or(DEFAULT_MAX_INPUT_TOKENS);
        session::save_max_input_tokens(thread_path, resolved_max).await.ok();

        // 5. Clean up stale signal file
        session::cleanup_signal_file(thread_path).await;

        // 6. Read mode (from override)
        let mode_override = session::read_mode_override(thread_path).await;
        let agent_mode = if mode_override.as_deref() == Some("plan") {
            Some("plan".to_string())
        } else {
            None
        };

        let mode_label = agent_mode.as_deref().unwrap_or("build").to_string();

        tracing::info!(
            mode = %mode_label,
            mode_override = ?mode_override,
            agent = ?agent_mode,
            "Agent mode resolved"
        );

        // 6. Save reply context to disk for the MCP reply tool
        // The reply tool reads from .jyc/reply-context.json instead of a token in the prompt
        let thread_name_str = thread_name.to_string();
        crate::mcp::context::save_reply_context(thread_path, &crate::mcp::context::ReplyContext {
            channel: message.channel.clone(),
            thread_name: thread_name_str,
            incoming_message_dir: message_dir.to_string(),
            uid: message.channel_uid.clone(),
            model: model.clone(),
            mode: Some(mode_label.clone()),
            created_at: chrono::Utc::now().to_rfc3339(),
        }).await?;

        // Check if event bus is available for heartbeat events
        let event_bus_lock = self.event_bus.lock().await;
        let has_event_bus = event_bus_lock.is_some();
        drop(event_bus_lock); // Release lock immediately
        
        if has_event_bus {
            tracing::debug!("Event bus available, heartbeat events will be sent if processing takes longer than {} seconds", MIN_HEARTBEAT_ELAPSED.as_secs());
        }

        // 7. Check for session summaries
        // 8. Build prompts
        let system_prompt = prompt_builder::build_system_prompt(
            thread_path,
            self.agent_config.opencode.as_ref().and_then(|o| o.system_prompt.as_deref()),
            agent_mode.as_deref(),
        );

        let user_prompt = prompt_builder::build_prompt(
            message,
            thread_path,
            message_dir,
            session_reset_due_to_tokens,
        ).await?;

        // Model and mode are passed per-prompt — no session restart needed for switches
        let model_ref = model.as_deref().and_then(ModelRef::from_combined);

        let request = PromptRequest {
            system: system_prompt,
            model: model_ref,
            agent: agent_mode,
            parts: vec![PromptPart::Text { text: user_prompt }],
        };

        // 7. Send prompt via SSE streaming with ai{m=model:mode} span
        // Use Empty field — SSE handler will record the actual model when discovered.
        // If model is known upfront (config or /model override), record it immediately.
        let ai_span = tracing::info_span!("ai", m = tracing::field::Empty);
        if let Some(ref m) = model {
            ai_span.record("m", format!("{}:{}", m, mode_label));
        }

        tracing::info!(
            session_id = %session_id,
            "Sending prompt to OpenCode"
        );



        let sse_result = client
            .prompt_with_sse(&session_id, thread_path, &request, &mode_label, pending_rx)
            .instrument(ai_span.clone())
            .await;

        // 8. Handle result
        let result = match sse_result {
            Ok(result) => {
                self.handle_sse_result(
                    result, thread_name, thread_path,
                    &client, &session_id, &request, &mode_label, pending_rx,
                ).await
            }
            Err(e) => {
                tracing::error!(error = %e, "SSE streaming failed, trying blocking fallback");

                let blocking_result = client
                    .prompt_blocking(&session_id, thread_path, &request)
                    .await?;
                self.handle_blocking_result(
                    blocking_result, thread_name, thread_path,
                    &client, &session_id, &request,
                ).await
            }
        };



        // Handle the result
        // Note: OpenCodeClient will publish ProcessingCompleted event for successful cases
        // We only need to handle errors that occur before OpenCodeClient is called
        if let Err(e) = &result {
            // Publish ProcessingCompleted event for failure
            // (errors that occur before OpenCodeClient can publish events)
            self.publish_processing_completed(
                thread_name,
                message.external_id.clone().unwrap_or_else(|| "unknown".to_string()),
                start_time,
                false, // failure
            ).await;
            tracing::warn!(error = %e, "generate_reply failed");
        }

        result
    }

    /// Handle SSE streaming result.
    async fn handle_sse_result(
        &self,
        result: SseResult,
        thread_name: &str,
        thread_path: &Path,
        client: &OpenCodeClient,
        _session_id: &str,
        request: &PromptRequest,
        mode_label: &str,
        pending_rx: &mut mpsc::Receiver<QueueItem>,
    ) -> Result<GenerateReplyResult> {
        // ContextOverflow recovery
        if let Some(ref error) = result.error {
            tracing::error!(
                error = %error,
                user_message = ?result.error_message,
                "SSE result contains error"
            );
            if error.contains("ContextOverflow") {
                tracing::warn!("ContextOverflow — new session + retry");
                session::delete_session(thread_path).await?;
                let new_id = session::create_new_session(client, thread_path).await?;
                let retry = client.prompt_blocking(&new_id, thread_path, request).await?;
                return self.handle_blocking_result(
                    retry, thread_name, thread_path, client, &new_id, request,
                ).await;
            }
        }

        // Tool detection
        let reply_sent = result.reply_sent_by_tool
            || session::check_signal_file(thread_path).await;

        if reply_sent {
            // Extract the reply text from the tool's input so the monitor can deliver it
            // without reading from disk (reply.md is no longer created).
            let reply_text = extract_reply_text_from_tool_parts(&result.parts);
            tracing::info!(
                has_reply_text = reply_text.is_some(),
                "Reply sent by MCP tool"
            );
            return Ok(GenerateReplyResult {
                reply_sent_by_tool: true,
                reply_text,
                model_id: result.model_id,
                provider_id: result.provider_id,
                mode: result.mode,
            });
        }

        // Stale session detection
        let tool_reported = result.parts.iter().any(|p| {
            p.part_type == "tool"
                && p.tool.as_deref().map(|t| t.contains("reply_message")).unwrap_or(false)
                && p.state.as_ref().is_some_and(|s| s.status == "completed")
        });

        if tool_reported && !session::check_signal_file(thread_path).await {
            tracing::warn!("Stale session — retry");
            session::delete_session(thread_path).await?;
            let new_id = session::create_new_session(client, thread_path).await?;
            session::cleanup_signal_file(thread_path).await;
            let retry = client.prompt_with_sse(&new_id, thread_path, request, mode_label, pending_rx).await?;
            
            // Input tokens already persisted per step in client.rs
            
            let sent = retry.reply_sent_by_tool || session::check_signal_file(thread_path).await;
            if sent {
                return Ok(GenerateReplyResult {
                    reply_sent_by_tool: true,
                    reply_text: extract_reply_text_from_tool_parts(&retry.parts),
                    model_id: retry.model_id, provider_id: retry.provider_id,
                    mode: retry.mode,
                });
            }
            return Ok(GenerateReplyResult {
                reply_sent_by_tool: false,
                reply_text: extract_text_from_parts(&retry.parts),
                model_id: retry.model_id, provider_id: retry.provider_id,
                mode: retry.mode,
            });
        }

        // Timeout
        if result.timed_out {
            // Input tokens already persisted per step in client.rs
            
            if session::check_signal_file(thread_path).await {
                return Ok(GenerateReplyResult {
                    reply_sent_by_tool: true,
                    reply_text: extract_reply_text_from_tool_parts(&result.parts),
                    model_id: result.model_id, provider_id: result.provider_id,
                    mode: result.mode,
                });
            }
            let timeout_message = "Process timed out. Please try again.";
            tracing::error!(
                user_message = %timeout_message,
                "Timed out with no reply"
            );
            return Ok(GenerateReplyResult {
                reply_sent_by_tool: false, reply_text: Some(timeout_message.to_string()),
                model_id: result.model_id, provider_id: result.provider_id,
                mode: result.mode,
            });
        }

        // Fallback: extract text
        let reply_text = if let Some(ref error_message) = result.error_message {
            error_message.clone()
        } else {
            extract_text_from_parts(&result.parts).unwrap_or_default()
        };

        // Input tokens already persisted per step in client.rs

        Ok(GenerateReplyResult {
            reply_sent_by_tool: false,
            reply_text: Some(reply_text),
            model_id: result.model_id,
            provider_id: result.provider_id,
            mode: result.mode,
        })
    }

    /// Handle blocking prompt result.
    async fn handle_blocking_result(
        &self,
        result: PromptResponse,
        _thread_name: &str,
        thread_path: &Path,
        _client: &OpenCodeClient,
        _session_id: &str,
        _request: &PromptRequest,
    ) -> Result<GenerateReplyResult> {
        if let Some(ref data) = result.data {
            if let Some(ref info) = data.info {
                if let Some(ref error) = info.error {
                    tracing::error!(error = %error.name, "Blocking prompt error");
                }
            }
        }

        if session::check_signal_file(thread_path).await {
            // Blocking mode: no SSE parts available, reply text must be read
            // from the chat log by the monitor (thread_manager).
            return Ok(GenerateReplyResult {
                reply_sent_by_tool: true, reply_text: None,
                model_id: None, provider_id: None, mode: None,
            });
        }

        let parts = result.data.map(|d| d.parts).unwrap_or_default();
        Ok(GenerateReplyResult {
            reply_sent_by_tool: false,
            reply_text: extract_text_from_parts(&parts),
            model_id: None, provider_id: None, mode: None,
        })
    }
}

#[async_trait]
impl AgentService for OpenCodeService {
    async fn base_url(&self) -> Result<String> {
        self.server.base_url().await
    }

    async fn process(
        &self,
        message: &InboundMessage,
        thread_name: &str,
        thread_path: &Path,
        message_dir: &str,
        pending_rx: &mut mpsc::Receiver<QueueItem>,
    ) -> Result<AgentResult> {
        let result = self.generate_reply(message, thread_name, thread_path, message_dir, pending_rx).await?;

        Ok(AgentResult {
            reply_sent_by_tool: result.reply_sent_by_tool,
            reply_text: result.reply_text,
        })
    }

    async fn set_thread_event_bus(&self, thread_name: &str, event_bus: Option<ThreadEventBusRef>) {
        tracing::debug!(
            thread_name = %thread_name,
            has_event_bus = event_bus.is_some(),
            "Setting thread event bus for agent service (AgentService trait implementation)"
        );
        
        // Store event bus in per-thread map
        let mut event_bus_map = self.event_bus_map.lock().await;
        if let Some(bus) = event_bus {
            event_bus_map.insert(thread_name.to_string(), bus);
            tracing::debug!(
                thread_name = %thread_name,
                "Event bus stored in per-thread map (AgentService trait)"
            );
        } else {
            event_bus_map.remove(thread_name);
            tracing::debug!(
                thread_name = %thread_name,
                "Event bus removed from per-thread map (AgentService trait)"
            );
        }
    }
}

/// Internal result from generate_reply.
#[derive(Debug)]
struct GenerateReplyResult {
    reply_sent_by_tool: bool,
    reply_text: Option<String>,
    #[allow(dead_code)]
    model_id: Option<String>,
    #[allow(dead_code)]
    provider_id: Option<String>,
    /// The actual mode OpenCode used (from SSE message.updated)
    #[allow(dead_code)]
    mode: Option<String>,
}

/// Extract text content from accumulated response parts.
/// Filters out prompt echo parts (per-part, not combined).
fn extract_text_from_parts(parts: &[ResponsePart]) -> Option<String> {
    let text: String = parts
        .iter()
        .filter(|p| p.part_type == "text")
        .filter_map(|p| p.text.as_deref())
        .filter(|t| !t.is_empty())
        .filter(|t| !is_prompt_echo(t))
        .collect::<Vec<_>>()
        .join("\n");

    if text.trim().is_empty() { None } else { Some(text.trim().to_string()) }
}

/// Check if a text part is a prompt echo (AI repeating the prompt).
fn is_prompt_echo(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with("## Incoming Message")
        || trimmed.starts_with("## Conversation history")
        || trimmed.starts_with("<system-reminder>")
}

/// Extract the reply text from the MCP reply tool's input in the SSE parts.
///
/// When the reply_message tool completes successfully, the SSE tool part contains
/// the tool's input with a `message` field holding the full reply text.
/// This allows the monitor to deliver the reply without reading from disk.
fn extract_reply_text_from_tool_parts(parts: &[ResponsePart]) -> Option<String> {
    parts.iter()
        .find(|p| {
            p.part_type == "tool"
                && p.tool.as_deref().map(|t| t.contains("reply_message")).unwrap_or(false)
                && p.state.as_ref().is_some_and(|s| s.status == "completed")
        })
        .and_then(|p| p.state.as_ref())
        .and_then(|s| s.input.as_ref())
        .and_then(|input| input.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string())
}
