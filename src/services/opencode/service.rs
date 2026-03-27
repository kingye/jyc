use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::Instrument;

use super::client::{OpenCodeClient, SseResult};
use super::types::*;
use super::{session, prompt_builder, OpenCodeServer};
use crate::channels::types::InboundMessage;
use crate::config::types::AgentConfig;
use crate::services::agent::{AgentResult, AgentService};

/// Encapsulates all OpenCode AI interaction logic.
///
/// Channel-agnostic — does NOT know about email, SMTP, or reply formatting.
/// Returns raw AI text. The outbound adapter handles formatting + sending + storing.
pub struct OpenCodeService {
    server: Arc<OpenCodeServer>,
    agent_config: Arc<AgentConfig>,
    workdir: PathBuf,
}

impl OpenCodeService {
    pub fn new(
        server: Arc<OpenCodeServer>,
        agent_config: Arc<AgentConfig>,
        workdir: PathBuf,
    ) -> Self {
        Self {
            server,
            agent_config,
            workdir,
        }
    }

    /// Internal: generate AI reply via OpenCode SSE streaming.
    async fn generate_reply(
        &self,
        message: &InboundMessage,
        thread_name: &str,
        thread_path: &Path,
        message_dir: &str,
    ) -> Result<GenerateReplyResult> {
        let ch = &message.channel;

        // 1. Ensure OpenCode server is running
        let base_url = self.server.base_url().await?;
        let client = OpenCodeClient::new(&base_url);

        // 2. Ensure thread has opencode.json
        let config_changed = session::ensure_thread_opencode_setup(
            thread_path,
            &self.agent_config,
            &self.workdir,
        ).await?;

        if config_changed {
            tracing::info!("opencode.json changed");
            session::delete_session(thread_path).await?;
        }

        // 3. Get or create session
        let session_id = session::get_or_create_session(&client, thread_path).await?;

        // 4. Clean up stale signal file
        session::cleanup_signal_file(thread_path).await;

        // 5. Build prompts
        let system_prompt = prompt_builder::build_system_prompt(
            thread_path,
            self.agent_config.opencode.as_ref().and_then(|o| o.system_prompt.as_deref()),
        ).await;

        let user_prompt = prompt_builder::build_prompt(
            message,
            thread_path,
            message_dir,
        ).await?;

        // 6. Mode override (plan/build)
        let mode_override = session::read_mode_override(thread_path).await;
        let agent_mode = if mode_override.as_deref() == Some("plan") {
            Some("plan".to_string())
        } else {
            None
        };

        let mode_label = agent_mode.as_deref().unwrap_or("build").to_string();

        let request = PromptRequest {
            system: system_prompt,
            agent: agent_mode,
            parts: vec![PromptPart::Text { text: user_prompt }],
        };

        // 7. Send prompt via SSE streaming with ai{m=model:mode} span
        let ai_span = tracing::info_span!("ai", m = tracing::field::Empty);
        ai_span.record("m", format!(":{}", mode_label));

        tracing::info!(
            session_id = %session_id,
            "Sending prompt to OpenCode"
        );

        let sse_result = client
            .prompt_with_sse(&session_id, thread_path, &request, &mode_label)
            .instrument(ai_span.clone())
            .await;

        // 8. Handle result
        let result = match sse_result {
            Ok(result) => {
                self.handle_sse_result(
                    result, thread_name, thread_path,
                    &client, &session_id, &request, &mode_label,
                ).await?
            }
            Err(e) => {
                tracing::error!(error = %e, "SSE streaming failed, trying blocking fallback");
                let blocking_result = client
                    .prompt_blocking(&session_id, thread_path, &request)
                    .await?;
                self.handle_blocking_result(
                    blocking_result, thread_name, thread_path,
                    &client, &session_id, &request,
                ).await?
            }
        };

        session::update_session_timestamp(thread_path).await.ok();

        Ok(result)
    }

    /// Handle SSE streaming result.
    async fn handle_sse_result(
        &self,
        result: SseResult,
        thread_name: &str,
        thread_path: &Path,
        client: &OpenCodeClient,
        session_id: &str,
        request: &PromptRequest,
        mode_label: &str,
    ) -> Result<GenerateReplyResult> {
        // ContextOverflow recovery
        if let Some(ref error) = result.error {
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
            tracing::info!("Reply sent by MCP tool");
            return Ok(GenerateReplyResult {
                reply_sent_by_tool: true,
                reply_text: None,
                model_id: result.model_id,
                provider_id: result.provider_id,
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
            let retry = client.prompt_with_sse(&new_id, thread_path, request, mode_label).await?;
            let sent = retry.reply_sent_by_tool || session::check_signal_file(thread_path).await;
            if sent {
                return Ok(GenerateReplyResult {
                    reply_sent_by_tool: true, reply_text: None,
                    model_id: retry.model_id, provider_id: retry.provider_id,
                });
            }
            return Ok(GenerateReplyResult {
                reply_sent_by_tool: false,
                reply_text: extract_text_from_parts(&retry.parts),
                model_id: retry.model_id, provider_id: retry.provider_id,
            });
        }

        // Timeout
        if result.timed_out {
            if session::check_signal_file(thread_path).await {
                return Ok(GenerateReplyResult {
                    reply_sent_by_tool: true, reply_text: None,
                    model_id: result.model_id, provider_id: result.provider_id,
                });
            }
            tracing::error!("Timed out with no reply");
            return Ok(GenerateReplyResult {
                reply_sent_by_tool: false, reply_text: None,
                model_id: result.model_id, provider_id: result.provider_id,
            });
        }

        // Fallback: extract text
        Ok(GenerateReplyResult {
            reply_sent_by_tool: false,
            reply_text: extract_text_from_parts(&result.parts),
            model_id: result.model_id,
            provider_id: result.provider_id,
        })
    }

    /// Handle blocking prompt result.
    async fn handle_blocking_result(
        &self,
        result: PromptResponse,
        thread_name: &str,
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
            return Ok(GenerateReplyResult {
                reply_sent_by_tool: true, reply_text: None,
                model_id: None, provider_id: None,
            });
        }

        let parts = result.data.map(|d| d.parts).unwrap_or_default();
        Ok(GenerateReplyResult {
            reply_sent_by_tool: false,
            reply_text: extract_text_from_parts(&parts),
            model_id: None, provider_id: None,
        })
    }
}

#[async_trait]
impl AgentService for OpenCodeService {
    async fn process(
        &self,
        message: &InboundMessage,
        thread_name: &str,
        thread_path: &Path,
        message_dir: &str,
    ) -> Result<AgentResult> {
        let result = self.generate_reply(message, thread_name, thread_path, message_dir).await?;

        Ok(AgentResult {
            reply_sent_by_tool: result.reply_sent_by_tool,
            reply_text: result.reply_text,
        })
    }
}

/// Internal result from generate_reply.
#[derive(Debug)]
struct GenerateReplyResult {
    reply_sent_by_tool: bool,
    reply_text: Option<String>,
    model_id: Option<String>,
    provider_id: Option<String>,
}

/// Extract text content from accumulated response parts.
fn extract_text_from_parts(parts: &[ResponsePart]) -> Option<String> {
    let text: String = parts
        .iter()
        .filter(|p| p.part_type == "text")
        .filter_map(|p| p.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n");

    let cleaned = strip_prompt_echo(&text);
    if cleaned.trim().is_empty() { None } else { Some(cleaned) }
}

/// Strip prompt artifacts that the AI may echo back.
fn strip_prompt_echo(text: &str) -> String {
    let markers = ["## Incoming Message", "REPLY_TOKEN=", "## Conversation history"];
    let mut end = text.len();
    for marker in &markers {
        if let Some(pos) = text.find(marker) {
            if pos < end { end = pos; }
        }
    }
    text[..end].trim().to_string()
}
