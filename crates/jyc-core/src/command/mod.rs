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

/// Returns the static list of all built-in commands with descriptions.
///
/// IMPORTANT: This list must be kept in sync with the commands actually
/// registered in `CommandRegistry` (see `topic_manager.rs`). If you add
/// a new command handler, add its entry here too.
pub fn all_commands() -> Vec<CommandInfo> {
    vec![
        CommandInfo {
            name: "/model".into(),
            description: "Switch AI model for this topic".into(),
        },
        CommandInfo {
            name: "/plan".into(),
            description: "Switch to plan mode (read-only)".into(),
        },
        CommandInfo {
            name: "/build".into(),
            description: "Switch to build mode (full execution)".into(),
        },
        CommandInfo {
            name: "/reset".into(),
            description: "Reset session, keep chat history".into(),
        },
        CommandInfo {
            name: "/new".into(),
            description: "Reset session and clear chat history".into(),
        },
        CommandInfo {
            name: "/close".into(),
            description: "Close and delete this topic (requires --confirm or -y)".into(),
        },
        CommandInfo {
            name: "/template".into(),
            description: "Apply or re-apply topic template".into(),
        },
        CommandInfo {
            name: "/cancel".into(),
            description: "Cancel current AI processing".into(),
        },
        CommandInfo {
            name: "/?".into(),
            description: "Show available commands".into(),
        },
        CommandInfo {
            name: "/pin".into(),
            description: "Pin this ad-hoc websocket topic to config.toml".into(),
        },
        CommandInfo {
            name: "/unpin".into(),
            description: "Remove pinned topic configuration from config.toml".into(),
        },
        CommandInfo {
            name: "/thinking".into(),
            description: "Show or hide AI thinking/reasoning content".into(),
        },
        CommandInfo {
            name: "/exchange".into(),
            description: "Show shareable URLs for this topic's published files".into(),
        },
        CommandInfo {
            name: "/context".into(),
            description: "View or change the context strategy / debug-dump wire payload".into(),
        },
        CommandInfo {
            name: "/info".into(),
            description: "Show topic info (mode, model, tokens, cost, files)".into(),
        },
    ]
}

/// Returns built-in commands plus user-defined `[[commands]]` from config.
///
/// Used by the `/?` help output and the dashboard command popup so both
/// surfaces list custom commands alongside the built-ins.
pub fn all_commands_with(custom: &[CustomCommand]) -> Vec<CommandInfo> {
    let mut commands = all_commands();
    commands.extend(custom.iter().map(|c| CommandInfo {
        // Match CustomCommandHandler::new()'s normalization so the popup shows
        // the name the registry actually dispatches on.
        name: format!("/{}", c.name.trim().trim_start_matches('/').to_lowercase()),
        description: if c.description.trim().is_empty() {
            "(no description)".into()
        } else {
            c.description.clone()
        },
    }));
    commands
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
        ] {
            assert!(
                names.contains(expected),
                "all_commands() is missing '{expected}'. Add it to keep the command popup in sync."
            );
        }
        assert_eq!(
            commands.len(),
            15,
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
            user_prompt: "do it".into(),
        }
    }

    #[test]
    fn test_all_commands_with_appends_custom() {
        let builtin_count = all_commands().len();
        let commands = all_commands_with(&[custom("review", "Review the PR")]);

        assert_eq!(commands.len(), builtin_count + 1);
        let last = commands.last().unwrap();
        assert_eq!(last.name, "/review");
        assert_eq!(last.description, "Review the PR");
    }

    #[test]
    fn test_all_commands_with_normalizes_slash() {
        let commands = all_commands_with(&[custom("/review", "d")]);
        assert!(commands.iter().any(|c| c.name == "/review"));
        assert!(!commands.iter().any(|c| c.name == "//review"));
    }

    #[test]
    fn test_all_commands_with_empty_matches_builtin() {
        assert_eq!(all_commands_with(&[]).len(), all_commands().len());
    }

    #[test]
    fn test_all_commands_with_falls_back_on_empty_description() {
        let commands = all_commands_with(&[custom("review", "")]);
        let entry = commands.iter().find(|c| c.name == "/review").unwrap();
        assert_eq!(entry.description, "(no description)");
    }

    /// The popup must show the name the registry dispatches on, which is
    /// lowercase (see CustomCommandHandler::new).
    #[test]
    fn test_all_commands_with_lowercases_name() {
        let commands = all_commands_with(&[custom("Review", "d")]);
        assert!(commands.iter().any(|c| c.name == "/review"));
        assert!(!commands.iter().any(|c| c.name == "/Review"));
    }
}
