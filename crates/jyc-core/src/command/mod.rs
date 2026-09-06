pub mod backlog_handler;
pub mod cancel_handler;
pub mod close_handler;
pub mod context_handler;
pub mod custom_handler;
pub mod exchange_handler;
pub mod handler;
pub mod help_handler;
pub mod info_handler;
pub mod mode_handler;
pub mod model_handler;
pub mod new_handler;
pub mod pin_common;
pub mod pin_handler;
pub mod registry;
pub mod reset_handler;
pub mod template_handler;
pub mod thinking_handler;
pub mod unpin_handler;

pub use model_handler::list_available_models;

use jyc_types::{CommandInfo, CustomCommand};

/// Returns the static list of built-in commands with descriptions.
///
/// This is the TUI's legacy fallback for older servers that don't
/// populate `topic.commands` in the inspect API. The dashboard uses
/// it to show at least the built-ins when the popup would otherwise
/// be empty.
///
/// IMPORTANT: This list must be kept in sync with the commands actually
/// registered in `CommandRegistry` (see `topic_manager.rs`). If you add
/// a new command handler, add its entry here too.
pub fn all_commands() -> Vec<CommandInfo> {
    vec![
        CommandInfo {
            name: "/model".into(),
            description: "Switch AI model for this topic".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/plan".into(),
            description: "Switch to plan mode (read-only)".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/build".into(),
            description: "Switch to build mode (full execution)".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/reset".into(),
            description: "Reset session, keep chat history".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/new".into(),
            description: "Reset session and clear chat history".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/close".into(),
            description: "Close and delete this topic (requires --confirm or -y)".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/template".into(),
            description: "Apply or re-apply topic template".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/cancel".into(),
            description: "Cancel current AI processing".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/?".into(),
            description: "Show available commands".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/pin".into(),
            description: "Pin this ad-hoc websocket topic to config.toml".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/unpin".into(),
            description: "Remove pinned topic configuration from config.toml".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/thinking".into(),
            description: "Show or hide AI thinking/reasoning content".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/exchange".into(),
            description: "Show shareable URLs for this topic's published files".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/context".into(),
            description: "View or change the context strategy / debug-dump wire payload".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/info".into(),
            description: "Show topic info (mode, model, tokens, cost, files)".into(),
            ..Default::default()
        },
        CommandInfo {
            name: "/backlog".into(),
            // `/backlog pop` injects the popped text into the agent's
            // next turn via `append_body`, so it continues into an agent
            // run and needs a progress indicator on piped channels
            // (feishu). The other subcommands (`push`/`list`/`rm`) reply
            // instantly — but the flag is set at the command level, not
            // per-subcommand, because the channels.rs watcher spawns
            // before dispatch and cannot inspect the subcommand.
            description: "Save and replay user messages (push|list|pop|rm)".into(),
            continues_to_agent: true,
        },
    ]
}

/// Returns built-in commands plus user-defined globals (`[[commands]]`)
/// and the topic's per-agent commands (`[[agents.<name>.commands]]`).
///
/// Used by the `/?` help output and the dashboard command popup so both
/// surfaces list custom commands alongside the built-ins.
///
/// Per-agent wins on name collision (matches runtime semantics —
/// `CommandRegistry::register` overwrites on collision with a `tracing::warn`).
/// The popup must show the name the registry actually dispatches on.
///
/// Output is sorted by name so repeated calls return the same order. The
/// TUI's `/` popup stores selection by index and re-filters against this
/// list every render; if ordering were unstable (e.g., a `HashMap`-backed
/// dedup), the overview poll (500 ms) would replace `app.chat.commands`
/// with a freshly-shuffled vec and the popup would appear to flicker.
pub fn all_commands_with(
    global: &[CustomCommand],
    per_agent: &[CustomCommand],
) -> Vec<CommandInfo> {
    let mut by_name: std::collections::HashMap<String, CommandInfo> = all_commands()
        .into_iter()
        .map(|c| (c.name.clone(), c))
        .collect();
    for c in global {
        let info = custom_to_info(c);
        by_name.insert(info.name.clone(), info);
    }
    for c in per_agent {
        let info = custom_to_info(c);
        by_name.insert(info.name.clone(), info);
    }
    let mut sorted: Vec<CommandInfo> = by_name.into_values().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    sorted
}

/// Single canonical lookup: given a pattern/agent name and the live
/// `AppConfig`, return the per-agent custom commands for that pattern.
/// Empty `Vec` when no such agent is defined.
///
/// Both the worker (used by `/?` help) and the inspect server (used by
/// the `/` popup) call this so the answer is identical for identical
/// inputs. `cfg.agents` is the single source for `[[agents.<name>.commands]]`;
/// everything else (built-ins, globals, per-topic wiring) lives elsewhere
/// and is composed via [`all_commands_with`].
pub fn per_agent_commands(cfg: &jyc_types::AppConfig, pattern_name: &str) -> Vec<CustomCommand> {
    cfg.agents
        .get(pattern_name)
        .map(|a| a.commands.clone())
        .unwrap_or_default()
}

fn custom_to_info(c: &CustomCommand) -> CommandInfo {
    CommandInfo {
        // Match CustomCommandHandler::new()'s normalization so the popup shows
        // the name the registry actually dispatches on.
        name: format!("/{}", c.name.trim().trim_start_matches('/').to_lowercase()),
        description: if c.description.trim().is_empty() {
            "(no description)".into()
        } else {
            c.description.clone()
        },
        // `shell` commands run an argv directly and reply synchronously
        // (no agent run, no `ProcessingStarted`); prompt commands inject
        // `user_prompt` via `append_body` and continue into the agent.
        continues_to_agent: c.shell.is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify all_commands() contains the expected set of commands.
    /// If this test fails, update both the registry in topic_manager.rs
    /// and the all_commands() list.
    #[test]
    fn test_all_commands_has_expected_names() {
        let commands = all_commands();
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        for expected in &[
            "/model",
            "/plan",
            "/build",
            "/reset",
            "/new",
            "/close",
            "/template",
            "/cancel",
            "/?",
            "/pin",
            "/unpin",
            "/thinking",
            "/exchange",
            "/context",
            "/info",
            "/backlog",
        ] {
            assert!(
                names.contains(expected),
                "all_commands() is missing '{expected}'. Add it to keep the command popup in sync."
            );
        }
        assert_eq!(
            commands.len(),
            16,
            "all_commands() count changed. Update this test if intentional."
        );
    }

    #[test]
    fn test_all_commands_has_no_duplicates() {
        let commands = all_commands();
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            names.len(),
            sorted.len(),
            "all_commands() contains duplicate names"
        );
    }

    /// `jyc_types::BUILTIN_COMMAND_NAMES` drives config validation's
    /// "shadows a built-in" check, so it must match `all_commands()`.
    #[test]
    fn test_builtin_names_match_all_commands() {
        let mut from_commands: Vec<String> =
            all_commands().iter().map(|c| c.name.clone()).collect();
        let mut from_const: Vec<String> = jyc_types::BUILTIN_COMMAND_NAMES
            .iter()
            .map(|s| s.to_string())
            .collect();
        from_commands.sort();
        from_const.sort();
        assert_eq!(
            from_commands, from_const,
            "BUILTIN_COMMAND_NAMES (jyc-types) is out of sync with all_commands()"
        );
    }

    fn custom(name: &str, description: &str) -> CustomCommand {
        CustomCommand {
            name: name.into(),
            description: description.into(),
            mode: None,
            skills: None,
            user_prompt: Some("do it".into()),
            shell: None,
        }
    }

    #[test]
    fn test_all_commands_with_appends_custom() {
        let builtin_count = all_commands().len();
        let commands = all_commands_with(&[custom("review", "Review the PR")], &[]);

        assert_eq!(commands.len(), builtin_count + 1);
        let entry = commands.iter().find(|c| c.name == "/review").unwrap();
        assert_eq!(entry.description, "Review the PR");
    }

    #[test]
    fn test_all_commands_with_normalizes_slash() {
        let commands = all_commands_with(&[custom("/review", "d")], &[]);
        assert!(commands.iter().any(|c| c.name == "/review"));
        assert!(!commands.iter().any(|c| c.name == "//review"));
    }

    #[test]
    fn test_all_commands_with_empty_matches_builtin() {
        assert_eq!(all_commands_with(&[], &[]).len(), all_commands().len());
    }

    #[test]
    fn test_all_commands_with_falls_back_on_empty_description() {
        let commands = all_commands_with(&[custom("review", "")], &[]);
        let entry = commands.iter().find(|c| c.name == "/review").unwrap();
        assert_eq!(entry.description, "(no description)");
    }

    /// The popup must show the name the registry dispatches on, which is
    /// lowercase (see CustomCommandHandler::new).
    #[test]
    fn test_all_commands_with_lowercases_name() {
        let commands = all_commands_with(&[custom("Review", "d")], &[]);
        assert!(commands.iter().any(|c| c.name == "/review"));
        assert!(!commands.iter().any(|c| c.name == "/Review"));
    }

    /// Per-agent commands are merged alongside globals so the topic's
    /// `/?` and command popup reflect what actually dispatches at runtime.
    #[test]
    fn test_all_commands_with_includes_per_agent() {
        let builtin_count = all_commands().len();
        let commands = all_commands_with(
            &[custom("review", "global review")],
            &[custom("deploy", "per-agent deploy")],
        );
        assert_eq!(commands.len(), builtin_count + 2);
        assert!(commands.iter().any(|c| c.name == "/review"));
        assert!(commands.iter().any(|c| c.name == "/deploy"));
    }

    /// Per-agent wins on name collision so the popup shows what the
    /// registry dispatches on (matches `CommandRegistry::register`
    /// overwrite semantics).
    #[test]
    fn test_all_commands_with_per_agent_overwrites_global() {
        let commands = all_commands_with(
            &[custom("review", "global")],
            &[custom("review", "per-agent")],
        );
        let entries: Vec<&CommandInfo> = commands.iter().filter(|c| c.name == "/review").collect();
        assert_eq!(entries.len(), 1, "duplicate /review");
        assert_eq!(entries[0].description, "per-agent");
    }

    /// `all_commands_with` must return the same order on repeated calls.
    /// The TUI `/` popup stores selection by index and re-filters this
    /// list every render. If ordering were unstable (e.g., a `HashMap`-
    /// backed dedup without a final sort), the overview poll (500 ms)
    /// would replace `app.chat.commands` with a freshly-shuffled vec
    /// and the popup would appear to flicker — selection jumps, list
    /// reshuffles, navigation breaks. The fix sorts the final vec by
    /// name; this test pins that contract.
    #[test]
    fn test_all_commands_with_is_stably_ordered() {
        let a = all_commands_with(
            &[custom("review", "r"), custom("audit", "a")],
            &[custom("deploy", "d"), custom("backup", "b")],
        );
        let b = all_commands_with(
            &[custom("review", "r"), custom("audit", "a")],
            &[custom("deploy", "d"), custom("backup", "b")],
        );
        let names_a: Vec<&str> = a.iter().map(|c| c.name.as_str()).collect();
        let names_b: Vec<&str> = b.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names_a, names_b,
            "ordering must be deterministic across calls"
        );
        let mut sorted = names_a.clone();
        sorted.sort();
        assert_eq!(names_a, sorted, "output must be in ascending name order");
    }

    /// `channels.rs` uses `continues_to_agent` to decide whether to spawn a
    /// progress watcher on piped channels (feishu). Today only `/backlog`
    /// continues into the agent (via `append_body` on `pop`); every other
    /// built-in replies instantly. If this drifts, the watcher either
    /// sleeps for nothing (false negative) or misses a real agent run
    /// (false positive — user-visible bug).
    #[test]
    fn test_only_backlog_continues_to_agent_in_builtins() {
        let commands = all_commands();
        for c in &commands {
            let expected = c.name == "/backlog";
            assert_eq!(
                c.continues_to_agent, expected,
                "command {} has continues_to_agent={}, expected {}",
                c.name, c.continues_to_agent, expected,
            );
        }
    }

    /// Prompt-injection custom commands inject `user_prompt` via
    /// `append_body` and continue into the agent run.
    #[test]
    fn test_custom_prompt_command_continues_to_agent() {
        let commands = all_commands_with(&[custom("review", "Review the PR")], &[]);
        let entry = commands.iter().find(|c| c.name == "/review").unwrap();
        assert!(entry.continues_to_agent);
    }

    /// Shell custom commands reply synchronously with stdout/stderr and
    /// never reach the agent — the progress watcher would sleep until
    /// MAX_LIFETIME for nothing.
    #[test]
    fn test_custom_shell_command_does_not_continue_to_agent() {
        let cmd = CustomCommand {
            name: "gitlog".into(),
            description: "git log".into(),
            mode: None,
            skills: None,
            user_prompt: None,
            shell: Some(vec!["git".into(), "log".into()]),
        };
        let commands = all_commands_with(&[cmd], &[]);
        let entry = commands.iter().find(|c| c.name == "/gitlog").unwrap();
        assert!(!entry.continues_to_agent);
    }

    /// `per_agent_commands` is the single canonical lookup the worker
    /// AND the inspect server share. If this test passes for one, it
    /// must pass for both — by construction they go through the same
    /// `cfg.agents.get(...)`.
    #[test]
    fn test_per_agent_commands_returns_agent_commands() {
        use std::collections::HashMap;
        let mut agents = HashMap::new();
        agents.insert(
            "jyc".to_string(),
            jyc_types::AgentConfig {
                commands: vec![custom("deploy", "self deploy")],
                ..Default::default()
            },
        );
        let cfg = jyc_types::AppConfig {
            agents,
            ..Default::default()
        };
        let got = per_agent_commands(&cfg, "jyc");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "deploy");
    }

    #[test]
    fn test_per_agent_commands_unknown_pattern_returns_empty() {
        let cfg = jyc_types::AppConfig::default();
        assert!(per_agent_commands(&cfg, "does-not-exist").is_empty());
        assert!(per_agent_commands(&cfg, "").is_empty());
    }
}
