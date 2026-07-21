pub mod cancel_handler;
pub mod close_handler;
pub mod handler;
pub mod help_handler;
pub mod mode_handler;
pub mod model_handler;
pub mod new_handler;
pub mod registry;
pub mod reset_handler;
pub mod template_handler;

pub use model_handler::list_available_models;

use jyc_types::CommandInfo;

/// Returns the static list of all available commands with descriptions.
///
/// IMPORTANT: This list must be kept in sync with the commands actually
/// registered in `CommandRegistry` (see `thread_manager.rs`). If you add
/// a new command handler, add its entry here too.
pub fn all_commands() -> Vec<CommandInfo> {
    vec![
        CommandInfo {
            name: "/model".into(),
            description: "Switch AI model for this thread".into(),
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
            description: "Close and delete this thread".into(),
        },
        CommandInfo {
            name: "/template".into(),
            description: "Apply or re-apply thread template".into(),
        },
        CommandInfo {
            name: "/cancel".into(),
            description: "Cancel current AI processing".into(),
        },
        CommandInfo {
            name: "/?".into(),
            description: "Show available commands".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify all_commands() contains the expected set of commands.
    /// If this test fails, update both the registry in thread_manager.rs
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
        ] {
            assert!(
                names.contains(expected),
                "all_commands() is missing '{expected}'. Add it to keep the command popup in sync."
            );
        }
        assert_eq!(
            commands.len(),
            9,
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
}
