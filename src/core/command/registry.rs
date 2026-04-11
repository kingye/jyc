use std::collections::HashMap;

use anyhow::Result;

use super::handler::{CommandContext, CommandHandler, CommandOutput, CommandResult};

/// Registry of command handlers with unified parse-execute-strip processing.
///
/// Unlike jiny-m, which splits parsing (CommandRegistry.parseCommands) and
/// body stripping (thread-manager.ts) into two separate passes, JYC unifies
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
    /// Returns executed results + cleaned body. ThreadManager does NOT need
    /// to know about command line syntax.
    pub async fn process_commands(
        &self,
        body: &str,
        context: &CommandContext,
    ) -> Result<CommandOutput> {
        let mut results = Vec::new();
        let mut body_lines = Vec::new();
        let mut in_command_block = true;

        for line in body.lines() {
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
                        let args: Vec<String> =
                            parts[1..].iter().map(|s| s.to_string()).collect();
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
                                    requires_restart: false,
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

        let cleaned_body = body_lines.join("\n");
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
                requires_restart: false,
            })
        }
    }

    fn test_context() -> CommandContext {
        CommandContext {
            args: vec![],
            thread_path: PathBuf::from("/tmp/test"),
            config: Arc::new(crate::config::load_config_from_str(
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
mode = "opencode"
"#,
            ).unwrap()),
            channel: "test".into(),
            agent: None,
            template_dir: PathBuf::from("/tmp/test/templates"),
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

    #[tokio::test]
    async fn test_results_summary() {
        let output = CommandOutput {
            results: vec![
                CommandResult {
                    success: true,
                    message: "/model: switched to GPT-4".into(),
                    error: None,
                    requires_restart: true,
                },
                CommandResult {
                    success: false,
                    message: "/plan: failed".into(),
                    error: Some("mode not supported".into()),
                    requires_restart: false,
                },
            ],
            cleaned_body: String::new(),
            body_empty: true,
        };

        assert!(output.requires_restart());
        let summary = output.results_summary();
        assert!(summary.contains("/model: switched to GPT-4"));
        assert!(summary.contains("Error: mode not supported"));
    }
}
