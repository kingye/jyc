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
        // verify each is within the working directory (or configured
        // read/write roots). This catches obvious escape attempts
        // (cat /etc/passwd) without fully sandboxing bash.
        //
        // Only tokens OUTSIDE of quoted strings are checked. This avoids
        // false positives on path-like substrings inside data arguments
        // (e.g., `//` in a markdown comment passed via `--body "..."`).
        for token in unquoted_path_tokens(command) {
            let candidate = std::path::Path::new(&token);
            // Only check tokens that correspond to existing filesystem
            // paths. This catches obvious escape attempts (e.g.
            // `cat /etc/passwd`) while allowing flags like `-C`.
            if candidate.exists()
                && let Err(msg) = ctx.check_write_boundary(&token, candidate)
            {
                return Ok(ToolOutput::error(msg));
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

/// Extract absolute-path tokens (starting with `/`) from the **unquoted**
/// portions of a bash command string.
///
/// Bash quoting rules handled:
/// - Single quotes `'...'`: everything inside is literal; the closing `'`
///   is the next `'` character.
/// - Double quotes `"..."`: everything inside is literal except `\"`
///   (escaped quote) which does not close the string.
/// - Backslash outside quotes: escapes the next character (e.g., `\"`
///   produces a literal `"` without opening a quoted region).
///
/// Tokens inside quotes are **skipped** to avoid false positives on
/// path-like substrings in data arguments (e.g., `//` in a markdown
/// comment passed via `--body "..."`).
fn unquoted_path_tokens(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_single {
            // Inside single quotes: only `'` ends the region.
            if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        if in_double {
            // Inside double quotes: `\"` is an escaped quote, everything
            // else (including `\`) is literal until the closing `"`.
            if ch == '\\' {
                chars.next(); // skip escaped char
                continue;
            }
            if ch == '"' {
                in_double = false;
            }
            continue;
        }
        // Outside quotes
        match ch {
            '\'' => {
                // Flush current token before entering single-quote region
                if !current.is_empty() {
                    if current.starts_with('/') {
                        tokens.push(std::mem::take(&mut current));
                    } else {
                        current.clear();
                    }
                }
                in_single = true;
            }
            '"' => {
                if !current.is_empty() {
                    if current.starts_with('/') {
                        tokens.push(std::mem::take(&mut current));
                    } else {
                        current.clear();
                    }
                }
                in_double = true;
            }
            '\\' => {
                // Escaped char outside quotes — skip next char
                chars.next();
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    if current.starts_with('/') {
                        tokens.push(std::mem::take(&mut current));
                    } else {
                        current.clear();
                    }
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }
    // Flush trailing token
    if !current.is_empty() && current.starts_with('/') {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquoted_absolute_path_detected() {
        let tokens = unquoted_path_tokens("cat /etc/passwd");
        assert_eq!(tokens, vec!["/etc/passwd"]);
    }

    #[test]
    fn quoted_path_skipped() {
        // Path inside double quotes should not be extracted
        let tokens = unquoted_path_tokens(r#"echo "// comment""#);
        assert!(tokens.is_empty(), "expected no tokens, got {:?}", tokens);
    }

    #[test]
    fn single_quoted_path_skipped() {
        let tokens = unquoted_path_tokens(r#"echo '// comment'"#);
        assert!(tokens.is_empty(), "expected no tokens, got {:?}", tokens);
    }

    #[test]
    fn markdown_body_with_double_slash_skipped() {
        // Simulates: gh pr comment --body "Review\n\n### // comment\n..."
        let cmd = r###"gh pr comment 273 --body "## Review\n\n### // comment\n\nDetails here.""###;
        let tokens = unquoted_path_tokens(cmd);
        assert!(
            !tokens.contains(&"//".to_string()),
            "double-slash inside quotes must not be extracted, got {:?}",
            tokens
        );
    }

    #[test]
    fn unquoted_path_after_quoted_string_detected() {
        let tokens = unquoted_path_tokens(r#"echo "hello" /etc/passwd"#);
        assert_eq!(tokens, vec!["/etc/passwd"]);
    }

    #[test]
    fn escaped_quote_outside_does_not_open_region() {
        // `\"` outside quotes is a literal quote, not a string opener
        let tokens = unquoted_path_tokens(r#"echo \"hello\" /tmp"#);
        assert!(tokens.contains(&"/tmp".to_string()), "got {:?}", tokens);
    }

    #[test]
    fn escaped_quote_inside_double_quotes() {
        // `\"` inside double quotes does not close the string
        let tokens = unquoted_path_tokens(r#"echo "he said \"hi\"" /var/log"#);
        assert_eq!(tokens, vec!["/var/log"]);
    }

    #[test]
    fn multiple_unquoted_paths_detected() {
        let tokens = unquoted_path_tokens("cp /etc/hosts /tmp/backup");
        assert_eq!(tokens, vec!["/etc/hosts", "/tmp/backup"]);
    }

    #[test]
    fn relative_paths_ignored() {
        let tokens = unquoted_path_tokens("cd repo && gh issue view 197");
        assert!(tokens.is_empty());
    }

    #[test]
    fn mixed_quoted_and_unquoted() {
        let cmd = r###"cd repo && gh pr comment 273 --body "see // here" && cat /etc/hostname"###;
        let tokens = unquoted_path_tokens(cmd);
        assert_eq!(tokens, vec!["/etc/hostname"]);
    }
}
