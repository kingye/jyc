//! Local (TUI-only) commands for the leader-key popup.
//!
//! Unlike `/` commands (registered in `jyc-core` and executed server-side),
//! local commands are dispatched entirely within the TUI: navigation,
//! zen mode, activity pane, external editor, scrolling. The leader
//! popup is invoked with `Ctrl+P` and shows all in-scope commands. Typing the assigned
//! keys (one or two chars) dispatches the action immediately; `Esc`
//! closes. Multi-char sequences (e.g., `gg` for scroll top) wait for the
//! next key while a prefix is still ambiguous.
//!
//! Each command has a [`CommandScope`] controlling on which screen it is
//! offered: the leader on a given screen shows commands scoped to that
//! screen plus all `Shared` commands.

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

/// A TUI-local action dispatched by the leader-key popup.
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
    /// Toggle zen mode (hide all aux panes; restore the snapshot on exit).
    ToggleZen,
    /// Show/hide the activity pane (bottom 20% on, hidden off).
    ToggleActivity,
    /// Show/hide the bottom status bar.
    ToggleStatus,
    /// Show/hide the thread info pane (right side).
    ToggleInfo,
    /// Open the chat input in an external editor ($EDITOR).
    OpenExternalEditor,
    /// Toggle terminal mouse capture. While on, the wheel scrolls the
    /// chat message area (PR #484); while off, tmux/terminal-native
    /// text selection works. Default is on. A right-aligned status-bar
    /// chip (mirroring the status-bar chip format) shows the state.
    ToggleMouseCapture,
    /// Focus the chat message area for keyboard scrolling.
    FocusChat,
    /// Scroll the message area to the top.
    ScrollTop,
    /// Scroll the message area to the bottom.
    ScrollBottom,
    /// Open the `/` command popup (leader equivalent of typing `/` in
    /// an empty input).
    OpenCommandPopup,
}

/// Static metadata for one leader entry.
pub struct LocalCommand {
    /// Display/filter name (e.g. "toggle zen").
    pub name: &'static str,
    /// Short description shown next to the name.
    pub description: &'static str,
    /// Which screen offers this command.
    pub scope: CommandScope,
    /// Action to execute when selected.
    pub action: LocalAction,
    /// Leader keys (1-2 chars) the user types after the leader trigger
    /// to dispatch this command. Multi-char keys support sequences like
    /// `gg` (scroll top): the parser waits for the next key when the
    /// buffer is a prefix of any entry's keys. Keys must be unique within
    /// a scope but may repeat across scopes (e.g. `c` opens chat on the
    /// dashboard and focuses the message area in chat).
    pub leader_keys: &'static str,
}

/// All local commands, in leader display order.
pub fn local_commands() -> &'static [LocalCommand] {
    use CommandScope::{Chat, Dashboard, Shared};
    &[
        LocalCommand {
            name: "command popup",
            description: "Open the / command popup",
            scope: Chat,
            action: LocalAction::OpenCommandPopup,
            leader_keys: "/",
        },
        LocalCommand {
            name: "open dashboard",
            description: "Close chat and return to the dashboard",
            scope: Chat,
            action: LocalAction::OpenDashboard,
            leader_keys: "d",
        },
        LocalCommand {
            name: "open chat",
            description: "Open chat for the selected thread",
            scope: Dashboard,
            action: LocalAction::OpenChat,
            leader_keys: "c",
        },
        LocalCommand {
            name: "new chat",
            description: "Start a new chat (select a pattern)",
            scope: Shared,
            action: LocalAction::NewChat,
            leader_keys: "n",
        },
        LocalCommand {
            name: "reload config",
            description: "Reload server configuration",
            scope: Shared,
            action: LocalAction::ReloadConfig,
            leader_keys: "r",
        },
        LocalCommand {
            name: "quit",
            description: "Quit the TUI",
            scope: Shared,
            action: LocalAction::Quit,
            leader_keys: "q",
        },
        LocalCommand {
            name: "toggle explorer",
            description: "Show/hide thread explorer pane",
            scope: Chat,
            action: LocalAction::ToggleExplorer,
            leader_keys: "e",
        },
        LocalCommand {
            name: "toggle zen",
            description: "Zen mode: hide all panes, restore on exit",
            scope: Chat,
            action: LocalAction::ToggleZen,
            leader_keys: "z",
        },
        LocalCommand {
            name: "toggle activity",
            description: "Show/hide the activity pane",
            scope: Chat,
            action: LocalAction::ToggleActivity,
            leader_keys: "a",
        },
        LocalCommand {
            name: "toggle status",
            description: "Show/hide the bottom status bar",
            scope: Chat,
            action: LocalAction::ToggleStatus,
            leader_keys: "s",
        },
        LocalCommand {
            name: "toggle info",
            description: "Show/hide thread info pane",
            scope: Chat,
            action: LocalAction::ToggleInfo,
            leader_keys: "i",
        },
        LocalCommand {
            name: "open in editor",
            description: "Compose input in external $EDITOR",
            scope: Chat,
            action: LocalAction::OpenExternalEditor,
            leader_keys: "o",
        },
        LocalCommand {
            name: "toggle mouse",
            description: "Toggle mouse capture",
            scope: Shared,
            action: LocalAction::ToggleMouseCapture,
            leader_keys: "m",
        },
        LocalCommand {
            name: "focus chat",
            description: "Focus the message area (j/k scroll; typing returns to input)",
            scope: Chat,
            action: LocalAction::FocusChat,
            leader_keys: "c",
        },
        LocalCommand {
            name: "scroll top",
            description: "Scroll messages to the top",
            scope: Chat,
            action: LocalAction::ScrollTop,
            leader_keys: "gg",
        },
        LocalCommand {
            name: "scroll bottom",
            description: "Scroll messages to the bottom",
            scope: Chat,
            action: LocalAction::ScrollBottom,
            leader_keys: "G",
        },
    ]
}

/// One leader entry (used by the `Leader` controller).
#[derive(Debug, Clone)]
pub struct LeaderEntry {
    pub keys: &'static str,
    #[allow(dead_code)]
    pub name: &'static str,
    #[allow(dead_code)]
    pub description: &'static str,
    #[allow(dead_code)]
    pub action: LocalAction,
}

/// Leader entries for one screen. Includes commands scoped to `screen`
/// plus all `Shared` ones, in leader display order.
pub fn leader_entries_for(screen: CommandScope) -> Vec<LeaderEntry> {
    local_commands()
        .iter()
        .filter(|c| c.scope == screen || c.scope == CommandScope::Shared)
        .map(|c| LeaderEntry {
            keys: c.leader_keys,
            name: c.name,
            description: c.description,
            action: c.action,
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

    /// No two commands visible in the same scope may share a leader-key
    /// prefix relationship: dispatch would be ambiguous. A command's keys
    /// must not be a prefix of (or equal to) any other visible command's
    /// keys.
    #[test]
    fn leader_keys_unique_per_scope() {
        for scope in [CommandScope::Dashboard, CommandScope::Chat] {
            let entries = leader_entries_for(scope);
            for (i, a) in entries.iter().enumerate() {
                for (j, b) in entries.iter().enumerate() {
                    if i == j {
                        continue;
                    }
                    assert!(
                        a.keys != b.keys,
                        "{scope:?}: duplicate leader key {:?} on {} and {}",
                        a.keys,
                        a.name,
                        b.name
                    );
                    assert!(
                        !a.keys.starts_with(b.keys) && !b.keys.starts_with(a.keys),
                        "{scope:?}: leader keys {:?} ({}) and {:?} ({}) share a prefix — ambiguous dispatch",
                        a.keys,
                        a.name,
                        b.keys,
                        b.name
                    );
                }
            }
        }
    }

    #[test]
    fn leader_entries_for_filters_by_scope() {
        let dashboard = leader_entries_for(CommandScope::Dashboard);
        let chat = leader_entries_for(CommandScope::Chat);

        let dash_keys: Vec<_> = dashboard.iter().map(|e| e.keys).collect();
        let chat_keys: Vec<_> = chat.iter().map(|e| e.keys).collect();

        // Dashboard screen: dashboard-scoped + shared.
        assert!(dash_keys.contains(&"c"), "open chat must be on dashboard");
        assert!(dash_keys.contains(&"n"));
        assert!(dash_keys.contains(&"r"));
        assert!(dash_keys.contains(&"q"));
        assert!(
            !dash_keys.contains(&"d"),
            "open dashboard must be Chat-only"
        );
        assert!(!dash_keys.contains(&"z"));

        // Chat screen: chat-scoped + shared.
        assert!(chat_keys.contains(&"/"), "command popup must be Chat-only");
        assert!(!dash_keys.contains(&"/"));
        assert!(chat_keys.contains(&"d"));
        assert!(chat_keys.contains(&"z"));
        assert!(chat_keys.contains(&"n"));
        assert!(chat_keys.contains(&"r"));
        assert!(chat_keys.contains(&"q"));
        // `c` is focus chat on the chat screen, open chat on the dashboard.
        assert!(chat_keys.contains(&"c"), "focus chat must be on chat");
        let chat_c = chat.iter().find(|e| e.keys == "c").unwrap();
        assert_eq!(chat_c.action, LocalAction::FocusChat);
        let dash_c = dashboard.iter().find(|e| e.keys == "c").unwrap();
        assert_eq!(dash_c.action, LocalAction::OpenChat);
    }
}
