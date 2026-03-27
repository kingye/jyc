use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::client::{OpenCodeClient, SseResult};
use super::types::*;
use super::{session, prompt_builder, OpenCodeServer};
use crate::channels::email::outbound::EmailOutboundAdapter;
use crate::channels::types::InboundMessage;
use crate::config::types::AgentConfig;
use crate::core::email_parser;
use crate::core::message_storage::MessageStorage;
use crate::services::agent::{AgentResult, AgentService};

/// Encapsulates all OpenCode AI interaction logic.
///
/// Owns: server lifecycle, sessions, prompts, SSE streaming, error recovery,
/// reply building (with quoted history), sending (via tool or fallback), and storage.
pub struct OpenCodeService {
    server: Arc<OpenCodeServer>,
    agent_config: Arc<AgentConfig>,
    storage: Arc<MessageStorage>,
    outbound: Arc<EmailOutboundAdapter>,
    workdir: PathBuf,
}

impl OpenCodeService {
    pub fn new(
        server: Arc<OpenCodeServer>,
        agent_config: Arc<AgentConfig>,
        storage: Arc<MessageStorage>,
        outbound: Arc<EmailOutboundAdapter>,
        workdir: PathBuf,
    ) -> Self {
        Self {
            server,
            agent_config,
            storage,
            outbound,
            workdir,
        }
    }

    /// Internal: generate AI reply via OpenCode SSE streaming.
    /// Returns the raw result (reply_sent_by_tool, reply_text).
    async fn generate_reply(
        &self,
        message: &InboundMessage,
        thread_name: &str,
        thread_path: &Path,
        message_dir: &str,
    ) -> Result<GenerateReplyResult> {
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
            tracing::info!(thread = %thread_name, "opencode.json changed");
            // Config changed — delete old session so a new one picks up the new config
            session::delete_session(thread_path).await?;
        }

        // 3. Get or create session (reuse existing if still valid on server)
        let session_id = session::get_or_create_session(&client, thread_path).await?;

        // 4. Clean up stale signal file
        session::cleanup_signal_file(thread_path).await;

        // 5. Build prompts
        let include_history = self.agent_config
            .opencode
            .as_ref()
            .map(|o| o.include_thread_history)
            .unwrap_or(true);

        let system_prompt = prompt_builder::build_system_prompt(
            thread_path,
            self.agent_config.opencode.as_ref().and_then(|o| o.system_prompt.as_deref()),
        ).await;

        let user_prompt = prompt_builder::build_prompt(
            message,
            thread_path,
            message_dir,
            include_history,
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

        // 7. Send prompt via SSE streaming
        tracing::info!(
            thread = %thread_name,
            session_id = %session_id,
            mode = %mode_label,
            "Sending prompt to OpenCode..."
        );

        let sse_result = client
            .prompt_with_sse(&session_id, thread_path, &request, None)
            .await;

        // 8. Handle result
        let result = match sse_result {
            Ok(result) => {
                self.handle_sse_result(
                    result, thread_name, thread_path,
                    &client, &session_id, &request,
                ).await?
            }
            Err(e) => {
                tracing::error!(
                    thread = %thread_name,
                    error = %e,
                    "SSE streaming failed, trying blocking fallback"
                );
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
    ) -> Result<GenerateReplyResult> {
        // ContextOverflow recovery
        if let Some(ref error) = result.error {
            if error.contains("ContextOverflow") {
                tracing::warn!(thread = %thread_name, "ContextOverflow — new session + retry");
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
            tracing::info!(thread = %thread_name, model = ?result.model_id, "Reply sent by MCP tool");
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
            tracing::warn!(thread = %thread_name, "Stale session — retry");
            session::delete_session(thread_path).await?;
            let new_id = session::create_new_session(client, thread_path).await?;
            session::cleanup_signal_file(thread_path).await;
            let retry = client.prompt_with_sse(&new_id, thread_path, request, None).await?;
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
            tracing::error!(thread = %thread_name, "Timed out with no reply");
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
                    tracing::error!(thread = %thread_name, error = %error.name, "Blocking prompt error");
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

        if result.reply_sent_by_tool {
            return Ok(AgentResult {
                reply_sent: true,
                summary: format!("Reply sent by MCP tool (model: {:?})", result.model_id),
            });
        }

        // Fallback: build full reply with quoted history and send
        if let Some(ref text) = result.reply_text {
            tracing::info!(
                thread = %thread_name,
                text_len = text.len(),
                "Fallback: building full reply with quoted history"
            );

            let body_text = message
                .content
                .text
                .as_deref()
                .or(message.content.markdown.as_deref())
                .unwrap_or("");

            let full_reply = email_parser::build_full_reply_text(
                text,
                thread_path,
                &message.sender,
                &message.timestamp.to_rfc3339(),
                &message.topic,
                body_text,
                message_dir,
            )
            .await;

            self.outbound.send_reply(message, &full_reply, None).await?;
            self.storage
                .store_reply(thread_path, &full_reply, message_dir)
                .await?;

            tracing::info!(thread = %thread_name, "Fallback reply sent");

            Ok(AgentResult {
                reply_sent: true,
                summary: "Fallback reply sent".to_string(),
            })
        } else {
            tracing::warn!(thread = %thread_name, "No reply text from AI");
            Ok(AgentResult {
                reply_sent: false,
                summary: "No reply text from AI".to_string(),
            })
        }
    }
}

/// Internal result from generate_reply (before AgentService wrapping).
#[derive(Debug)]
struct GenerateReplyResult {
    reply_sent_by_tool: bool,
    reply_text: Option<String>,
    model_id: Option<String>,
    provider_id: Option<String>,
}

/// Extract text content from accumulated response parts.
/// Strips prompt echoes that the AI may include when the reply tool fails.
fn extract_text_from_parts(parts: &[ResponsePart]) -> Option<String> {
    let text: String = parts
        .iter()
        .filter(|p| p.part_type == "text")
        .filter_map(|p| p.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n");

    let cleaned = strip_prompt_echo(&text);

    if cleaned.trim().is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Strip prompt artifacts that the AI may echo back when the reply tool fails.
fn strip_prompt_echo(text: &str) -> String {
    let markers = [
        "## Incoming Message",
        "<reply_context>",
        "## Conversation history",
    ];

    let mut end = text.len();
    for marker in &markers {
        if let Some(pos) = text.find(marker) {
            if pos < end {
                end = pos;
            }
        }
    }

    text[..end].trim().to_string()
}
