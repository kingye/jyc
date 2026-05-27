//! Bash tool — execute shell commands.
//!
//! ## Security boundary
//!
//! `bash` applies a **best-effort heuristic check** before execution: it
//! scans the command string for absolute-path tokens (words starting with
//! `/`). Each such token is resolved against `working_dir` and passed
//! through `ToolContext::check_path_boundary`. This catches obvious escape
//! attempts like `cat /etc/passwd`, `ls /root`, or `rm -rf /tmp/evil`.
//!
//! **Limitations**: this is a simple lexer heuristic, not a full sandbox.
//! Bash is inherently capable of arbitrary system access via shell
//! builtins (`cd /`, variable expansion, process substitution, etc.).
//! A determined attacker can bypass it. Full sandboxing requires OS-level
//! isolation (Linux namespaces, chroot, containers).

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

        // Best-effort boundary check: scan for absolute-path tokens and
        // verify each is within the working directory. This catches obvious
        // escape attempts (cat /etc/passwd) without fully sandboxing bash.
        for token in command.split_whitespace() {
            if token.starts_with('/') {
                let candidate = std::path::Path::new(token);
                // Only check tokens that start with `/` and correspond to
                // existing filesystem paths. This catches obvious escape
                // attempts (e.g. `cat /etc/passwd`) while allowing flags
                // like `-C` and commands that shell out to relative paths.
                if candidate.exists()
                    && let Err(msg) = ctx.check_path_boundary(token, candidate)
                {
                    return Ok(ToolOutput::error(msg));
                }
            }
        }

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
