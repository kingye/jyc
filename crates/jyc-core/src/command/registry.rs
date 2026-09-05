use std::collections::HashMap;

use anyhow::Result;

use super::handler::{CommandContext, CommandHandler, CommandOutput, CommandResult};

/// Registry of command handlers with unified parse-execute-strip processing.
///
/// Unlike jiny-m, which splits parsing (CommandRegistry.parseCommands) and
/// body stripping (topic-manager.ts) into two separate passes, JYC unifies
/// these into a single `process_commands()` method.
pub struct CommandRegistry {
    handlers: HashMap<String, Box<dyn CommandHandler>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Register a command handler.
    pub fn register(&mut self, handler: Box<dyn CommandHandler>) {
        let name = handler.name().to_string();
        if self.handlers.contains_key(&name) {
            tracing::warn!(command = %name, "Command handler already registered, overwriting");
        }
        tracing::debug!(command = %name, "Command handler registered");
        self.handlers.insert(name, handler);
    }

    /// Get a handler by name.
    #[allow(dead_code)]
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
    /// content). Lines starting with `/` that match a registered handler are
    /// treated as commands. Empty lines between commands are skipped. The first
    /// non-empty, non-command line ends the command block — everything from
    /// that line onward is the cleaned body.
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
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    let cmd_name = parts[0].to_lowercase();

                    if let Some(handler) = self.handlers.get(&cmd_name) {
                        let mut args: Vec<String> =
                            parts[1..].iter().map(|s| s.to_string()).collect();

                        // Commands that opt into continuation lines (e.g.
                        // `/backlog push`) get subsequent non-blank lines
                        // appended as additional args. Stops at the first
                        // blank line so a body that follows the command
                        // block still flows through to the agent.
                        if handler.collect_subsequent_lines() {
                            while let Some(&next) = lines_iter.peek() {
                                if next.trim().is_empty() {
                                    lines_iter.next(); // consume the blank separator
                                    break;
                                }
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
                    // Unknown command starting with / — not a registered command,
                    // treat as start of message body
                }

                // First non-empty, non-command line → end the command block
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
    struct TestHandler {
        name: String,
    }

    #[async_trait]
    impl CommandHandler for TestHandler {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "test command"
        }
        async fn execute(&self, ctx: CommandContext) -> Result<CommandResult> {
            Ok(CommandResult {
                success: true,
                message: format!("{}: args={:?}", self.name, ctx.args),
                error: None,
                append_body: None,
            })
        }
    }

    fn test_context() -> CommandContext {
        CommandContext {
            args: vec![],
            topic_path: PathBuf::from("/tmp/test"),
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
        registry.register(Box::new(TestHandler {
            name: "/model".into(),
        }));

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

    #[tokio::test]
    async fn test_command_only_message() {
        let mut registry = CommandRegistry::new();
        registry.register(Box::new(TestHandler {
            name: "/model".into(),
        }));

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
        registry.register(Box::new(TestHandler {
            name: "/model".into(),
        }));
        registry.register(Box::new(TestHandler {
            name: "/plan".into(),
        }));

        let body = "/model SomeModel\n/plan\n\nDo the work";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(output.results.len(), 2);
        assert_eq!(output.cleaned_body, "Do the work");
    }

    #[tokio::test]
    async fn test_unknown_command_is_body() {
        let mut registry = CommandRegistry::new();
        registry.register(Box::new(TestHandler {
            name: "/model".into(),
        }));

        // /unknown is not registered, so it's treated as body start
        let body = "/unknown stuff\nmore body";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert!(output.results.is_empty());
        assert_eq!(output.cleaned_body, "/unknown stuff\nmore body");
    }

    /// A handler that injects text into the body via `append_body`.
    struct InjectingHandler;

    /// Echoes received args into `append_body`, proving the registry passes
    /// same-line args through to the handler rather than discarding them.
    struct ArgsEchoHandler;

    #[async_trait]
    impl CommandHandler for ArgsEchoHandler {
        fn name(&self) -> &str {
            "/review"
        }
        fn description(&self) -> &str {
            "args echo"
        }
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
        fn name(&self) -> &str {
            "/review"
        }
        fn description(&self) -> &str {
            "injecting"
        }
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
        registry.register(Box::new(InjectingHandler));

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
        registry.register(Box::new(InjectingHandler));

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
        registry.register(Box::new(TestHandler {
            name: "/model".into(),
        }));

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
        registry.register(Box::new(ArgsEchoHandler));

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
        registry.register(Box::new(ArgsEchoHandler));

        let output = registry
            .process_commands("/review", &test_context())
            .await
            .unwrap();

        assert_eq!(output.cleaned_body, "[args=] PROMPT");
    }

    /// A handler that opts into collecting continuation lines. Subsequent
    /// non-blank lines are appended to `args` (one line per arg), and the
    /// registry stops at the first blank line so a body that follows can
    /// still be passed to the agent.
    struct MultiLineHandler;

    #[async_trait]
    impl CommandHandler for MultiLineHandler {
        fn name(&self) -> &str {
            "/backlog"
        }
        fn description(&self) -> &str {
            "multiline opt-in"
        }
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
    async fn test_collect_subsequent_lines_stops_at_blank() {
        let mut registry = CommandRegistry::new();
        registry.register(Box::new(MultiLineHandler));

        let body = "/backlog push\nline 1\nline 2\nline 3\n\nbody text";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(output.results.len(), 1);
        assert_eq!(
            output.results[0].message,
            r#"args=["push", "line 1", "line 2", "line 3"]"#
        );
        // Trailing body still flows through after the blank line.
        assert_eq!(output.cleaned_body, "body text");
    }

    #[tokio::test]
    async fn test_collect_subsequent_lines_until_end_of_body() {
        let mut registry = CommandRegistry::new();
        registry.register(Box::new(MultiLineHandler));

        // No trailing blank line; collection runs until the body ends.
        let body = "/backlog push\nonly line";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(output.results[0].message, r#"args=["push", "only line"]"#);
        assert!(output.body_empty);
    }

    #[tokio::test]
    async fn test_collect_subsequent_lines_no_continuation() {
        let mut registry = CommandRegistry::new();
        registry.register(Box::new(MultiLineHandler));

        // Blank line right after the command — nothing to collect.
        let body = "/backlog push\n\nlater body";
        let output = registry
            .process_commands(body, &test_context())
            .await
            .unwrap();

        assert_eq!(output.results[0].message, r#"args=["push"]"#);
        assert_eq!(output.cleaned_body, "later body");
    }
}
