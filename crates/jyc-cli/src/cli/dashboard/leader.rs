//! Leader-key popup controller used by both the dashboard and the chat
//! screen.
//!
//! Invoked by `Ctrl+P`.
//! Shows all local commands for the active scope with their assigned
//! leader keys; typing the keys (one or two chars) dispatches the action
//! immediately, `Esc` closes. Multi-char keys (e.g., `gg` for scroll
//! top) wait for the next key when the current buffer is a prefix of
//! some entry's keys.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

use super::local_commands::{self, CommandScope, LeaderEntry, LocalAction};

/// Outcome of feeding a key event to an open leader popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderResult {
    /// Key was consumed; the leader stays open.
    Consumed,
    /// The leader closed without an action (`Esc`).
    Closed,
    /// A command was selected; the leader closed and the caller should
    /// dispatch the action.
    Action(LocalAction),
}

/// An open leader-key popup for one screen.
pub struct Leader {
    entries: Vec<LeaderEntry>,
    buffer: String,
}

impl Leader {
    /// Open the leader with the commands for `scope` (plus shared ones).
    pub fn new(scope: CommandScope) -> Self {
        Self {
            entries: local_commands::leader_entries_for(scope),
            buffer: String::new(),
        }
    }

    /// Current leader-key buffer (what the user has typed so far).
    #[allow(dead_code)]
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// Feed a key event. The caller owns closing (drop the leader) and
    /// dispatching the returned action.
    pub fn handle_key(&mut self, key: KeyEvent) -> LeaderResult {
        match key.code {
            KeyCode::Esc => LeaderResult::Closed,
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let mut buf = std::mem::take(&mut self.buffer);
                buf.push(c);
                // Scope-aware lookup: keys may repeat across scopes (e.g.
                // `c` = open chat on the dashboard, focus chat in chat),
                // so match against this screen's entries only.
                if let Some(entry) = self.entries.iter().find(|e| e.keys == buf) {
                    let action = entry.action;
                    self.buffer.clear();
                    LeaderResult::Action(action)
                } else if self.entries.iter().any(|e| e.keys.starts_with(&buf)) {
                    // Buffer is a prefix of some entry — wait for next key.
                    self.buffer = buf;
                    LeaderResult::Consumed
                } else {
                    // No exact match and no entry has this prefix — reset.
                    self.buffer.clear();
                    LeaderResult::Consumed
                }
            }
            _ => LeaderResult::Consumed,
        }
    }

    /// Render the leader as a centered overlay (used on the dashboard
    /// screen, which has no input field to anchor against).
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        render_leader(frame, area, &self.entries, &self.buffer);
    }

    /// Rows this leader needs when anchored: top rule + entries + footer.
    /// The chat layout reserves exactly this many rows below the input
    /// field, so the layout and the renderer share this helper.
    pub fn popup_height(&self) -> u16 {
        // +1 top rule, +1 footer; `max(1)` keeps the "no commands" row
        // visible for an empty scope.
        2 + self.entries.len().max(1) as u16
    }

    /// Render as a full-width top rule + list inside `rect` — the slot the
    /// chat layout placed directly below the input field. No side or bottom
    /// borders, same treatment as the `/` command popup.
    pub fn render_anchored(&self, frame: &mut Frame, rect: Rect) {
        let block = Block::default()
            .title(leader_title(&self.buffer))
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        render_leader_body(frame, inner, &self.entries, &self.buffer);
    }
}

/// Width of the leader-key column (longest key, at least 2).
fn leader_key_col_width(entries: &[LeaderEntry]) -> usize {
    entries
        .iter()
        .map(|e| UnicodeWidthStr::width(e.keys))
        .max()
        .unwrap_or(0)
        .max(2)
}

/// Title line for both render styles: the pending key buffer as a chip.
fn leader_title(buffer: &str) -> Line<'static> {
    if buffer.is_empty() {
        return Line::from(Span::styled(
            "── Leader ──",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(vec![
        Span::styled("── Leader ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" {} ", buffer),
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ),
        Span::styled(" ──", Style::default().add_modifier(Modifier::BOLD)),
    ])
}

/// Centered overlay inside a full box — used on the dashboard screen.
fn render_leader(frame: &mut Frame, area: Rect, entries: &[LeaderEntry], buffer: &str) {
    // Adaptive width: fit the longest entry (key column + name + description).
    let key_col_width = leader_key_col_width(entries);
    let content_width = entries
        .iter()
        .map(|e| {
            key_col_width
                + 2
                + UnicodeWidthStr::width(e.name)
                + 2
                + UnicodeWidthStr::width(e.description)
        })
        .max()
        .unwrap_or(0);
    let popup_width = (content_width as u16 + 2).clamp(36, area.width.saturating_sub(2).max(36));

    let list_height = entries.len() as u16 + 1; // rows + footer
    let popup_height = list_height + 2; // borders

    let x = area.x + area.width.saturating_sub(popup_width) / 2;
    let y = area.y + area.height.saturating_sub(popup_height) / 2;
    let popup_area = Rect::new(
        x,
        y.min(area.bottom().saturating_sub(popup_height)),
        popup_width,
        popup_height,
    );

    // Clear behind
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(leader_title(buffer))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    render_leader_body(frame, inner, entries, buffer);
}

/// Entries + footer, shared by the centered overlay and the anchored
/// (directly below the chat input field) rendering.
fn render_leader_body(frame: &mut Frame, area: Rect, entries: &[LeaderEntry], buffer: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let key_col_width = leader_key_col_width(entries);

    let mut lines: Vec<Line> = entries
        .iter()
        .map(|e| {
            let keys_width = UnicodeWidthStr::width(e.keys);
            let pad = " ".repeat(key_col_width.saturating_sub(keys_width));
            let keys_span = if e.keys == buffer {
                Span::styled(
                    format!("{}{}", e.keys, pad),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else if buffer.starts_with(e.keys) && !buffer.is_empty() {
                Span::styled(
                    format!("{}{}", e.keys, pad),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    format!("{}{}", e.keys, pad),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            };
            Line::from(vec![
                keys_span,
                Span::raw("  "),
                Span::styled(e.name, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(e.description, Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No commands available",
            Style::default().fg(Color::DarkGray),
        )));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[0]);

    let footer = if buffer.is_empty() {
        Line::from(Span::styled(
            " Esc to cancel",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(Span::styled(
            " Esc to cancel · waiting for next key",
            Style::default().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(footer), chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn esc_closes_leader() {
        let mut leader = Leader::new(CommandScope::Chat);
        assert_eq!(
            leader.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            LeaderResult::Closed
        );
    }

    #[test]
    fn single_char_hit_dispatches_and_closes() {
        let mut leader = Leader::new(CommandScope::Chat);
        assert_eq!(
            leader.handle_key(key('e')),
            LeaderResult::Action(LocalAction::ToggleExplorer)
        );
        // Buffer is reset after dispatch.
        assert_eq!(leader.buffer(), "");
    }

    #[test]
    fn multi_char_sequence_dispatches_after_completion() {
        let mut leader = Leader::new(CommandScope::Chat);
        // First `g` is a prefix of `gg` — wait, no dispatch yet.
        assert_eq!(leader.handle_key(key('g')), LeaderResult::Consumed);
        assert_eq!(leader.buffer(), "g");
        // Second `g` completes `gg` — dispatch scroll top.
        assert_eq!(
            leader.handle_key(key('g')),
            LeaderResult::Action(LocalAction::ScrollTop)
        );
        assert_eq!(leader.buffer(), "");
    }

    #[test]
    fn capital_g_dispatches_scroll_bottom() {
        let mut leader = Leader::new(CommandScope::Chat);
        assert_eq!(
            leader.handle_key(key('G')),
            LeaderResult::Action(LocalAction::ScrollBottom)
        );
    }

    #[test]
    fn invalid_after_partial_resets_buffer() {
        let mut leader = Leader::new(CommandScope::Chat);
        // `g` is a prefix — wait.
        assert_eq!(leader.handle_key(key('g')), LeaderResult::Consumed);
        assert_eq!(leader.buffer(), "g");
        // `x` is not a valid completion for any entry starting with `g`
        // and the buffer `gx` is not a prefix of any entry — reset.
        assert_eq!(leader.handle_key(key('x')), LeaderResult::Consumed);
        assert_eq!(leader.buffer(), "");
    }

    #[test]
    fn invalid_first_char_consumed_silently() {
        let mut leader = Leader::new(CommandScope::Chat);
        // `z` is a complete command (toggle zen), so this should dispatch.
        // Use a truly unknown char for the "consumed silently" check.
        assert_eq!(leader.handle_key(key('!')), LeaderResult::Consumed);
        assert_eq!(leader.buffer(), "");
    }

    #[test]
    fn ctrl_key_consumed_silently() {
        let mut leader = Leader::new(CommandScope::Chat);
        let k = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(leader.handle_key(k), LeaderResult::Consumed);
        assert_eq!(leader.buffer(), "");
    }

    #[test]
    fn dashboard_open_chat_uses_c_key() {
        let mut leader = Leader::new(CommandScope::Dashboard);
        assert_eq!(
            leader.handle_key(key('c')),
            LeaderResult::Action(LocalAction::OpenChat)
        );
    }
}
