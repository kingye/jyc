//! AgentService implementation using the in-process agent loop.
//!
//! Replaces the OpenCode HTTP/SSE client with direct LLM calls and tool execution.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::Path;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing;

use jyc_core::agent::{AgentResult, AgentService};
use jyc_core::thread_event_bus::ThreadEventBusRef;
use jyc_types::{InboundMessage, QueueItem};

use crate::agent_loop;
use crate::provider::{self, Provider};
use crate::tools::registry::ToolRegistry;
use crate::types::AgentConfig;

/// In-process AI agent service.
///
/// Implements `AgentService` by running LLM inference and tool execution
/// directly in-process (no external OpenCode server needed).
pub struct JycAgentService {
    config: AgentConfig,
}

impl JycAgentService {
    /// Create a new agent service with the given configuration.
    pub fn new(config: AgentConfig) -> Self {
        Self { config }
    }

    /// Build the system prompt for a thread.
    fn build_system_prompt(&self, thread_path: &Path) -> String {
        let mut prompt = String::new();

        // Security: directory boundaries
        prompt.push_str(&format!(
            "Your working directory is \"{}\". You MUST only read, write, and access files within this directory.\n\n",
            thread_path.display()
        ));

        // Load AGENTS.md if present in the working directory
        let agents_md = thread_path.join("AGENTS.md");
        if agents_md.exists() {
            if let Ok(content) = std::fs::read_to_string(&agents_md) {
                prompt.push_str("## Project Instructions (from AGENTS.md)\n\n");
                prompt.push_str(&content);
                prompt.push_str("\n\n");
            }
        }

        // Reply instructions
        prompt.push_str(
            "## Reply Instructions\n\
             When you have your answer ready, use the jyc_reply_reply_message tool:\n\
             - `message`: Your reply text\n\
             - `attachments`: Optional filenames to attach from the working directory\n\
             After a successful reply, STOP immediately. Do NOT call any other tools.\n\
             CRITICAL: Always use the jyc_reply_reply_message tool to send your reply.\n"
        );

        prompt
    }

    /// Build the user prompt from an inbound message.
    fn build_user_prompt(&self, message: &InboundMessage) -> String {
        let mut prompt = String::new();

        prompt.push_str("## Incoming Message\n");
        prompt.push_str(&format!("**From:** {} <{}>\n", message.sender, message.sender_address));
        prompt.push_str(&format!("**Subject:** {}\n", message.topic));
        prompt.push_str(&format!("**Date:** {}\n\n", message.timestamp.to_rfc3339()));

        // Body
        let body = message
            .content
            .text
            .as_deref()
            .or(message.content.markdown.as_deref())
            .unwrap_or("[no text content]");

        prompt.push_str(body);
        prompt
    }

    /// Create the tool registry for a thread.
    fn build_tool_registry(&self, _thread_path: &Path) -> ToolRegistry {
        let registry = ToolRegistry::new();

        // TODO: Register built-in tools (bash, read, write, edit, glob, grep, webfetch)
        // TODO: Register MCP bridge tools (reply_message, vision, question)

        registry
    }

    /// Get or create the provider for the current model.
    fn create_provider(&self, model_override: Option<&str>) -> Result<Box<dyn Provider>> {
        let model = model_override
            .or(self.config.model.as_deref())
            .ok_or_else(|| anyhow::anyhow!("No model configured. Set [agent].model in config.toml"))?;

        provider::create_provider(model, &self.config.providers)
    }
}

#[async_trait]
impl AgentService for JycAgentService {
    async fn base_url(&self) -> Result<String> {
        // Not applicable for in-process agent
        Ok("in-process".to_string())
    }

    async fn process(
        &self,
        message: &InboundMessage,
        thread_name: &str,
        thread_path: &Path,
        message_dir: &str,
        _pending_rx: &mut mpsc::Receiver<QueueItem>,
        thread_cancel: CancellationToken,
    ) -> Result<AgentResult> {
        tracing::info!(
            thread = %thread_name,
            message_dir = %message_dir,
            "Processing message with in-process agent"
        );

        // 1. Read model override if present
        let model_override_path = thread_path.join(".jyc").join("model-override");
        let model_override = if model_override_path.exists() {
            tokio::fs::read_to_string(&model_override_path)
                .await
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        } else {
            None
        };

        // 2. Create provider
        let provider = self.create_provider(model_override.as_deref())
            .context("Failed to create LLM provider")?;

        tracing::info!(
            provider = %provider.name(),
            model = %provider.model(),
            "Using provider"
        );

        // 3. Build prompts
        let system_prompt = self.build_system_prompt(thread_path);
        let user_prompt = self.build_user_prompt(message);

        // 4. Build tool registry
        let tools = self.build_tool_registry(thread_path);

        // 5. Run agent loop
        let result = agent_loop::run(
            provider.as_ref(),
            &tools,
            &system_prompt,
            &user_prompt,
            thread_path,
            thread_cancel,
        )
        .await?;

        tracing::info!(
            reply_sent_by_tool = result.reply_sent_by_tool,
            text_len = result.text.len(),
            input_tokens = result.input_tokens,
            output_tokens = result.output_tokens,
            "Agent loop completed"
        );

        // 6. Return result
        if result.reply_sent_by_tool {
            Ok(AgentResult {
                reply_sent_by_tool: true,
                reply_text: result.reply_text_from_tool,
            })
        } else {
            Ok(AgentResult {
                reply_sent_by_tool: false,
                reply_text: if result.text.is_empty() { None } else { Some(result.text) },
            })
        }
    }

    async fn set_thread_event_bus(&self, _thread_name: &str, _event_bus: Option<ThreadEventBusRef>) {
        // TODO: Use event bus for progress reporting
    }
}
