//! Shared command-palette controller used by both the dashboard and the
//! chat screen.
//!
//! Bundles the popup state with the command list for one screen scope and
//! wraps the generic popup machinery in `command_popup.rs` so each screen
//! only needs: open with [`Palette::new`], feed keys to
//! [`Palette::handle_key`], and draw with [`Palette::render`].

use crossterm::event::KeyEvent;
use jyc_types::CommandInfo;
use ratatui::{Frame, layout::Rect};

use super::super::command_popup::{
    CommandPopupState, PopupAction, handle_popup_key, render_palette_popup,
};
use super::local_commands::{self, CommandScope, LocalAction};

/// Outcome of feeding a key event to an open palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteResult {
    /// Key was consumed; the palette stays open.
    Consumed,
    /// The palette closed without an action (Esc).
    Closed,
    /// A command was selected; the palette closed and the caller should
    /// dispatch the action.
    Action(LocalAction),
}

/// An open command palette for one screen.
pub struct Palette {
    state: CommandPopupState,
    commands: Vec<CommandInfo>,
}

impl Palette {
    /// Open the palette with the commands for `scope` (plus shared ones).
    pub fn new(scope: CommandScope) -> Self {
        Self {
            state: CommandPopupState::new(),
            commands: local_commands::command_infos_for(scope),
        }
    }

    /// Feed a key event. The caller owns closing (drop the palette) and
    /// dispatching the returned action.
    pub fn handle_key(&mut self, key: KeyEvent) -> PaletteResult {
        match handle_popup_key(key, &mut self.state, &self.commands, &[]) {
            PopupAction::None => PaletteResult::Consumed,
            PopupAction::Close => PaletteResult::Closed,
            PopupAction::Send(name) | PopupAction::CopyToInput(name) => {
                match local_commands::find_by_name(&name) {
                    Some(action) => PaletteResult::Action(action),
                    None => PaletteResult::Closed,
                }
            }
        }
    }

    /// Render the palette as a centered overlay.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        render_palette_popup(frame, area, &self.state, &self.commands);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn esc_closes_palette() {
        let mut palette = Palette::new(CommandScope::Chat);
        assert_eq!(palette.handle_key(key(KeyCode::Esc)), PaletteResult::Closed);
    }

    #[test]
    fn typing_consumes_keys() {
        let mut palette = Palette::new(CommandScope::Chat);
        assert_eq!(
            palette.handle_key(key(KeyCode::Char('z'))),
            PaletteResult::Consumed
        );
    }

    #[test]
    fn enter_on_filtered_command_returns_action() {
        let mut palette = Palette::new(CommandScope::Chat);
        for c in "toggle z".chars() {
            palette.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            palette.handle_key(key(KeyCode::Enter)),
            PaletteResult::Action(LocalAction::ToggleZen)
        );
    }

    #[test]
    fn enter_on_toggle_mouse_returns_action() {
        // `toggle mouse` is Shared scope — must be reachable from both
        // dashboard and chat palettes.
        for scope in [CommandScope::Dashboard, CommandScope::Chat] {
            let mut palette = Palette::new(scope);
            for c in "toggle mouse".chars() {
                palette.handle_key(key(KeyCode::Char(c)));
            }
            assert_eq!(
                palette.handle_key(key(KeyCode::Enter)),
                PaletteResult::Action(LocalAction::ToggleMouseCapture),
                "toggle mouse must be offered on {scope:?}"
            );
        }
    }

    #[test]
    fn enter_without_match_stays_open() {
        let mut palette = Palette::new(CommandScope::Dashboard);
        for c in "zzzzz".chars() {
            palette.handle_key(key(KeyCode::Char(c)));
        }
        // No matching command — Enter is a no-op, palette stays open.
        assert_eq!(
            palette.handle_key(key(KeyCode::Enter)),
            PaletteResult::Consumed
        );
    }

    #[test]
    fn dashboard_palette_offers_dashboard_and_shared_only() {
        let mut palette = Palette::new(CommandScope::Dashboard);
        // "open chat" is dashboard-scoped and selectable.
        for c in "open chat".chars() {
            palette.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            palette.handle_key(key(KeyCode::Enter)),
            PaletteResult::Action(LocalAction::OpenChat)
        );
    }
}
