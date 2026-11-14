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

    /// Rows the anchored popup needs at `width` cells: grid rows + footer.
    /// The chat layout reserves exactly this many rows below the input field,
    /// so the layout and the renderer share this helper — and share the grid
    /// math with [`leader_grid_lines`], which is what keeps the two from
    /// disagreeing about how tall the popup is. There is deliberately no top
    /// rule: the input area's bottom rule, one row above, is the boundary.
    pub fn popup_height(&self, width: usize) -> u16 {
        (1 + leader_grid_rows(&self.entries, width)) as u16
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

/// Width of one cell in the anchored grid: key column, a gap, the longest
/// command name, and a gutter so cells never run into each other.
fn leader_cell_width(entries: &[LeaderEntry]) -> usize {
    let name = entries
        .iter()
        .map(|e| UnicodeWidthStr::width(e.name))
        .max()
        .unwrap_or(0);
    leader_key_col_width(entries) + 2 + name + 3
}

/// Columns the anchored grid fits in `width` cells (at least one).
fn leader_grid_cols(entries: &[LeaderEntry], width: usize) -> usize {
    (width / leader_cell_width(entries)).max(1)
}

/// Rows the entries occupy in the anchored grid at `width` cells. An empty
/// scope still takes the one row that shows "No commands available".
fn leader_grid_rows(entries: &[LeaderEntry], width: usize) -> usize {
    entries
        .len()
        .div_ceil(leader_grid_cols(entries, width))
        .max(1)
}

/// A leader key as shown in the popup. The space key is invisible as a bare
/// space, so it gets the open-box glyph; dispatch still matches the real `" "`
/// (one cell wide either way, so the grid math is unaffected).
fn key_label(keys: &str) -> &str {
    if keys == " " { "␣" } else { keys }
}

/// The leader key of one entry, padded to `key_col` and styled by how well it
/// matches what the user has typed so far.
fn leader_key_span(entry: &LeaderEntry, buffer: &str, key_col: usize) -> Span<'static> {
    let pad = " ".repeat(key_col.saturating_sub(entry.keys.width()));
    let style = if entry.keys == buffer {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if !buffer.is_empty() && buffer.starts_with(entry.keys) {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    };
    Span::styled(format!("{}{}", key_label(entry.keys), pad), style)
}

/// One entry as `key  name  description`, for the centered popup.
fn leader_entry_line(entry: &LeaderEntry, buffer: &str, key_col: usize) -> Line<'static> {
    Line::from(vec![
        leader_key_span(entry, buffer, key_col),
        Span::raw("  "),
        Span::styled(entry.name, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(entry.description, Style::default().fg(Color::DarkGray)),
    ])
}

/// Title line for both render styles: the pending key buffer as a chip.
fn leader_title(buffer: &str) -> Line<'static> {
    if buffer.is_empty() {
        return Line::from(Span::styled(
            crate::cli::command_popup::rule_title("Leader"),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(vec![
        Span::styled("── Leader ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" {} ", key_label(buffer)),
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

    let key_col = leader_key_col_width(entries);
    let lines = entries
        .iter()
        .map(|e| leader_entry_line(e, buffer, key_col))
        .collect();
    render_leader_body(frame, inner, lines, buffer);
}

/// Entries + footer, shared by the centered overlay (which passes one
/// descriptive row per entry) and the anchored popup (which passes the grid).
fn render_leader_body(frame: &mut Frame, area: Rect, mut lines: Vec<Line<'static>>, buffer: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No commands available",
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[0]);
    let text = if buffer.is_empty() {
        " Esc to cancel"
    } else {
        " Esc to cancel · waiting for next key"
    };
    let footer = Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)));
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
            leader.handle_key(key('x')),
            LeaderResult::Action(LocalAction::ClearInput)
        );
        // Buffer is reset after dispatch.
        assert_eq!(leader.buffer(), "");
    }

    #[test]
    fn invalid_after_dispatch_resets_buffer() {
        let mut leader = Leader::new(CommandScope::Chat);
        // `t` dispatches immediately.
        assert_eq!(
            leader.handle_key(key('t')),
            LeaderResult::Action(LocalAction::ToggleThinking)
        );
        assert_eq!(leader.buffer(), "");
        // The dispatch already reset the buffer; the next key starts fresh.
        assert_eq!(
            leader.handle_key(key('t')),
            LeaderResult::Action(LocalAction::ToggleThinking)
        );
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
    fn space_dispatches_toggle_mode() {
        let mut leader = Leader::new(CommandScope::Chat);
        assert_eq!(
            leader.handle_key(key(' ')),
            LeaderResult::Action(LocalAction::ToggleMode)
        );
    }

    /// The space key has to be visible in the popup: a bare space renders as
    /// an empty key column, and as a blank chip in the title while the buffer

    #[test]
    fn dashboard_open_chat_uses_c_key() {
        let mut leader = Leader::new(CommandScope::Dashboard);
        assert_eq!(
            leader.handle_key(key('c')),
            LeaderResult::Action(LocalAction::OpenChat)
        );
    }

    /// One cell per row when the terminal is too narrow for two, and an empty
    /// scope still reserves the row that says so.
    #[test]
    fn anchored_grid_degrades_gracefully() {
        let leader = Leader::new(CommandScope::Chat);
        assert_eq!(leader_grid_cols(&leader.entries, 20), 1);
        assert_eq!(
            leader_grid_rows(&leader.entries, 20),
            leader.entries.len(),
            "the degenerate case is one row per entry"
        );
        let empty: Vec<LeaderEntry> = Vec::new();
        assert_eq!(leader_grid_rows(&empty, 80), 1, "the placeholder row");
    }
}
