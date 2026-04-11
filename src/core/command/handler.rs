use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::types::AppConfig;

/// Context passed to a command handler during execution.
#[derive(Clone)]
pub struct CommandContext {
    /// Command arguments (everything after the command name)
    pub args: Vec<String>,
    /// Path to the thread directory
    pub thread_path: PathBuf,
    /// Application configuration
    pub config: Arc<AppConfig>,
    /// Channel name
    pub channel: String,
    /// Agent service (optional, for commands that need to query server)
    pub agent: Option<Arc<dyn crate::services::agent::AgentService>>,
    /// Template directory path
    pub template_dir: PathBuf,
}

impl std::fmt::Debug for CommandContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandContext")
            .field("args", &self.args)
            .field("thread_path", &self.thread_path)
            .field("config", &self.config)
            .field("channel", &self.channel)
            .field("agent", &self.agent.is_some())
            .finish()
    }
}

/// Result of executing a command.
#[derive(Debug, Clone)]
pub struct CommandResult {
    /// Whether the command succeeded
    pub success: bool,
    /// User-facing result message
    pub message: String,
    /// Error message (if !success)
    pub error: Option<String>,
    /// Whether the OpenCode server needs to be restarted
    #[allow(dead_code)]
    pub requires_restart: bool,
}

/// Output of unified command processing (parse + execute + strip).
#[derive(Debug)]
pub struct CommandOutput {
    /// Results from all executed commands
    pub results: Vec<CommandResult>,
    /// Message body with command lines stripped
    pub cleaned_body: String,
    /// Whether the body was empty after stripping (command-only message)
    #[allow(dead_code)]
    pub body_empty: bool,
}

impl CommandOutput {
    /// Format results as a summary string for direct reply.
    pub fn results_summary(&self) -> String {
        self.results
            .iter()
            .map(|r| {
                if r.success {
                    r.message.clone()
                } else {
                    format!(
                        "Error: {}",
                        r.error.as_deref().unwrap_or(&r.message)
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether any command requires an OpenCode server restart.
    #[allow(dead_code)]
    pub fn requires_restart(&self) -> bool {
        self.results.iter().any(|r| r.requires_restart)
    }
}

/// Trait for command handlers (e.g., /model, /plan, /build).
///
/// Each handler is registered in the CommandRegistry by name.
#[async_trait]
pub trait CommandHandler: Send + Sync {
    /// Command name including the slash (e.g., "/model")
    fn name(&self) -> &str;

    /// Short description of the command
    #[allow(dead_code)]
    fn description(&self) -> &str;

    /// Execute the command with the given context.
    async fn execute(&self, context: CommandContext) -> Result<CommandResult>;
}
