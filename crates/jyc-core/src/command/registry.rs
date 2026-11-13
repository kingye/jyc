use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use super::handler::{CommandContext, CommandHandler, CommandOrigin, CommandOutput, CommandResult};

/// Registry of command handlers with unified parse-execute-strip processing.
///
/// Unlike jiny-m, which splits parsing (CommandRegistry.parseCommands) and
/// body stripping (topic-manager.ts) into two separate passes, JYC unifies
/// these into a single `process_commands()` method.
pub struct CommandRegistry {
    handlers: HashMap<String, Arc<dyn CommandHandler>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Register a handler under `name`.
    ///
    /// The caller supplies the key: built-ins pass the name from
    /// [`crate::command::builtin`] (the same table the `/` popup rows come from),
    /// user-defined commands pass [`CustomCommandHandler::command_key`]. A handler
    /// cannot report its own name — that is what kept dispatch and display from
    /// drifting apart (#814).
    pub fn register(&mut self, name: &str, handler: Arc<dyn CommandHandler>) {
        if self.handlers.contains_key(name) {
            tracing::warn!(command = %name, "Command handler already registered, overwriting");
        }
        tracing::debug!(command = %name, "Command handler registered");
        self.handlers.insert(name.to_string(), handler);
    }

    /// Get a handler by name.
    pub fn get(&self, name: &str) -> Option<&dyn CommandHandler> {
        self.handlers.get(name).map(|h| h.as_ref())
    }

    /// List all registered handlers.
    #[allow(dead_code)]
    pub fn list(&self) -> Vec<&dyn CommandHandler> {
        self.handlers.values().map(|h| h.as_ref()).collect()
    }

    /// Parse, execute, and strip commands from message body in a single pass.
    ///
    /// Commands must appear at the top of the body (before any non-command
    /// content). Lines starting with `/` are treated as commands: a registered
    /// handler is executed; an unregistered name produces an "Unknown command"
    /// error result and its line is dropped from the body. Empty lines between
    /// commands are skipped. The first non-empty, non-`/` line ends the command
    /// block — everything from that line onward is the cleaned body. Handlers
    /// that opt into `collect_subsequent_lines` instead consume the rest of the
    /// message, blank lines included, and leave no body.
    ///
    /// Returns executed results + cleaned body. TopicManager does NOT need
    /// to know about command line syntax.
    pub async fn process_commands(
        &self,
        body: &str,
        context: &CommandContext,
    ) -> Result<CommandOutput> {
        let mut results = Vec::new();
        let mut body_lines = Vec::new();
        let mut in_command_block = true;

        // Peekable so opt-in handlers can collect continuation lines into
        // their `args` vector without changing outer loop control flow.
        let mut lines_iter = body.lines().peekable();

        while let Some(line) = lines_iter.next() {
            let trimmed = line.trim();

            if in_command_block {
                if trimmed.is_empty() {
                    // Skip blank lines in the command block
                    continue;
                }

                if trimmed.starts_with('/') {
                    // A line whose quotes do not close has no word boundaries
                    // to read, so nothing on it can be run — the same shape as
                    // an unknown command: report it, drop the line, and let the
                    // rest of the message be body text.
                    let parts = match super::line::split(trimmed) {
                        Ok(parts) => parts,
                        Err(error) => {
                            results.push(CommandResult::refusal(format!(
                                "{error}: the line was not run. Close the quote, or drop it."
                            )));
                            in_command_block = false;
                            continue;
                        }
                    };
                    let cmd_name = parts[0].to_lowercase();

                    if let Some(handler) = self.handlers.get(&cmd_name) {
                        // One command block can stack several commands
                        // (`/close --force` then `/thinking high`), and each
                        // runs after the previous one has taken effect — so by
                        // the time the second runs, its topic is gone and any
                        // state write it does would recreate the directory the
                        // close deleted. Refusing here (rather than trusting
                        // every handler to consult the freeze) is what keeps
                        // this from coming back with each new command; the
                        // per-handler guards stay as defence in depth. `/close`
                        // itself is still let through so a repeated close stays
                        // an idempotent no-op.
                        if cmd_name != "close"
                            && jyc_types::state_dir::is_closed(&context.topic_name)
                        {
                            tracing::info!(
                                command = %cmd_name,
                                "Topic was closed; skipping command"
                            );
                            results.push(CommandResult::refusal(format!(
                                "/{cmd_name}: this topic was just closed; the command was skipped"
                            )));
                            continue;
                        }

                        // An authorization command widens or narrows the
                        // filesystem boundary, so only the user may run one. The
                        // send path refuses such a message before it is queued;
                        // this is the dispatch-side half, for every other way a
                        // message can carry a command it did not get from the
                        // user (a turn reply forwarded to its requester, a future
                        // injection path). `send_handler` is not trusted to be
                        // the only one.
                        if context.origin == CommandOrigin::Agent
                            && super::builtin::is_authorization_command(&cmd_name)
                        {
                            tracing::warn!(
                                command = %cmd_name,
                                "Refusing an authorization command from an agent-sent message"
                            );
                            results.push(CommandResult::refusal(format!(
                                "{cmd_name}: only the user may run this; it changes filesystem access"
                            )));
                            continue;
                        }

                        let mut args: Vec<String> = parts[1..].to_vec();

                        // Commands that opt into continuation lines (e.g.
                        // `/backlog push`) get every remaining line of the
                        // message appended as additional args — blank lines
                        // included — so the command owns the rest of the
                        // message. Paragraph breaks inside an item stay with
                        // the item instead of leaking the tail into an agent
                        // run.
                        //
                        // The first-line content (everything after the
                        // subcommand on the command line) is collapsed into
                        // a single string at `args[1]`. Without this, the
                        // handler cannot distinguish command-line content
                        // (e.g. `/backlog push This is a description`) from
                        // continuation lines (e.g. `/backlog push\nline 1\nline 2`).
                        // An empty placeholder is pushed at `args[1]` when
                        // there is no first-line content, so the handler
                        // can index `args[1]` uniformly.
                        if handler.collect_subsequent_lines() {
                            match args.len() {
                                0 => {
                                    // No subcommand on the command line.
                                    // The handler should guard against this
                                    // and return a "missing subcommand" error
                                    // before indexing `args[0]` — see
                                    // `BacklogCommandHandler::execute` for an
                                    // example. We do NOT push a placeholder
                                    // here because that would change `args[0]`
                                    // and break subcommand dispatch.
                                }
                                _ => {
                                    let first_line = args[1..].join(" ");
                                    args.truncate(1);
                                    args.push(first_line);
                                }
                            }
                            while let Some(&next) = lines_iter.peek() {
                                args.push(next.to_string());
                                lines_iter.next();
                            }
                        }

                        let ctx = CommandContext {
                            args,
                            ..context.clone()
                        };

                        tracing::info!(
                            command = %cmd_name,
                            "Executing command"
                        );

                        match handler.execute(ctx).await {
                            Ok(result) => {
                                if result.success {
                                    tracing::info!(
                                        command = %cmd_name,
                                        message = %result.message,
                                        "Command succeeded"
                                    );
                                } else {
                                    tracing::warn!(
                                        command = %cmd_name,
                                        error = ?result.error,
                                        "Command failed"
                                    );
                                }
                                results.push(result);
                            }
                            Err(e) => {
                                tracing::error!(
                                    command = %cmd_name,
                                    error = %e,
                                    "Command execution error"
                                );
                                results.push(CommandResult {
                                    success: false,
                                    message: format!("{cmd_name}: error"),
                                    error: Some(e.to_string()),
                                    append_body: None,
                                });
                            }
                        }
                        continue; // Command consumed, don't add to body
                    }

                    // Unknown `/` command: report the error and drop this
                    // line from the body; the rest of the message continues
                    // as body text as usual.
                    results.push(CommandResult {
                        success: false,
                        message: String::new(),
                        error: Some(format!(
                            "Unknown command `{cmd_name}`. Type `/?` for the command list."
                        )),
                        append_body: None,
                    });
                    in_command_block = false;
                    continue;
                }

                // First non-empty, non-`/` line → end the command block
                in_command_block = false;
                body_lines.push(line);
            } else {
                body_lines.push(line);
            }
        }

        let mut cleaned_body = body_lines.join("\n");

        // Append any command-injected prompt text (user-defined commands
        // contribute their `user_prompt` here). Appended after the user's own
        // text so the command instruction is the last thing the agent reads.
        for injected in results.iter().filter_map(|r| r.append_body.as_deref()) {
            if !cleaned_body.trim().is_empty() {
                cleaned_body.push_str("\n\n");
            }
            cleaned_body.push_str(injected);
        }

        let body_empty = cleaned_body.trim().is_empty();

        Ok(CommandOutput {
            results,
            cleaned_body,
            body_empty,
        })
    }
}

/// The command names in `body`'s leading command block, lowercased.
///
/// Mirrors the block scan [`CommandRegistry::process_commands`] performs — the
/// block starts at the first non-blank line and ends at the first line that is
/// not `/`-prefixed — but stops at the names: it reports what is shaped like a
/// command without running anything.
///
/// Deliberately an over-approximation. `process_commands` also ends the block at
/// an unknown command, and at a command that swallows the rest of the message as
/// arguments (`collect_subsequent_lines`), so this can name a command that would
/// never actually run. Over-naming is the safe direction for its one caller —
/// the send path refuses a message carrying an authorization command — while
/// the exact check is the dispatch-side guard inside
/// [`CommandRegistry::process_commands`], which sees the real handler table.
pub fn leading_command_names(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with('/') {
            break;
        }
        // The splitter dispatch reads the line with, not a second reading of
        // our own: a spelling dispatch sees as `/grant` has to be named here
        // too, or a line like `/gra"nt" /data` would pass this guard and be
        // run as a grant by the dispatcher — an agent granting itself a path.
        // A line the splitter refuses runs nothing at all, so the plain
        // whitespace split on that path can only over-name, which is this
        // scan's safe direction.
        let name = match super::line::split(trimmed) {
            Ok(words) => words.into_iter().next(),
            Err(_) => trimmed.split_whitespace().next().map(str::to_string),
        };
        if let Some(name) = name {
            names.push(name.to_lowercase());
        }
    }
    names
}

/// Whether `body`'s leading command block carries a command only the user may
/// run — an [`CommandSpec::authorization`] command such as `/grant`.
///
/// [`CommandSpec::authorization`]: super::builtin::CommandSpec::authorization
pub fn carries_authorization_command(body: &str) -> bool {
    leading_command_names(body)
        .iter()
        .any(|name| super::builtin::is_authorization_command(name))
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// A simple test command handler.
    struct TestHandler;

    #[async_trait]
    impl CommandHandler for TestHandler {
        async fn execute(&self, ctx: CommandContext) -> Result<CommandResult> {
            Ok(CommandResult {
                success: true,
                message: format!("ok: args={:?}", ctx.args),
                error: None,
                append_body: None,
            })
        }
    }

    fn test_context() -> CommandContext {
        crate::test_support::test_context("test-topic", std::path::Path::new("/tmp/test"))
    }

    #[tokio::test]
    async fn test_no_commands() {
        let registry = CommandRegistry::new();
        let output = registry
            .process_commands("Hello, how are you?", &test_context())
            .await
            .unwrap();

        assert!(output.results.is_empty());
        assert_eq!(output.cleaned_body, "Hello, how are you?");
        assert!(!output.body_empty);
    }

    #[tokio::test]
    async fn test_command_at_top_with_body() {
        let mut registry = CommandRegistry::new();
        registry.register("/model", Arc::new(TestHandler));

        let body = "/model SomeModel\n\nImplement feature X";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(output.results.len(), 1);
        assert!(output.results[0].success);
        assert_eq!(output.cleaned_body, "Implement feature X");
        assert!(!output.body_empty);
    }

    /// `/skill:<name> <text>` end to end: the command line is consumed, the
    /// rest of the message stays the body, and the skill directive is appended
    /// after it — the same shape a `[[commands]]` entry produces.
    #[tokio::test]
    async fn test_skill_command_appends_directive_to_body() {
        let meta = jyc_types::SkillMeta {
            name: "ponytail".into(),
            description: "Laziest solution that works".into(),
            source_path: std::path::PathBuf::from("/skills/ponytail"),
        };
        let mut registry = CommandRegistry::new();
        for cmd in crate::command::skill_commands(&[meta], None) {
            let key = crate::command::custom_handler::CustomCommandHandler::command_key(&cmd);
            registry.register(
                &key,
                Arc::new(crate::command::custom_handler::CustomCommandHandler::new(
                    cmd,
                )),
            );
        }

        let output = registry
            .process_commands("/skill:ponytail 帮我看看这段\n第二段\n", &test_context())
            .await
            .unwrap();

        assert_eq!(output.results.len(), 1);
        assert!(output.results[0].success);
        assert!(
            output.cleaned_body.starts_with("第二段"),
            "the user's own lines lead: {:?}",
            output.cleaned_body
        );
        assert!(output.cleaned_body.contains("帮我看看这段"));
        // The directive carries the skill's name, path and description, so
        // the agent can find and read the SKILL.md even when the topic's
        // prompt does not list the skill.
        assert!(output.cleaned_body.contains("## Skill: ponytail"));
        assert!(output.cleaned_body.contains("/skills/ponytail"));
        assert!(output.cleaned_body.contains("SKILL.md"));
    }

    #[tokio::test]
    async fn test_command_only_message() {
        let mut registry = CommandRegistry::new();
        registry.register("/model", Arc::new(TestHandler));

        let body = "/model reset\n";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(output.results.len(), 1);
        assert!(output.body_empty);
    }

    #[tokio::test]
    async fn test_multiple_commands() {
        let mut registry = CommandRegistry::new();
        registry.register("/model", Arc::new(TestHandler));
        registry.register("/plan", Arc::new(TestHandler));

        let body = "/model SomeModel\n/plan\n\nDo the work";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(output.results.len(), 2);
        assert_eq!(output.cleaned_body, "Do the work");
    }

    #[tokio::test]
    async fn test_unknown_command_errors() {
        let mut registry = CommandRegistry::new();
        registry.register("/model", Arc::new(TestHandler));

        // /unknown is not registered: it produces an error result and its
        // line is dropped; the remaining lines become the body.
        let body = "/unknown stuff\nmore body";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(output.results.len(), 1);
        assert!(!output.results[0].success);
        let err = output.results[0].error.as_deref().unwrap();
        assert!(err.contains("Unknown command `/unknown`"), "{err}");
        assert_eq!(output.cleaned_body, "more body");
    }

    #[tokio::test]
    async fn test_unknown_command_alone_replies_only() {
        let registry = CommandRegistry::new();
        let output = registry
            .process_commands("/blabla", &test_context())
            .await
            .unwrap();

        assert_eq!(output.results.len(), 1);
        assert!(!output.results[0].success);
        assert!(output.cleaned_body.trim().is_empty());
    }

    /// A handler that injects text into the body via `append_body`.
    struct InjectingHandler;

    /// Echoes received args into `append_body`, proving the registry passes
    /// same-line args through to the handler rather than discarding them.
    struct ArgsEchoHandler;

    #[async_trait]
    impl CommandHandler for ArgsEchoHandler {
        async fn execute(&self, ctx: CommandContext) -> Result<CommandResult> {
            Ok(CommandResult {
                success: true,
                message: "ok".into(),
                error: None,
                append_body: Some(format!("[args={}] PROMPT", ctx.args.join(" "))),
            })
        }
    }

    #[async_trait]
    impl CommandHandler for InjectingHandler {
        async fn execute(&self, _ctx: CommandContext) -> Result<CommandResult> {
            Ok(CommandResult {
                success: true,
                message: "/review: ok".into(),
                error: None,
                append_body: Some("Review the code.".into()),
            })
        }
    }

    #[tokio::test]
    async fn test_append_body_on_command_only_message() {
        let mut registry = CommandRegistry::new();
        registry.register("/review", Arc::new(InjectingHandler));

        let output = registry
            .process_commands("/review", &test_context())
            .await
            .unwrap();

        // The injected prompt becomes the body, so the message is NOT empty
        // and therefore still reaches the agent.
        assert_eq!(output.cleaned_body, "Review the code.");
        assert!(!output.body_empty);
    }

    #[tokio::test]
    async fn test_append_body_appends_after_user_text() {
        let mut registry = CommandRegistry::new();
        registry.register("/review", Arc::new(InjectingHandler));

        let output = registry
            .process_commands(
                "/review

focus on error handling",
                &test_context(),
            )
            .await
            .unwrap();

        assert_eq!(
            output.cleaned_body,
            "focus on error handling\n\nReview the code."
        );
        assert!(!output.body_empty);
    }

    #[tokio::test]
    async fn test_no_append_body_leaves_body_unchanged() {
        let mut registry = CommandRegistry::new();
        registry.register("/model", Arc::new(TestHandler));

        let output = registry
            .process_commands("/model X\n\nhello", &test_context())
            .await
            .unwrap();

        assert_eq!(output.cleaned_body, "hello");
    }

    #[tokio::test]
    async fn test_results_summary() {
        let output = CommandOutput {
            results: vec![
                CommandResult {
                    success: true,
                    message: "/model: switched to GPT-4".into(),
                    error: None,
                    append_body: None,
                },
                CommandResult {
                    success: false,
                    message: "/plan: failed".into(),
                    error: Some("mode not supported".into()),
                    append_body: None,
                },
            ],
            cleaned_body: String::new(),
            body_empty: true,
        };

        let summary = output.results_summary();
        assert!(summary.contains("/model: switched to GPT-4"));
        assert!(summary.contains("Error: mode not supported"));
    }

    /// End-to-end regression for the documented `/review focus on X` form.
    /// The registry consumes the entire command line, so args only survive if
    /// the handler re-injects them via `append_body`.
    #[tokio::test]
    async fn test_same_line_args_survive_into_cleaned_body() {
        let mut registry = CommandRegistry::new();
        registry.register("/review", Arc::new(ArgsEchoHandler));

        let output = registry
            .process_commands("/review focus on error handling", &test_context())
            .await
            .unwrap();

        assert_eq!(output.cleaned_body, "[args=focus on error handling] PROMPT");
        assert!(!output.body_empty);
    }

    #[tokio::test]
    async fn test_command_without_args_yields_empty_args() {
        let mut registry = CommandRegistry::new();
        registry.register("/review", Arc::new(ArgsEchoHandler));

        let output = registry
            .process_commands("/review", &test_context())
            .await
            .unwrap();

        assert_eq!(output.cleaned_body, "[args=] PROMPT");
    }

    /// A handler that opts into collecting continuation lines. Every
    /// remaining line of the message is appended to `args` (one line per
    /// arg, blank lines included) — the command owns the rest of the
    /// message and nothing leaks into the cleaned body.
    struct MultiLineHandler;

    #[async_trait]
    impl CommandHandler for MultiLineHandler {
        fn collect_subsequent_lines(&self) -> bool {
            true
        }
        async fn execute(&self, ctx: CommandContext) -> Result<CommandResult> {
            // Echo the joined args back so the test can assert exact contents.
            Ok(CommandResult {
                success: true,
                message: format!("args={:?}", ctx.args),
                error: None,
                append_body: None,
            })
        }
    }

    #[tokio::test]
    async fn test_collect_subsequent_lines_owns_rest_of_message() {
        let mut registry = CommandRegistry::new();
        registry.register("/backlog", Arc::new(MultiLineHandler));

        let body = "/backlog push\nline 1\nline 2\nline 3\n\nbody text";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(output.results.len(), 1);
        // args[0] = subcommand, args[1] = "" (no first-line content), args[2..]
        // = every remaining line, including the blank separator — the tail
        // stays with the command instead of leaking into an agent run.
        assert_eq!(
            output.results[0].message,
            r#"args=["push", "", "line 1", "line 2", "line 3", "", "body text"]"#
        );
        assert!(output.body_empty, "nothing may follow a collecting command");
    }

    #[tokio::test]
    async fn test_collect_subsequent_lines_until_end_of_body() {
        let mut registry = CommandRegistry::new();
        registry.register("/backlog", Arc::new(MultiLineHandler));

        // No trailing blank line; collection runs until the body ends.
        let body = "/backlog push\nonly line";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(
            output.results[0].message,
            r#"args=["push", "", "only line"]"#
        );
        assert!(output.body_empty);
    }

    #[tokio::test]
    async fn test_collect_subsequent_lines_blank_immediately_owns_body() {
        let mut registry = CommandRegistry::new();
        registry.register("/backlog", Arc::new(MultiLineHandler));

        // A blank line right after the command no longer separates the
        // command from the body: the collecting command owns the rest of
        // the message — the blank and `later body` both land in args
        // (args[1] is the empty first-line placeholder), and nothing
        // remains for the agent.
        let body = "/backlog push\n\nlater body";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(
            output.results[0].message,
            r#"args=["push", "", "", "later body"]"#
        );
        assert!(output.body_empty);
    }

    /// `/backlog push hello world` (single command line, no continuation):
    /// the tokens after `push` should be collapsed into a single
    /// first-line string at args[1], not left as separate elements.
    /// This is the regression test for the user's reported issue:
    /// previously each space-separated token became a separate `args`
    /// element, so the description joined with `\n` rendered as five
    /// separate lines.
    #[tokio::test]
    async fn test_collect_subsequent_lines_collapses_first_line() {
        let mut registry = CommandRegistry::new();
        registry.register("/backlog", Arc::new(MultiLineHandler));

        let body = "/backlog push This is a backlog issue";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(
            output.results[0].message,
            r#"args=["push", "This is a backlog issue"]"#
        );
    }

    /// Marks its topic closed, the way `/close` does — so the *next* command
    /// in the same block runs against a topic that is already gone.
    struct ClosingHandler {
        /// The state dir to register for the topic. Registration comes first
        /// because `resolve_state_dir` deliberately panics on an unregistered
        /// topic.
        state_dir: PathBuf,
    }

    #[async_trait]
    impl CommandHandler for ClosingHandler {
        async fn execute(&self, ctx: CommandContext) -> Result<CommandResult> {
            jyc_types::state_dir::register_if_absent(&ctx.topic_name, &self.state_dir);
            jyc_types::state_dir::mark_closed(&ctx.topic_name);
            Ok(CommandResult {
                success: true,
                message: "closed".to_string(),
                error: None,
                append_body: None,
            })
        }
    }

    /// A command stacked behind a close must not run: the topic is gone, and
    /// whatever the handler writes would recreate its state dir (the
    /// `/close --force` + `/model x` class, #992).
    #[tokio::test]
    async fn test_commands_stacked_behind_close_are_refused() {
        // Unique name per run: the closed set and the registry are
        // process-global.
        let topic = format!(
            "registry-close-gate-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let home = tempfile::tempdir().unwrap();
        let state_dir = home.path().join(".jyc");

        let mut registry = CommandRegistry::new();
        registry.register("/dummy", Arc::new(TestHandler));
        registry.register(
            "/close",
            Arc::new(ClosingHandler {
                state_dir: state_dir.clone(),
            }),
        );

        let context = CommandContext {
            topic_name: topic.clone(),
            topic_path: home.path().to_path_buf(),
            ..test_context()
        };

        let output = registry
            .process_commands("/dummy before\n/close\n/dummy after", &context)
            .await
            .unwrap();

        assert_eq!(output.results.len(), 3);
        assert!(output.results[0].success, "runs before the close");
        assert!(output.results[1].success, "`/close` itself is let through");
        assert!(!output.results[2].success, "runs after the close");
        assert!(
            output.results[2]
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("just closed")
        );
        assert!(
            !state_dir.exists(),
            "the gate must not recreate the state dir"
        );

        jyc_types::state_dir::clear_closed(&topic);
        jyc_types::state_dir::unregister(&topic);
    }

    /// Mixed form: tokens on the command line + continuation lines.
    /// args[1] = space-joined first-line content; args[2..] = continuation.
    #[tokio::test]
    async fn test_collect_subsequent_lines_mixes_first_line_and_continuation() {
        let mut registry = CommandRegistry::new();
        registry.register("/backlog", Arc::new(MultiLineHandler));

        let body = "/backlog push first line\nsecond line\nthird line";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(
            output.results[0].message,
            r#"args=["push", "first line", "second line", "third line"]"#
        );
    }
    // ---- authorization commands: the table, and the dispatch-side guard ----

    /// The leading command block is what dispatch reads, so the send-path scan
    /// must name exactly the lines dispatch would treat as commands — and stop
    /// at the first line that is not one.
    #[test]
    fn leading_command_names_reads_only_the_leading_block() {
        assert_eq!(
            leading_command_names("/grant /data -w\n/ungrant /data\n继续"),
            vec!["/grant", "/ungrant"]
        );
        assert_eq!(
            leading_command_names("\n\n/grant /data\n"),
            vec!["/grant"],
            "blank lines are skipped, as dispatch skips them"
        );
        assert!(
            leading_command_names("请看 /grant 的说明\n/grant /data").is_empty(),
            "a body line ends the block; a later /grant is body text"
        );
        assert!(leading_command_names("").is_empty());
    }

    /// The table is the one place that says which commands change the boundary.
    #[test]
    fn the_table_names_the_authorization_commands() {
        assert!(carries_authorization_command("/grant /data"));
        assert!(carries_authorization_command("/ungrant /data"));
        assert!(
            carries_authorization_command("/GRANT /data"),
            "dispatch lowercases before matching, so the scan must too"
        );
        assert!(!carries_authorization_command("/model gpt-4"));
        assert!(
            carries_authorization_command("/grant /data\n/ungrant /data"),
            "one command block can stack several commands"
        );
    }

    /// The scan deliberately over-approximates. Dispatch ends the command
    /// block at a line that is not a command — including an unknown `/` name,
    /// whose line it drops — so a `/grant` after one is body text and never
    /// runs. The scan reads past it and names the command anyway, so the send
    /// path refuses a message dispatch would have run nothing in.
    ///
    /// Asserted against `process_commands` itself: the claim rests on the
    /// dispatcher, not on a reading of it.
    #[tokio::test]
    async fn the_scan_refuses_more_than_dispatch_would_run() {
        let mut registry = CommandRegistry::new();
        registry.register("/model", Arc::new(TestHandler));
        registry.register("/grant", Arc::new(TestHandler));

        // A registered command keeps the block open, so dispatch really does
        // reach the `/grant` after it.
        let stacked = "/model gpt-4\n/grant /data";
        let output = registry
            .process_commands(stacked, &test_context())
            .await
            .unwrap();
        assert_eq!(
            output.results.len(),
            2,
            "both commands run: {:?}",
            output.results
        );
        assert!(carries_authorization_command(stacked));

        // An unknown name ends the block: the `/grant` becomes body text and
        // never runs, yet the scan still refuses the message.
        let after_unknown = "/no-such-command\n/grant /data";
        let output = registry
            .process_commands(after_unknown, &test_context())
            .await
            .unwrap();
        assert_eq!(
            output.results.len(),
            1,
            "only the unknown-command error: {:?}",
            output.results
        );
        assert_eq!(
            output.cleaned_body, "/grant /data",
            "the `/grant` line survives as body text, unexecuted"
        );
        assert!(
            carries_authorization_command(after_unknown),
            "the scan refuses anyway — over-refusing is the safe direction for \
             a guard on filesystem access"
        );
    }

    /// Every name the scan reports as authorization is one dispatch actually
    /// refuses from an agent — one table, read by both guards.
    #[test]
    fn the_scan_and_the_table_agree_on_every_authorization_command() {
        let authorization: Vec<&str> = super::super::builtin::BUILTIN_COMMANDS
            .iter()
            .filter(|spec| spec.authorization)
            .map(|spec| spec.name)
            .collect();
        assert_eq!(
            authorization,
            vec!["/grant", "/ungrant"],
            "the two commands that change filesystem access"
        );
        for name in authorization {
            assert!(
                carries_authorization_command(&format!("{name} /data")),
                "{name} must be caught by the send-path scan"
            );
        }
    }

    /// The scan is deliberately an over-approximation: dispatch stops the block
    /// at an unknown command, while the scan keeps reading. Over-naming refuses
    /// a harmless message, which is the safe direction — recorded here so the
    /// divergence is a decision, not a surprise.
    #[test]
    fn the_scan_over_approximates_where_dispatch_stops_at_an_unknown_command() {
        let body = "/not-a-command\n/grant /data";
        assert_eq!(
            leading_command_names(body),
            vec!["/not-a-command", "/grant"],
            "the scan does not know the handler table"
        );
        assert!(carries_authorization_command(body));
    }

    /// Quoting reaches the handler: a quoted run is one argument, and a `#`
    /// is a word rather than the start of a comment.
    #[tokio::test]
    async fn a_quoted_argument_arrives_as_one_word() {
        let mut registry = CommandRegistry::new();
        registry.register("/model", Arc::new(TestHandler));

        let output = registry
            .process_commands(r#"/model "a b" ping #1045"#, &test_context())
            .await
            .unwrap();

        assert_eq!(
            output.results[0].message, r##"ok: args=["a b", "ping", "#1045"]"##,
            "{:?}",
            output.results[0].error
        );
    }

    /// A quoted command name is the same command to both readers. They share
    /// one splitter for this: were the scan still splitting on whitespace, it
    /// would read `/gra"nt"` as no command at all while dispatch ran a grant —
    /// an agent could widen its own boundary through the send path.
    #[tokio::test]
    async fn a_quoted_command_name_is_the_same_command_to_both_readers() {
        let mut registry = CommandRegistry::new();
        registry.register("/grant", Arc::new(TestHandler));

        for line in [r#"/gra"nt" /data"#, r#"/g"r"'a'nt /data"#] {
            let output = registry
                .process_commands(line, &test_context())
                .await
                .unwrap();
            assert_eq!(output.results.len(), 1, "{line}: one command must run");
            assert!(
                output.results[0].success,
                "{line}: dispatch must read it as /grant — {:?}",
                output.results[0].error
            );
            assert!(
                carries_authorization_command(line),
                "{line}: the scan must name the command dispatch ran"
            );
        }
    }

    /// An unbalanced line runs nothing: dispatch refuses the whole line rather
    /// than guessing at word boundaries nobody typed.
    #[tokio::test]
    async fn an_unbalanced_quote_is_refused_by_dispatch() {
        let mut registry = CommandRegistry::new();
        registry.register("/grant", Arc::new(TestHandler));

        for line in [
            r#"/gra"nt /data"#,
            r#"/g'ra"'nt' /data"#,
            r#"/grant /data "x"#,
        ] {
            let output = registry
                .process_commands(line, &test_context())
                .await
                .unwrap();
            assert_eq!(output.results.len(), 1, "{line}: a refusal, not silence");
            assert!(
                !output.results[0].success,
                "{line}: an unbalanced line must not run"
            );
        }
    }

    /// Where the splitter refuses, the scan falls back to whitespace on the
    /// same line. That can name `/grant` that dispatch never ran — over-naming,
    /// the safe direction, since the message is refused before it is queued.
    /// It cannot under-name a line dispatch *did* run: `split` succeeded there.
    #[test]
    fn an_unbalanced_quote_after_the_name_over_names_rather_than_under_names() {
        // The unmatched quote sits after the command name, so the fallback's
        // first word is still `/grant`.
        assert!(carries_authorization_command(r#"/grant /data "x"#));
        // Here the fallback word is the mangled `"/gra"nt` — not named. Nothing
        // runs either, which is why this is not a hole.
        assert!(!carries_authorization_command(r#"/gra"nt /data"#));
    }

    /// A quote before the leading `/` puts the line outside the command block
    /// for both readers alike: the `/` check runs before any splitting, so a
    /// quote cannot smuggle a command in — or out. The two agree it is not a
    /// command, which is why neither reader runs nor names one.
    #[tokio::test]
    async fn a_quote_before_the_slash_is_no_command_to_either_reader() {
        let mut registry = CommandRegistry::new();
        registry.register("/grant", Arc::new(TestHandler));

        for line in [r#"'/grant' /data"#, r#""/grant" /data"#] {
            let output = registry
                .process_commands(line, &test_context())
                .await
                .unwrap();
            assert!(
                output.results.is_empty(),
                "{line}: no command must run — {:?}",
                output.results
            );
            assert_eq!(output.cleaned_body, line, "{line}: the line is body text");
            assert!(
                !carries_authorization_command(line),
                "{line}: the scan must not name a command either"
            );
        }
    }

    /// A line whose quotes do not close cannot be read, so it is refused and
    /// dropped rather than run with a word nothing delimited — and the rest of
    /// the message is body text, as after an unknown command.
    #[tokio::test]
    async fn an_unclosed_quote_is_refused_and_the_line_is_dropped() {
        let mut registry = CommandRegistry::new();
        registry.register("/grant", Arc::new(TestHandler));

        let output = registry
            .process_commands("/grant \"/data\nhello", &test_context())
            .await
            .unwrap();

        assert_eq!(output.results.len(), 1, "{:?}", output.results);
        assert!(!output.results[0].success);
        assert!(
            output.results[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("unclosed")),
            "{:?}",
            output.results[0].error
        );
        assert_eq!(output.cleaned_body, "hello");
    }

    /// The dispatch-side half: a message an agent sent cannot run `/grant`,
    /// even if it got past the send path by some other route.
    #[tokio::test]
    async fn an_agent_sent_message_cannot_run_an_authorization_command() {
        let mut registry = CommandRegistry::new();
        registry.register("/grant", Arc::new(TestHandler));

        let mut context = test_context();
        context.origin = CommandOrigin::Agent;

        let output = registry
            .process_commands("/grant /data -w", &context)
            .await
            .unwrap();

        assert_eq!(output.results.len(), 1);
        assert!(!output.results[0].success);
        assert!(
            output.results[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("only the user")),
            "{:?}",
            output.results
        );
    }

    /// The user's own message runs it: the guard keys on origin, not on the
    /// command name alone.
    #[tokio::test]
    async fn a_user_sent_message_can_run_an_authorization_command() {
        let mut registry = CommandRegistry::new();
        registry.register("/grant", Arc::new(TestHandler));

        let output = registry
            .process_commands("/grant /data -w", &test_context())
            .await
            .unwrap();

        assert_eq!(output.results.len(), 1);
        assert!(output.results[0].success, "{:?}", output.results[0].error);
        assert!(output.results[0].message.contains("/data"));
    }
}
