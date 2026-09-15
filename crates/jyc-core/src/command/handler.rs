use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;

use jyc_types::config::HookEvent;
use jyc_types::{AppConfig, CustomCommand};
use jyc_utils::hooks::{HookCtx, HookSet};

/// Context passed to a command handler during execution.
#[derive(Clone)]
pub struct CommandContext {
    /// Command arguments (everything after the command name)
    pub args: Vec<String>,
    /// Path to the topic directory
    pub topic_path: PathBuf,
    /// Topic name — the identity that state-dir resolution keys on
    /// (`state_dir::jyc_dir`). Several agents may pin one `topic_path`,
    /// so state access must never go through the dir alone.
    pub topic_name: String,
    /// Application configuration
    pub config: Arc<AppConfig>,
    /// Channel name
    pub channel: String,
    /// Channel type (e.g., "websocket", "email")
    pub channel_type: String,
    /// Agent service (optional, for commands that need to query server)
    pub agent: Option<Arc<dyn crate::agent::AgentService>>,
    /// Template directories (layered: L1 global < L2 workdir; topic L3
    /// `.jyc/templates/` is checked first at lookup time)
    pub template_dirs: crate::template_dirs::TemplateDirs,
    /// Path to the config.toml file (for commands that write config)
    pub config_path: Option<PathBuf>,
    /// Per-agent custom commands for this topic's agent
    /// (`[[agents.<pattern>.commands]]`). Empty for non-agent topics.
    /// Used by `/?` so it reflects what actually dispatches at runtime
    /// (per-agent wins on collision with globals).
    pub per_agent_commands: Vec<CustomCommand>,
    /// Compiled hooks for this topic's agent scope (global `[[hooks]]`
    /// merged with `[[agents.<pattern>.hooks]]`). Session lifecycle
    /// commands (`/reset`, `/new`, `/close`) fire notification events
    /// through it; the default empty set no-ops instantly.
    pub hooks: Arc<HookSet>,
}

impl Default for CommandContext {
    /// Test-only default — every handler test that just needs *a*
    /// `CommandContext` can spread `..Default::default()` and only fill
    /// the fields under test. Production code constructs one explicitly
    /// in `topic_manager::worker::process_message`.
    fn default() -> Self {
        Self {
            args: vec![],
            topic_path: PathBuf::new(),
            topic_name: String::new(),
            config: Arc::new(AppConfig::default()),
            channel: String::new(),
            channel_type: String::new(),
            agent: None,
            template_dirs: crate::template_dirs::TemplateDirs::default(),
            config_path: None,
            per_agent_commands: vec![],
            hooks: Arc::new(HookSet::default()),
        }
    }
}

impl CommandContext {
    /// Fire a notification `session_start` hook (source: startup|reset|new).
    pub(crate) async fn session_start_hook(&self, source: &str) {
        self.session_hook(HookEvent::SessionStart, source, true)
            .await;
    }

    /// Fire a notification `session_end` hook (reason: reset|new|close),
    /// before the destructive action so hooks can still archive state.
    pub(crate) async fn session_end_hook(&self, reason: &str) {
        self.session_hook(HookEvent::SessionEnd, reason, false)
            .await;
    }

    async fn session_hook(&self, event: HookEvent, cause: &str, start: bool) {
        if self.hooks.is_empty() {
            return;
        }
        let mut ctx = HookCtx {
            topic: self.topic_name.clone(),
            cwd: self.topic_path.display().to_string(),
            channel: Some(self.channel.clone()),
            ..Default::default()
        };
        if start {
            ctx.source = Some(cause.to_string());
        } else {
            ctx.reason = Some(cause.to_string());
        }
        self.hooks.run(event, Some(cause), &ctx).await;
    }
}

impl std::fmt::Debug for CommandContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandContext")
            .field("args", &self.args)
            .field("topic_path", &self.topic_path)
            .field("config", &self.config)
            .field("channel", &self.channel)
            .field("channel_type", &self.channel_type)
            .field("agent", &self.agent.is_some())
            .field("config_path", &self.config_path)
            .field(
                "per_agent_commands",
                &self
                    .per_agent_commands
                    .iter()
                    .map(|c| &c.name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Result of executing a command.
#[derive(Debug, Clone, Default)]
pub struct CommandResult {
    /// Whether the command succeeded
    pub success: bool,
    /// User-facing result message
    pub message: String,
    /// Error message (if !success)
    pub error: Option<String>,
    /// Text to append to the message body before it reaches the agent.
    ///
    /// Used by user-defined commands to inject their `user_prompt` (and the
    /// skills to use) into the prompt. `None` for commands that only reply.
    pub append_body: Option<String>,
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
                    format!("Error: {}", r.error.as_deref().unwrap_or(&r.message))
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
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

    /// If `true`, the registry will:
    ///
    /// 1. Collapse all tokens after the subcommand on the command line
    ///    into a single string at `args[1]` (space-joined). E.g.
    ///    `/foo bar baz qux` becomes `args = ["bar", "baz qux"]`. This
    ///    lets the handler distinguish command-line content from
    ///    continuation lines.
    /// 2. Push an empty placeholder at `args[1]` when there is no
    ///    first-line content (`/foo` alone), so handlers can index
    ///    `args[1]` uniformly.
    /// 3. Collect continuation lines (non-blank lines after the command)
    ///    as `args[2..]`, one element per line, stopping at the first
    ///    blank line.
    ///
    /// **Caveat**: lines starting with `/` are also collected as
    /// description text — they are NOT dispatched as commands. Users
    /// must insert a blank line between a multi-line command and the
    /// next `/command`. This is intentional: descriptions like
    /// `/path/to/file` should be preserved literally.
    ///
    /// Used by commands whose first argument is free-form multi-line text
    /// (e.g. `/backlog push <description>`). Default `false` preserves
    /// the existing single-line `args` semantics where each
    /// space-separated token is a separate `args` element.
    fn collect_subsequent_lines(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::config::HookConfig;

    fn hook(event: &str, script: &str) -> HookConfig {
        HookConfig {
            event: event.into(),
            matcher: None,
            shell: vec!["sh".into(), "-c".into(), script.into()],
            timeout: None,
        }
    }

    #[tokio::test]
    async fn session_hooks_fire_with_source_and_reason() {
        let tmp = tempfile::tempdir().unwrap();
        // Hooks scan stdin for the cause field and mark hits in the topic
        // dir (which is also the hook's cwd).
        let set = HookSet::for_agent(
            &[
                hook(
                    "session_end",
                    "grep -q '\"reason\":\"reset\"' - && echo end >> hook-events.log",
                ),
                hook(
                    "session_start",
                    "grep -q '\"source\":\"reset\"' - && echo start >> hook-events.log",
                ),
            ],
            "hk",
        );
        let ctx = CommandContext {
            topic_path: tmp.path().to_path_buf(),
            topic_name: "hk-topic".into(),
            hooks: Arc::new(set),
            ..Default::default()
        };
        ctx.session_end_hook("reset").await;
        ctx.session_start_hook("reset").await;
        let content = std::fs::read_to_string(tmp.path().join("hook-events.log"))
            .unwrap_or_else(|e| panic!("marker missing: {e}"));
        assert_eq!(content.lines().collect::<Vec<_>>(), vec!["end", "start"]);
    }

    #[tokio::test]
    async fn session_hooks_noop_with_empty_set() {
        let ctx = CommandContext::default();
        // Must not panic or spawn anything.
        ctx.session_end_hook("close").await;
        ctx.session_start_hook("new").await;
    }

    #[tokio::test]
    async fn session_end_exit2_is_ignored_notification_only() {
        let tmp = tempfile::tempdir().unwrap();
        let set = HookSet::for_agent(&[hook("session_end", "echo hostile >&2; exit 2")], "hk");
        let ctx = CommandContext {
            topic_path: tmp.path().to_path_buf(),
            hooks: Arc::new(set),
            ..Default::default()
        };
        // Notification events never propagate Block — just must not panic.
        ctx.session_end_hook("close").await;
    }
}
