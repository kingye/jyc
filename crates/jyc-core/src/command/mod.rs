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

use jyc_types::CommandInfo;

/// Returns the static list of all available commands with descriptions.
///
/// This is the single source of truth for the command palette in the
/// dashboard TUI. Every command registered in `CommandRegistry` should
/// appear here.
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
