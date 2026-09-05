use anyhow::Result;
use async_trait::async_trait;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use super::mode_handler::set_mode;
use jyc_types::CustomCommand;

/// Hard cap on shell-command output (stdout + stderr combined) before
/// truncation. Same order of magnitude as the agent's `bash` tool — small
/// enough that a runaway loop cannot flood the chat transcript, large
/// enough for `cargo test`, `git log`, etc. to come through intact.
const SHELL_MAX_OUTPUT_BYTES: usize = 8 * 1024;

/// Hard ceiling on a single shell-command invocation. Anything still running
/// after this is killed (the child process gets a `SIGKILL` via
/// [`tokio::process::Child::start_kill`] in the timeout arm of
/// [`CustomCommandHandler::execute_shell`]) and the result surfaces as
/// `Error: timed out after 30s`.
///
/// ponytail: no per-command override — wrap with a script
/// (`shell = ["./scripts/long.sh"]`) if you need a different ceiling.
///
/// `pub(crate)` so the timeout test can override it with a tight deadline
/// rather than wait the full 30s per CI run.
pub(crate) const SHELL_TIMEOUT_SECS: u64 = 30;

/// Handler for a user-defined command declared in `config.toml` `[[commands]]`.
///
/// Two flavors, selected by the config:
/// - **Prompt-injection** (default): switches topic mode (when set), then
///   injects the command's `user_prompt` + same-line args into the message
///   body via [`CommandResult::append_body`], so the agent runs the prompt
///   through the LLM (incurs tokens).
/// - **Direct shell execution** (when `shell` is set): runs the argv via
///   `tokio::process::Command` and returns the captured output as the reply.
///   **No LLM is involved, so no tokens are spent.** User args typed after
///   the command are appended to the argv.
///
/// Skill *paths* are not resolved here: the system prompt already lists every
/// discovered skill with its path and description, so naming the skills is
/// enough for the agent to locate and read the right `SKILL.md`. Skills and
/// mode are silently ignored on shell commands (no agent to apply them to).
pub struct CustomCommandHandler {
    /// Command name including the leading slash (e.g. `/review`).
    name: String,
    config: CustomCommand,
}

impl CustomCommandHandler {
    /// Create a handler for `config`, normalizing the name to a lowercase,
    /// slash-prefixed form.
    ///
    /// Lowercasing is required: [`CommandRegistry::process_commands`] lowercases
    /// the incoming command before looking it up, so a handler registered under
    /// a name containing uppercase could never be found. Config validation
    /// rejects uppercase names outright, so this is belt-and-braces to keep the
    /// invariant true regardless of how the handler is constructed.
    ///
    /// [`CommandRegistry::process_commands`]: super::registry::CommandRegistry::process_commands
    pub fn new(config: CustomCommand) -> Self {
        let name = format!(
            "/{}",
            config.name.trim().trim_start_matches('/').to_lowercase()
        );
        Self { name, config }
    }

    /// Build the text appended to the message body: any same-line arguments,
    /// then a skills directive (when the command names skills), then the
    /// `user_prompt`.
    ///
    /// `args` are the words typed after the command on the same line. The
    /// registry consumes the whole command line, so without this they would be
    /// silently dropped. Placing them first makes `/review focus on X`
    /// equivalent to typing `focus on X` on the line below the command.
    fn build_append_body(&self, args: &[String]) -> String {
        let prompt = self
            .config
            .user_prompt
            .as_deref()
            .expect("user_prompt is set when shell is None (enforced by validation)");

        let mut out = String::new();

        if !args.is_empty() {
            out.push_str(&args.join(" "));
            out.push_str("\n\n");
        }

        if let Some(skills) = self.config.skills.as_ref().filter(|s| !s.is_empty()) {
            out.push_str(&format!(
                "For this task, use these skills: {}.\n\
                 Read their SKILL.md before starting.\n\n",
                skills.join(", ")
            ));
        }

        out.push_str(prompt.trim());
        out
    }

    /// Run the command's `shell` argv with `user_args` appended, returning
    /// a `CommandResult` whose `message` (success) or `error` (failure) holds
    /// the captured stdout/stderr. `mode`/`skills` are ignored — they are an
    /// LLM concept and this path does not reach the LLM.
    ///
    /// Plumbing: `Command::output()` and `wait_with_output()` both consume
    /// the `Child`, which means we can't kill it on timeout. Dropping the
    /// future they return reaps the zombie but does **not** kill the
    /// process — it would keep running orphaned. So we spawn with
    /// `Stdio::piped()`, take stdout/stderr handles, and wrap
    /// `child.wait()` in `tokio::time::timeout` — when the timeout fires
    /// the inner `wait` future is dropped (releasing its `&mut child`
    /// borrow), so we can then `start_kill` + `wait` to reap.
    async fn execute_shell(
        &self,
        argv: &[String],
        user_args: &[String],
        timeout: Duration,
    ) -> Result<CommandResult> {
        let mut cmd = Command::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }
        if !user_args.is_empty() {
            cmd.args(user_args);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Inherit jyc's cwd — predictable default. Per-command cwd is a
        // matrix-bomb: most callers don't think about cwd, and a surprising
        // one silently breaks them. Wrap with `cd /path && cmd` in a script
        // (`shell = ["./scripts/deploy.sh"]`) when a different directory
        // matters.

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(CommandResult {
                    success: false,
                    message: String::new(),
                    error: Some(format!("failed to spawn: {e}")),
                    append_body: None,
                });
            }
        };

        // Take stdout/stderr handles before any awaits. They close when
        // the child exits (including after we SIGKILL it).
        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                // wait itself failed (IO error). Kill and reap so we don't
                // leak an orphan, then surface the IO error.
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Ok(CommandResult {
                    success: false,
                    message: String::new(),
                    error: Some(format!("failed to read output: {e}")),
                    append_body: None,
                });
            }
            Err(_) => {
                // Timeout fired: tokio::time::timeout has dropped the
                // inner `wait` future, releasing the `&mut child` borrow,
                // so we can now kill and reap.
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Ok(CommandResult {
                    success: false,
                    message: String::new(),
                    error: Some(format!("timed out after {}s", timeout.as_secs())),
                    append_body: None,
                });
            }
        };

        // Drain remaining stdout/stderr. After wait() returned, the pipes
        // are closed (child exited); read_to_end yields EOF.
        let mut stdout_buf = Vec::new();
        if let Some(mut s) = stdout_handle {
            let _ = s.read_to_end(&mut stdout_buf).await;
        }
        let mut stderr_buf = Vec::new();
        if let Some(mut s) = stderr_handle {
            let _ = s.read_to_end(&mut stderr_buf).await;
        }

        let stdout = String::from_utf8_lossy(&stdout_buf);
        let stderr = String::from_utf8_lossy(&stderr_buf);

        let mut body = String::new();
        if !stdout.is_empty() {
            body.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str("STDERR:\n");
            body.push_str(&stderr);
        }

        let original_len = body.len();
        if body.len() > SHELL_MAX_OUTPUT_BYTES {
            body.truncate(SHELL_MAX_OUTPUT_BYTES);
            body.push_str(&format!(
                "\n... [{} more bytes]",
                original_len - SHELL_MAX_OUTPUT_BYTES
            ));
        }

        if status.success() {
            // Empty `message` on no-output success keeps the format
            // consistent with other slash commands (e.g. `/model`), which
            // leave `message` empty when there's nothing to say.
            Ok(CommandResult {
                success: true,
                message: body,
                error: None,
                append_body: None,
            })
        } else {
            // Failure: results_summary() shows `error` and drops `message`,
            // so put the full captured output in `error` (the user needs it
            // to diagnose non-zero exits). The "Error: " prefix comes from
            // the registry, not from us.
            let prefix = match status.code() {
                Some(c) => format!("exit {c}"),
                None => "terminated by signal".to_string(),
            };
            let payload = if body.is_empty() {
                format!("{prefix} (no output)")
            } else {
                format!("{prefix}\n{body}")
            };
            Ok(CommandResult {
                success: false,
                message: String::new(),
                error: Some(payload),
                append_body: None,
            })
        }
    }
}

#[async_trait]
impl CommandHandler for CustomCommandHandler {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.config.description
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // Shell commands short-circuit before mode/skills: there is no LLM
        // to apply them to. mode/skills in the config are silently ignored
        // here — validation does not reject them, but neither do they do
        // anything. Documented in the struct-level rustdoc.
        if let Some(argv) = self.config.shell.as_deref() {
            return self
                .execute_shell(argv, &context.args, Duration::from_secs(SHELL_TIMEOUT_SECS))
                .await;
        }

        let mut notes = Vec::new();

        if let Some(ref mode) = self.config.mode {
            set_mode(&context, mode).await?;
            notes.push(format!("mode={mode}"));
        }
        if let Some(skills) = self.config.skills.as_ref().filter(|s| !s.is_empty()) {
            notes.push(format!("skills={}", skills.join(", ")));
        }

        let message = if notes.is_empty() {
            format!("{}: applied", self.name)
        } else {
            format!("{}: {}", self.name, notes.join(", "))
        };

        Ok(CommandResult {
            success: true,
            message,
            error: None,
            append_body: Some(self.build_append_body(&context.args)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn cmd(mode: Option<&str>, skills: Option<Vec<&str>>) -> CustomCommand {
        CustomCommand {
            name: "review".into(),
            description: "Review the PR".into(),
            mode: mode.map(|m| m.to_string()),
            skills: skills.map(|v| v.into_iter().map(|s| s.to_string()).collect()),
            user_prompt: Some("Review the diff and report findings.".into()),
            shell: None,
        }
    }

    fn shell_cmd(argv: &[&str]) -> CustomCommand {
        CustomCommand {
            name: "ls".into(),
            description: "List files".into(),
            mode: None,
            skills: None,
            user_prompt: None,
            shell: Some(argv.iter().map(|s| s.to_string()).collect()),
        }
    }

    fn test_context(topic_path: &Path) -> CommandContext {
        test_context_with_args(topic_path, vec![])
    }

    fn test_context_with_args(topic_path: &Path, args: Vec<&str>) -> CommandContext {
        CommandContext {
            args: args.into_iter().map(|s| s.to_string()).collect(),
            topic_path: topic_path.to_path_buf(),
            config: Arc::new(
                jyc_types::load_config_from_str(
                    r#"
[general]
[channels.test]
type = "email"
[channels.test.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.test.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
"#,
                )
                .unwrap(),
            ),
            channel: "test".into(),
            agent: None,
            template_dirs: PathBuf::from("/tmp/test/templates").into(),
            channel_type: "websocket".to_string(),
            config_path: None,
        }
    }

    #[test]
    fn name_gets_leading_slash() {
        assert_eq!(CustomCommandHandler::new(cmd(None, None)).name(), "/review");
    }

    #[test]
    fn name_is_not_double_slashed() {
        let mut c = cmd(None, None);
        c.name = "/review".into();
        assert_eq!(CustomCommandHandler::new(c).name(), "/review");
    }

    #[tokio::test]
    async fn plan_mode_writes_mode_override() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = CustomCommandHandler::new(cmd(Some("plan"), None));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();

        assert!(result.success);
        let mode = tokio::fs::read_to_string(tmp.path().join(".jyc/mode-override"))
            .await
            .unwrap();
        assert_eq!(mode, "plan");
    }

    #[tokio::test]
    async fn build_mode_clears_mode_override() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("mode-override"), "plan")
            .await
            .unwrap();

        let handler = CustomCommandHandler::new(cmd(Some("build"), None));
        handler.execute(test_context(tmp.path())).await.unwrap();

        assert!(!jyc_dir.join("mode-override").exists());
    }

    #[tokio::test]
    async fn no_mode_leaves_mode_override_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("mode-override"), "plan")
            .await
            .unwrap();

        let handler = CustomCommandHandler::new(cmd(None, None));
        handler.execute(test_context(tmp.path())).await.unwrap();

        // Still in plan mode — the command did not touch it.
        let mode = tokio::fs::read_to_string(jyc_dir.join("mode-override"))
            .await
            .unwrap();
        assert_eq!(mode, "plan");
    }

    #[tokio::test]
    async fn append_body_names_skills_then_user_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let handler =
            CustomCommandHandler::new(cmd(None, Some(vec!["pr-review", "coding-principles"])));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();
        let body = result.append_body.unwrap();

        assert!(body.contains("pr-review, coding-principles"));
        assert!(body.contains("SKILL.md"));
        // user_prompt comes last so it is the most recent instruction.
        assert!(
            body.trim_end()
                .ends_with("Review the diff and report findings.")
        );
    }

    #[tokio::test]
    async fn append_body_without_skills_is_just_user_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = CustomCommandHandler::new(cmd(None, None));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();

        assert_eq!(
            result.append_body.unwrap(),
            "Review the diff and report findings."
        );
    }

    #[tokio::test]
    async fn empty_skills_list_is_treated_as_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = CustomCommandHandler::new(cmd(None, Some(vec![])));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();

        assert_eq!(
            result.append_body.unwrap(),
            "Review the diff and report findings."
        );
    }

    /// Regression: the registry consumes the whole command line, so same-line
    /// args were silently dropped. README documents `/review focus on X` as
    /// working, so this locks that claim to the code.
    #[tokio::test]
    async fn same_line_args_reach_the_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = CustomCommandHandler::new(cmd(None, None));

        let ctx = test_context_with_args(tmp.path(), vec!["focus", "on", "error", "handling"]);
        let result = handler.execute(ctx).await.unwrap();
        let body = result.append_body.unwrap();

        assert!(
            body.starts_with("focus on error handling"),
            "args must lead the injected body, got: {body:?}"
        );
        assert!(
            body.trim_end()
                .ends_with("Review the diff and report findings.")
        );
    }

    /// `/review focus on X` and `/review\n\nfocus on X` must reach the agent
    /// as the same prompt — that equivalence is what the docs promise.
    #[tokio::test]
    async fn same_line_args_match_newline_form() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = CustomCommandHandler::new(cmd(None, Some(vec!["pr-review"])));

        let same_line = handler
            .execute(test_context_with_args(tmp.path(), vec!["focus", "on", "X"]))
            .await
            .unwrap()
            .append_body
            .unwrap();

        // The newline form leaves "focus on X" in cleaned_body, and the
        // registry appends append_body after it.
        let newline_form = format!(
            "focus on X\n\n{}",
            handler
                .execute(test_context(tmp.path()))
                .await
                .unwrap()
                .append_body
                .unwrap()
        );

        assert_eq!(same_line, newline_form);
    }

    #[tokio::test]
    async fn no_args_leaves_prompt_unprefixed() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = CustomCommandHandler::new(cmd(None, None));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();

        assert_eq!(
            result.append_body.unwrap(),
            "Review the diff and report findings."
        );
    }

    /// The registry lowercases the incoming command before lookup, so the
    /// registered name must be lowercase or the command is unreachable.
    #[test]
    fn name_is_lowercased() {
        let mut c = cmd(None, None);
        c.name = "Review".into();
        assert_eq!(CustomCommandHandler::new(c).name(), "/review");
    }

    // -- shell command path --

    #[tokio::test]
    async fn shell_runs_argv_and_returns_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = CustomCommandHandler::new(shell_cmd(&["printf", "%s\n", "hello"]));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();

        assert!(result.success);
        assert!(
            result.append_body.is_none(),
            "shell commands must not feed the LLM"
        );
        assert_eq!(result.message, "hello\n");
    }

    /// Argv is exec'd directly — no shell parses the elements, so a
    /// shell-meta argument is just a literal argument. Locks in the
    /// injection-safety claim.
    #[tokio::test]
    async fn shell_does_not_interpret_metachars() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = CustomCommandHandler::new(shell_cmd(&["printf", "%s", "; rm -rf /"]));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();

        assert!(result.success);
        assert_eq!(result.message, "; rm -rf /");
    }

    /// User args typed after `/cmd a b c` are appended as more argv
    /// elements, never inserted into existing ones.
    #[tokio::test]
    async fn shell_user_args_append_to_argv() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = CustomCommandHandler::new(shell_cmd(&["printf", "%s %s %s\n"]));

        let result = handler
            .execute(test_context_with_args(tmp.path(), vec!["a", "b", "c"]))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.message, "a b c\n");
    }

    /// Non-zero exit must surface as `!success` with the captured output
    /// preserved (otherwise the user has no way to diagnose the failure).
    #[tokio::test]
    async fn shell_nonzero_exit_surfaces_output_in_error() {
        let tmp = tempfile::tempdir().unwrap();
        // sh is POSIX; prints to stderr and exits 2 on syntax error.
        let handler = CustomCommandHandler::new(shell_cmd(&["sh", "-c", "echo boom 1>&2; exit 2"]));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();

        assert!(!result.success);
        let err = result.error.expect("failure must populate error");
        assert!(
            err.contains("exit 2"),
            "expected exit code in error, got: {err}"
        );
        assert!(
            err.contains("STDERR:"),
            "stderr must reach the user, got: {err}"
        );
        assert!(
            err.contains("boom"),
            "stderr content must be preserved, got: {err}"
        );
        assert!(result.message.is_empty());
    }

    /// Hung processes must be **killed**, not just abandoned — otherwise
    /// `sleep 60`-style commands keep running orphaned after we return, and
    /// a single misconfigured shell command can pin a topic worker (and
    /// spawn a new orphan per invocation). We drive `execute_shell` directly
    /// with a 500ms ceiling (rather than going through `execute`, which
    /// uses the 30s production default) so this test takes ~2.5s instead
    /// of 30s+.
    ///
    /// The child script writes a "started" marker, sleeps for 1s, then
    /// writes a "finished" marker. After the 500ms timeout we wait 2s more
    /// — long enough for the sleep to complete naturally if the child
    /// survived. If "finished" appears in that window, the child was NOT
    /// killed (regression of the orphan-leak bug).
    #[tokio::test]
    async fn shell_timeout_kills_long_running_command() {
        let tmp = tempfile::tempdir().unwrap();
        let started = tmp.path().join("started");
        let finished = tmp.path().join("finished");

        // POSIX-shell-safe single-quote: `'\''` closes the quoted string,
        // inserts a literal `'`, and reopens. Handles paths with apostrophes
        // (tempdir paths normally don't have them, but paranoia is cheap).
        let sq = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let script = format!(
            "touch {}; sleep 1; touch {}",
            sq(&started.to_string_lossy()),
            sq(&finished.to_string_lossy()),
        );

        let handler = CustomCommandHandler::new(shell_cmd(&["sh", "-c", &script]));

        let result = handler
            .execute_shell(
                &["sh".into(), "-c".into(), script],
                &[],
                Duration::from_millis(500),
            )
            .await
            .unwrap();

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("timed out after"),
            "expected timeout error, got: {:?}",
            result.error
        );
        assert!(
            started.exists(),
            "child never started — test is meaningless"
        );

        // Give the (would-be) surviving child enough wall time to finish
        // its sleep naturally if it wasn't killed. 2s > 1s sleep + slack.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !finished.exists(),
            "child reached 'finished' after the timeout — it was NOT killed"
        );
    }

    /// Output beyond the cap is truncated with a byte count so the user
    /// knows there was more (rather than wondering why output cut off).
    #[tokio::test]
    async fn shell_output_is_truncated_with_marker() {
        let tmp = tempfile::tempdir().unwrap();
        // 2048 × "AAAAAAAAAA" = 20 KiB, well over the 8 KiB cap. POSIX-
        // portable and avoids the shell-quoting pitfalls around translating
        // null bytes (`'\0'` in single quotes is the 2-char `\0`, not NUL).
        let handler = CustomCommandHandler::new(shell_cmd(&[
            "sh",
            "-c",
            "for i in $(seq 1 2048); do printf 'AAAAAAAAAA'; done",
        ]));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();

        assert!(result.success);
        assert!(
            result.message.ends_with("more bytes]"),
            "expected truncation marker, got tail: {:?}",
            result.message.chars().rev().take(40).collect::<String>()
        );
        // Body is at most cap + marker. 'A' is 1 byte in UTF-8 so byte length
        // == char count.
        assert!(result.message.len() <= SHELL_MAX_OUTPUT_BYTES + 64);
    }

    /// Shell path ignores `mode` and `skills` — they are an LLM concept and
    /// this path never reaches the LLM. Locks in "no hidden side effect".
    #[tokio::test]
    async fn shell_ignores_mode_and_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = shell_cmd(&["printf", "ok\n"]);
        c.mode = Some("plan".into());
        c.skills = Some(vec!["pr-review".into()]);
        let handler = CustomCommandHandler::new(c);

        // mode="plan" would normally write `.jyc/mode-override`. If shell
        // ignored mode incorrectly and the order flipped, this file would
        // exist after the call.
        let result = handler.execute(test_context(tmp.path())).await.unwrap();

        assert!(result.success);
        assert!(!tmp.path().join(".jyc/mode-override").exists());
        assert!(result.append_body.is_none());
    }
}
