use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use jyc_types::{CommandInfo, ModelInfo};

/// Strips a leading `/` from a command name for filter matching.
fn skip_slash(s: &str) -> &str {
    s.strip_prefix('/').unwrap_or(s)
}

/// Returns true if the filter text (with leading `/` stripped) indicates
/// model-selection mode (i.e., user typed "model " to select a model
/// instead of sending /model).
fn is_model_mode(filter: &str) -> bool {
    let f = skip_slash(filter);
    f.starts_with("model ")
}

/// Returns the model sub-filter (text after "model ").
fn model_subfilter(filter: &str) -> &str {
    let f = skip_slash(filter);
    f.strip_prefix("model ").unwrap_or("")
}

/// State for the `/` command popup in chat input.
#[derive(Debug)]
pub struct CommandPopupState {
    /// Current filter text typed by the user
    pub filter: String,
    /// Index of the selected item in the filtered list
    pub selected: usize,
}

impl CommandPopupState {
    pub fn new() -> Self {
        Self {
            filter: String::new(),
            selected: 0,
        }
    }

    /// Returns commands matching the current filter (case-insensitive).
    ///
    /// The filter is matched against the command name both with and without
    /// the leading `/`, so typing "model" matches "/model" without requiring
    /// the slash.
    pub fn filtered_commands<'a>(&self, all: &'a [CommandInfo]) -> Vec<&'a CommandInfo> {
        // In model mode, don't show commands
        if is_model_mode(&self.filter) {
            return vec![];
        }
        // Empty filter shows all commands
        if self.filter.is_empty() {
            return all.iter().collect();
        }
        let lower = self.filter.to_lowercase();
        all.iter()
            .filter(|cmd| {
                let name = cmd.name.to_lowercase();
                name.starts_with(&lower) || skip_slash(&name).starts_with(&lower)
            })
            .collect()
    }

    /// Returns models matching the model sub-filter (case-insensitive).
    pub fn filtered_models<'a>(&self, all: &'a [ModelInfo]) -> Vec<&'a ModelInfo> {
        let sub = model_subfilter(&self.filter);
        if sub.is_empty() {
            return all.iter().collect();
        }
        let lower = sub.to_lowercase();
        all.iter()
            .filter(|m| m.name.to_lowercase().contains(&lower))
            .collect()
    }
}

/// Handle a key event for the command popup.
///
/// Returns `Some(text)` if the user selected something via Enter:
/// - In command mode: `text` is the command name (e.g., `"/plan"`)
/// - In model mode: `text` is `"/model provider/model-id"`
///
/// Returns `Some("")` if Esc was pressed (close without action).
///
/// Returns `None` if the popup handled the key without selecting.
pub fn handle_popup_key(
    key: crossterm::event::KeyEvent,
    state: &mut CommandPopupState,
    commands: &[CommandInfo],
    models: &[ModelInfo],
) -> Option<String> {
    use crossterm::event::KeyCode;

    let model_mode = is_model_mode(&state.filter);

    // Clamp selection against current filtered list
    let count = if model_mode {
        state.filtered_models(models).len()
    } else {
        state.filtered_commands(commands).len()
    };
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
            if model_mode {
                let filtered = state.filtered_models(models);
                let selected = filtered.into_iter().nth(state.selected)?;
                return Some(format!("/model {}", selected.name));
            } else {
                let filtered = state.filtered_commands(commands);
                let selected = filtered.into_iter().nth(state.selected)?;
                return Some(selected.name.clone());
            }
        }
        KeyCode::Up => {
            if state.selected > 0 {
                state.selected -= 1;
            }
        }
        KeyCode::Down => {
            let count = if model_mode {
                state.filtered_models(models).len()
            } else {
                state.filtered_commands(commands).len()
            };
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
            // If we just transitioned into model mode or out of it, reset selection
            state.selected = 0;
        }
        _ => {}
    }
    None
}

/// Render the command/mode popup as a centered overlay.
pub fn render_command_popup(
    frame: &mut Frame,
    area: Rect,
    state: &CommandPopupState,
    commands: &[CommandInfo],
    models: &[ModelInfo],
) {
    let model_mode = is_model_mode(&state.filter);

    let (items, title) = if model_mode {
        let filtered = state.filtered_models(models);
        if filtered.is_empty() {
            (
                vec![Line::from(Span::styled(
                    "  (no models)",
                    Style::default().fg(Color::DarkGray),
                ))],
                " Models ",
            )
        } else {
            (render_model_list(&filtered, state.selected), " Models ")
        }
    } else if state.filter.is_empty() || !state.filtered_commands(commands).is_empty() {
        let filtered = state.filtered_commands(commands);
        (render_command_list(&filtered, state.selected), " Commands ")
    } else {
        // Filter doesn't match anything — show empty state
        (
            vec![Line::from(Span::styled(
                "  (no matches)",
                Style::default().fg(Color::DarkGray),
            ))],
            " Commands ",
        )
    };

    let list_height = items.len().clamp(1, 10) as u16;
    let popup_height = list_height + 3; // border(2) + filter(1)
    let popup_width = 52u16;

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
        .title(title)
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

    // "Loading..." — only before any data has arrived from the first poll
    let has_data = !commands.is_empty() || !models.is_empty();
    if !has_data {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Loading...",
                Style::default().fg(Color::DarkGray),
            ))),
            chunks[1],
        );
        return;
    }

    frame.render_widget(Paragraph::new(items).wrap(Wrap { trim: false }), chunks[1]);
}

fn render_command_list<'a>(filtered: &[&'a CommandInfo], selected: usize) -> Vec<Line<'a>> {
    let clamped = if filtered.is_empty() {
        0
    } else {
        selected.min(filtered.len() - 1)
    };

    filtered
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
        .collect()
}

fn render_model_list<'a>(filtered: &[&'a ModelInfo], selected: usize) -> Vec<Line<'a>> {
    let clamped = if filtered.is_empty() {
        0
    } else {
        selected.min(filtered.len() - 1)
    };

    filtered
        .iter()
        .enumerate()
        .map(|(i, model)| {
            let name = format!("  {}  ", model.name);
            if i == clamped {
                Line::from(vec![Span::styled(
                    name,
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )])
            } else {
                Line::from(vec![Span::raw(name)])
            }
        })
        .collect()
}
