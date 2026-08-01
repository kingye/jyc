//! Local (TUI-only) commands for the dashboard command palette.
//!
//! Unlike `/` commands (registered in `jyc-core` and executed server-side),
//! local commands are dispatched entirely within the TUI: navigation,
//! zen mode, activity pane, external editor, scrolling. The palette reuses
//! the `/` popup UI but never sends anything to the backend.
//!
//! Each command has a [`CommandScope`] controlling on which screen it is
//! offered: the palette on a given screen shows commands scoped to that
//! screen plus all `Shared` commands.

use jyc_types::CommandInfo;

/// Which screen a local command applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    /// Only offered on the dashboard (overview) screen.
    Dashboard,
    /// Only offered on the chat screen.
    Chat,
    /// Offered on both screens.
    Shared,
}

/// A TUI-local action dispatched by the command palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalAction {
    /// Close the chat screen and return to the dashboard.
    OpenDashboard,
    /// Open the chat screen for the selected thread (dashboard only).
    OpenChat,
    /// Start a new chat (pattern select).
    NewChat,
    /// Reload the server configuration.
    ReloadConfig,
    /// Quit the TUI.
    Quit,
    /// Toggle the thread explorer pane (left side).
    ToggleExplorer,
    /// Toggle zen mode (hide/show info pane + status bar).
    ToggleZen,
    /// Cycle the activity pane through its size states.
    CycleActivity,
    /// Open the chat input in an external editor ($EDITOR).
    OpenExternalEditor,
    /// Toggle terminal mouse capture. While on, the wheel scrolls the
    /// chat message area (PR #484); while off, tmux/terminal-native
    /// text selection works. Default is on. A right-aligned status-bar
    /// chip (mirroring the vim mode chip format) shows the state.
    ToggleMouseCapture,
    /// Scroll the message area to the top.
    ScrollTop,
    /// Scroll the message area to the bottom.
    ScrollBottom,
}

/// Static metadata for one palette entry.
pub struct LocalCommand {
    /// Display/filter name (e.g. "toggle zen").
    pub name: &'static str,
    /// Short description shown next to the name.
    pub description: &'static str,
    /// Keybinding hint shown in the palette (display only).
    pub keybinding: &'static str,
    /// Which screen offers this command.
    pub scope: CommandScope,
    /// Action to execute when selected.
    pub action: LocalAction,
}

/// All local commands, in palette display order.
pub fn local_commands() -> &'static [LocalCommand] {
    use CommandScope::{Chat, Dashboard, Shared};
    &[
        LocalCommand {
            name: "open dashboard",
            description: "Close chat and return to the dashboard",
            keybinding: "",
            scope: Chat,
            action: LocalAction::OpenDashboard,
        },
        LocalCommand {
            name: "open chat",
            description: "Open chat for the selected thread",
            keybinding: "Enter",
            scope: Dashboard,
            action: LocalAction::OpenChat,
        },
        LocalCommand {
            name: "new chat",
            description: "Start a new chat (select a pattern)",
            // `c` is only bound on the dashboard screen — no hint here so
            // the chat palette doesn't advertise a key that types into the
            // editor.
            keybinding: "",
            scope: Shared,
            action: LocalAction::NewChat,
        },
        LocalCommand {
            name: "reload config",
            description: "Reload server configuration",
            // `R` is only bound on the dashboard screen (see `new chat`).
            keybinding: "",
            scope: Shared,
            action: LocalAction::ReloadConfig,
        },
        LocalCommand {
            name: "quit",
            description: "Quit the TUI",
            keybinding: "Ctrl+Q",
            scope: Shared,
            action: LocalAction::Quit,
        },
        LocalCommand {
            name: "toggle explorer",
            description: "Show/hide thread explorer pane",
            keybinding: "Ctrl+E",
            scope: Chat,
            action: LocalAction::ToggleExplorer,
        },
        LocalCommand {
            name: "toggle zen",
            description: "Hide/show info pane and status bar",
            keybinding: "Ctrl+Z",
            scope: Chat,
            action: LocalAction::ToggleZen,
        },
        LocalCommand {
            name: "activity pane",
            description: "Cycle activity pane size",
            keybinding: "Ctrl+A",
            scope: Chat,
            action: LocalAction::CycleActivity,
        },
        LocalCommand {
            name: "open in editor",
            description: "Compose input in external $EDITOR",
            keybinding: "Ctrl+O",
            scope: Chat,
            action: LocalAction::OpenExternalEditor,
        },
        LocalCommand {
            name: "toggle mouse",
            description: "Toggle mouse capture",
            keybinding: "",
            scope: Shared,
            action: LocalAction::ToggleMouseCapture,
        },
        LocalCommand {
            name: "scroll top",
            description: "Scroll messages to the top",
            keybinding: "gg",
            scope: Chat,
            action: LocalAction::ScrollTop,
        },
        LocalCommand {
            name: "scroll bottom",
            description: "Scroll messages to the bottom",
            keybinding: "G",
            scope: Chat,
            action: LocalAction::ScrollBottom,
        },
    ]
}

/// Look up the action for a palette entry by exact name.
pub fn find_by_name(name: &str) -> Option<LocalAction> {
    local_commands()
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.action)
}

/// Palette entries for one screen as `CommandInfo` for reuse of the `/`
/// popup UI. Includes commands scoped to `screen` plus all `Shared` ones.
pub fn command_infos_for(screen: CommandScope) -> Vec<CommandInfo> {
    local_commands()
        .iter()
        .filter(|c| c.scope == screen || c.scope == CommandScope::Shared)
        .map(|c| {
            let description = if c.keybinding.is_empty() {
                c.description.to_string()
            } else {
                format!("{} · {}", c.description, c.keybinding)
            };
            CommandInfo {
                name: c.name.to_string(),
                description,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique() {
        let mut names: Vec<_> = local_commands().iter().map(|c| c.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), local_commands().len());
    }

    #[test]
    fn find_by_name_roundtrip() {
        for cmd in local_commands() {
            assert_eq!(find_by_name(cmd.name), Some(cmd.action));
        }
        assert_eq!(find_by_name("nonexistent"), None);
    }

    #[test]
    fn command_infos_for_filters_by_scope() {
        let dashboard = command_infos_for(CommandScope::Dashboard);
        let chat = command_infos_for(CommandScope::Chat);

        let dash_names: Vec<_> = dashboard.iter().map(|c| c.name.as_str()).collect();
        let chat_names: Vec<_> = chat.iter().map(|c| c.name.as_str()).collect();

        // Dashboard screen: dashboard-scoped + shared
        assert!(dash_names.contains(&"open chat"));
        assert!(dash_names.contains(&"new chat"));
        assert!(dash_names.contains(&"reload config"));
        assert!(dash_names.contains(&"quit"));
        assert!(!dash_names.contains(&"open dashboard"));
        assert!(!dash_names.contains(&"toggle zen"));

        // Chat screen: chat-scoped + shared
        assert!(chat_names.contains(&"open dashboard"));
        assert!(chat_names.contains(&"toggle zen"));
        assert!(chat_names.contains(&"new chat"));
        assert!(chat_names.contains(&"reload config"));
        assert!(chat_names.contains(&"quit"));
        assert!(!chat_names.contains(&"open chat"));

        // Shared commands appear on both screens exactly once each.
        let shared_count = local_commands()
            .iter()
            .filter(|c| c.scope == CommandScope::Shared)
            .count();
        assert_eq!(
            dashboard.len() + chat.len(),
            local_commands().len() + shared_count
        );
    }

    #[test]
    fn command_infos_include_keybinding_when_present() {
        let infos = command_infos_for(CommandScope::Chat);
        for info in &infos {
            let cmd = local_commands()
                .iter()
                .find(|c| c.name == info.name)
                .unwrap();
            if cmd.keybinding.is_empty() {
                assert!(!info.description.contains('·'), "{}", info.name);
            } else {
                assert!(info.description.contains('·'), "{}", info.name);
            }
        }
    }
}
