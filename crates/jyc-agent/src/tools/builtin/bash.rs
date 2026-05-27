//! Bash tool — execute shell commands.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;
use tracing;

use super::super::{Tool, ToolContext, ToolOutput};

/// Maximum output size before truncation (128KB).
const MAX_OUTPUT_SIZE: usize = 128 * 1024;

/// Default timeout for bash commands (2 minutes).
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Bash tool — executes shell commands in the working directory.
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command in the working directory. Use for running tests, builds, git commands, \
         file operations, and any system commands. Commands run with a 2-minute timeout by default."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional timeout in seconds (default: 120)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let command = input
            .get("command")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;

        let timeout_secs = input
            .get("timeout")
            .and_then(|t| t.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        tracing::debug!(command = %command, timeout = timeout_secs, "Executing bash command");

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            Command::new("bash")
                .arg("-c")
                .arg(command)
                .current_dir(ctx.working_dir)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                let mut result_text = String::new();

                if !stdout.is_empty() {
                    result_text.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result_text.is_empty() {
                        result_text.push('\n');
                    }
                    result_text.push_str("STDERR:\n");
                    result_text.push_str(&stderr);
                }

                // Truncate if too large
                if result_text.len() > MAX_OUTPUT_SIZE {
                    result_text.truncate(MAX_OUTPUT_SIZE);
                    result_text.push_str("\n... [output truncated]");
                }

                if result_text.is_empty() {
                    result_text = format!(
                        "Command completed with exit code {}",
                        output.status.code().unwrap_or(-1)
                    );
                }

                let is_error = !output.status.success();
                if is_error {
                    let exit_code = output.status.code().unwrap_or(-1);
                    result_text = format!("Exit code: {}\n{}", exit_code, result_text);
                }

                Ok(if is_error {
                    ToolOutput::error(result_text)
                } else {
                    ToolOutput::success(result_text)
                })
            }
            Ok(Err(e)) => Ok(ToolOutput::error(format!("Failed to execute command: {e}"))),
            Err(_) => Ok(ToolOutput::error(format!(
                "Command timed out after {}s",
                timeout_secs
            ))),
        }
    }
}
