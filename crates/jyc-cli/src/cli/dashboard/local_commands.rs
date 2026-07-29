//! Local (TUI-only) commands for the dashboard command palette.
//!
//! Unlike `/` commands (registered in `jyc-core` and executed server-side),
//! local commands are dispatched entirely within the TUI: zen mode,
//! activity pane, external editor, scrolling. The palette reuses the `/`
//! popup UI but never sends anything to the backend.

use jyc_types::CommandInfo;

/// A TUI-local action dispatched by the command palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalAction {
    /// Toggle zen mode (hide/show info pane + status bar).
    ToggleZen,
    /// Cycle the activity pane through its size states.
    CycleActivity,
    /// Open the chat input in an external editor ($EDITOR).
    OpenExternalEditor,
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
    /// Action to execute when selected.
    pub action: LocalAction,
}

/// All local commands shown in the palette.
pub fn local_commands() -> &'static [LocalCommand] {
    &[
        LocalCommand {
            name: "toggle zen",
            description: "Hide/show info pane and status bar",
            keybinding: "Ctrl+Z",
            action: LocalAction::ToggleZen,
        },
        LocalCommand {
            name: "activity pane",
            description: "Cycle activity pane size",
            keybinding: "Ctrl+A",
            action: LocalAction::CycleActivity,
        },
        LocalCommand {
            name: "open in editor",
            description: "Compose input in external $EDITOR",
            keybinding: "Ctrl+E",
            action: LocalAction::OpenExternalEditor,
        },
        LocalCommand {
            name: "scroll top",
            description: "Scroll messages to the top",
            keybinding: "gg",
            action: LocalAction::ScrollTop,
        },
        LocalCommand {
            name: "scroll bottom",
            description: "Scroll messages to the bottom",
            keybinding: "G",
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

/// Palette entries as `CommandInfo` for reuse of the `/` popup UI.
pub fn command_infos() -> Vec<CommandInfo> {
    local_commands()
        .iter()
        .map(|c| CommandInfo {
            name: c.name.to_string(),
            description: format!("{} · {}", c.description, c.keybinding),
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
    fn command_infos_include_keybinding() {
        let infos = command_infos();
        assert_eq!(infos.len(), local_commands().len());
        for info in &infos {
            assert!(info.description.contains('·'), "{}", info.description);
        }
    }
}
