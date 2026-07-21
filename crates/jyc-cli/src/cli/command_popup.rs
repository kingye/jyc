use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use jyc_types::CommandInfo;

/// State for the `/` command popup in chat input.
#[derive(Debug)]
pub struct CommandPopupState {
    /// Current filter text typed by the user
    pub filter: String,
    /// Index of the selected command in the filtered list
    pub selected: usize,
}

impl CommandPopupState {
    pub fn new() -> Self {
        Self {
            filter: String::new(),
            selected: 0,
        }
    }

    /// Returns commands matching the current filter (case-insensitive prefix match).
    pub fn filtered_commands<'a>(&self, all: &'a [CommandInfo]) -> Vec<&'a CommandInfo> {
        if self.filter.is_empty() {
            return all.iter().collect();
        }
        let lower = self.filter.to_lowercase();
        all.iter()
            .filter(|cmd| cmd.name.to_lowercase().starts_with(&lower))
            .collect()
    }
}

/// Handle a key event for the command popup.
///
/// Returns `Some(name)` if the user selected a command via Enter,
/// or `None` if the popup should stay open (`Esc` returns `Some("")`).
pub fn handle_popup_key(
    key: crossterm::event::KeyEvent,
    state: &mut CommandPopupState,
    commands: &[CommandInfo],
) -> Option<String> {
    use crossterm::event::KeyCode;

    // Clamp selection against current filtered count
    let count = state.filtered_commands(commands).len();
    if count == 0 {
        state.selected = 0;
    } else if state.selected >= count {
        state.selected = count - 1;
    }

    match key.code {
        KeyCode::Esc => {
            // Close without action — signal with empty string
            return Some(String::new());
        }
        KeyCode::Enter => {
            let filtered = state.filtered_commands(commands);
            let selected = filtered.into_iter().nth(state.selected)?;
            return Some(selected.name.clone());
        }
        KeyCode::Up => {
            if state.selected > 0 {
                state.selected -= 1;
            }
        }
        KeyCode::Down => {
            let count = state.filtered_commands(commands).len();
            if count > 0 && state.selected + 1 < count {
                state.selected += 1;
            }
        }
        KeyCode::Backspace => {
            state.filter.pop();
            state.selected = 0;
        }
        KeyCode::Char(c) if !c.is_control() => {
            state.filter.push(c);
            state.selected = 0;
        }
        _ => {}
    }
    None
}

/// Render the command popup as a centered overlay.
pub fn render_command_popup(
    frame: &mut Frame,
    area: Rect,
    state: &CommandPopupState,
    all: &[CommandInfo],
) {
    let filtered = state.filtered_commands(all);
    let list_height = filtered.len().clamp(1, 10) as u16;
    let popup_height = list_height + 3; // border(2) + filter(1)
    let popup_width = 42u16;

    // Center the popup
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

    // Main block
    let block = Block::default()
        .title(" Commands ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    // Inner layout: filter input (1 line) + list (remaining)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    // Filter input line
    let cursor_visible = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() / 500 % 2 == 0)
        .unwrap_or(true);

    let filter_display = if state.filter.is_empty() {
        if cursor_visible {
            Span::styled("▌", Style::default().add_modifier(Modifier::SLOW_BLINK))
        } else {
            Span::raw(" ")
        }
    } else {
        let cursor_char = if cursor_visible { "▌" } else { " " };
        Span::raw(format!("{}{}", state.filter, cursor_char))
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Yellow)),
            filter_display,
        ]))
        .style(Style::default()),
        chunks[0],
    );

    // Command list
    if all.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Loading...",
                Style::default().fg(Color::DarkGray),
            ))),
            chunks[1],
        );
        return;
    }

    if filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  (no matches)",
                Style::default().fg(Color::DarkGray),
            ))),
            chunks[1],
        );
        return;
    }

    // Clamp selection
    let mut clamped = state.selected;
    if !filtered.is_empty() && clamped >= filtered.len() {
        clamped = filtered.len() - 1;
    }

    let lines: Vec<Line> = filtered
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let padded = format!("  {}  ", cmd.name);
            let desc = cmd.description.as_str();
            if i == clamped {
                Line::from(vec![
                    Span::styled(
                        padded,
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {}", desc),
                        Style::default().fg(Color::Black).bg(Color::Cyan),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::raw(padded),
                    Span::styled(desc, Style::default().fg(Color::DarkGray)),
                ])
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[1]);
}
