use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use jyc_types::{CommandInfo, ModelInfo};
use unicode_width::UnicodeWidthStr;

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

/// True when model-selection mode is active: the filter starts with
/// "model " AND the caller actually has models to select from. The
/// command palette passes an empty models slice, so "model " typed into
/// it stays a plain (non-matching) command filter.
fn model_mode_active(filter: &str, models: &[ModelInfo]) -> bool {
    is_model_mode(filter) && !models.is_empty()
}

/// True when filter exactly matches a registered command name (with or
/// without leading `/`). Used by Tab to pick auto-complete vs. copy.
fn is_filter_complete(filter: &str, commands: &[CommandInfo]) -> bool {
    if filter.is_empty() {
        return false;
    }
    commands
        .iter()
        .any(|cmd| cmd.name == filter || skip_slash(&cmd.name) == skip_slash(filter))
}

/// Action the popup wants the caller to perform after handling a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupAction {
    /// Key was handled but no action is requested (Up/Down/Backspace/Char,
    /// or Tab on an incomplete filter). Popup stays open.
    None,
    /// Enter pressed — send the command immediately.
    Send(String),
    /// Tab pressed on a complete filter — copy the command to the chat
    /// input line so the user can add arguments before sending.
    CopyToInput(String),
    /// Esc pressed — close the popup.
    Close,
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
/// Returns the action the caller should perform. See [`PopupAction`].
pub fn handle_popup_key(
    key: crossterm::event::KeyEvent,
    state: &mut CommandPopupState,
    commands: &[CommandInfo],
    models: &[ModelInfo],
) -> PopupAction {
    use crossterm::event::KeyCode;

    let model_mode = model_mode_active(&state.filter, models);

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
        KeyCode::Esc => PopupAction::Close,
        KeyCode::Tab => {
            // Model mode: if the sub-filter already exactly matches a real
            // model name, copy to the input line (symmetric with command
            // mode). Otherwise fill the filter with the selected model.
            if model_mode {
                let sub = model_subfilter(&state.filter);
                if !sub.is_empty()
                    && let Some(model) = models.iter().find(|m| m.name == sub)
                {
                    return PopupAction::CopyToInput(format!("/model {}", model.name));
                }
                if let Some(model) = state
                    .filtered_models(models)
                    .into_iter()
                    .nth(state.selected)
                {
                    state.filter = format!("/model {}", model.name);
                    state.selected = 0;
                }
                PopupAction::None
            } else {
                match state
                    .filtered_commands(commands)
                    .into_iter()
                    .nth(state.selected)
                {
                    Some(cmd) if is_filter_complete(&state.filter, commands) => {
                        PopupAction::CopyToInput(cmd.name.clone())
                    }
                    Some(cmd) => {
                        state.filter = cmd.name.clone();
                        state.selected = 0;
                        PopupAction::None
                    }
                    None => PopupAction::None,
                }
            }
        }
        KeyCode::Enter => {
            if model_mode {
                match state
                    .filtered_models(models)
                    .into_iter()
                    .nth(state.selected)
                {
                    Some(m) => PopupAction::Send(format!("/model {}", m.name)),
                    None => PopupAction::None,
                }
            } else {
                match state
                    .filtered_commands(commands)
                    .into_iter()
                    .nth(state.selected)
                {
                    Some(cmd) => PopupAction::Send(cmd.name.clone()),
                    None => PopupAction::None,
                }
            }
        }
        KeyCode::Up => {
            if state.selected > 0 {
                state.selected -= 1;
            }
            PopupAction::None
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
            PopupAction::None
        }
        KeyCode::Backspace => {
            state.filter.pop();
            state.selected = 0;
            PopupAction::None
        }
        KeyCode::Char(c) if !c.is_control() => {
            state.filter.push(c);
            // If we just transitioned into model mode or out of it, reset selection
            state.selected = 0;
            PopupAction::None
        }
        _ => PopupAction::None,
    }
}

/// Render the command/mode popup as a centered overlay.
pub fn render_command_popup(
    frame: &mut Frame,
    area: Rect,
    state: &CommandPopupState,
    commands: &[CommandInfo],
    models: &[ModelInfo],
) {
    render_popup(frame, area, state, commands, models, " Commands ");
}

/// Shared renderer for the command popup and the command palette.
fn render_popup(
    frame: &mut Frame,
    area: Rect,
    state: &CommandPopupState,
    commands: &[CommandInfo],
    models: &[ModelInfo],
    cmd_title: &str,
) {
    let model_mode = model_mode_active(&state.filter, models);

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
        (render_command_list(&filtered, state.selected), cmd_title)
    } else {
        // Filter doesn't match anything — show empty state
        (
            vec![Line::from(Span::styled(
                "  (no matches)",
                Style::default().fg(Color::DarkGray),
            ))],
            cmd_title,
        )
    };

    let list_height = items.len().clamp(1, 10) as u16;
    let popup_height = list_height + 3; // border(2) + filter(1)

    // Adaptive width: fit the longest item text (name + description) so
    // wide content isn't truncated, clamped to the available area.
    // Width is computed over *all* entries (not the filtered subset) so
    // the popup doesn't resize while typing.
    let content_width = if model_mode {
        models
            .iter()
            .map(|m| UnicodeWidthStr::width(m.name.as_str()) + 4)
            .max()
            .unwrap_or(0)
    } else {
        commands
            .iter()
            .map(|c| {
                UnicodeWidthStr::width(c.name.as_str())
                    + UnicodeWidthStr::width(c.description.as_str())
                    + 6
            })
            .max()
            .unwrap_or(0)
    };
    let popup_width = (content_width as u16 + 2) // + borders
        .max(UnicodeWidthStr::width(title) as u16 + 4)
        .clamp(32, area.width.saturating_sub(2).max(32));

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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn make_cmd(name: &str) -> CommandInfo {
        CommandInfo {
            name: name.to_string(),
            description: format!("{name} description"),
            ..Default::default()
        }
    }

    fn make_model(name: &str) -> ModelInfo {
        ModelInfo {
            name: name.to_string(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn model_mode_requires_models() {
        // With models present, "model x" activates model mode.
        assert!(model_mode_active("model x", &[make_model("m1")]));
        // Without models (e.g. the command palette), "model x" stays a
        // plain command filter instead of showing an empty model list.
        assert!(!model_mode_active("model x", &[]));
        assert!(!model_mode_active("zen", &[make_model("m1")]));

        // Palette scenario: Enter with a "model " filter and no models
        // must not produce a "/model ..." send action.
        let mut state = CommandPopupState::new();
        state.filter = "model x".to_string();
        let cmds = vec![make_cmd("toggle zen")];
        assert_eq!(
            handle_popup_key(key(KeyCode::Enter), &mut state, &cmds, &[]),
            PopupAction::None
        );
    }

    #[test]
    fn tab_auto_completes_command_name_into_filter() {
        let mut state = CommandPopupState::new();
        state.filter = "pl".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Tab), &mut state, &commands, &[]);
        assert_eq!(result, PopupAction::None, "Tab should not close popup");
        assert_eq!(state.filter, "/plan");
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn tab_auto_completes_selected_command_not_first() {
        let mut state = CommandPopupState::new();
        state.filter = String::new(); // All commands shown
        state.selected = 1; // Second item
        let commands = vec![make_cmd("/plan"), make_cmd("/model")];

        let result = handle_popup_key(key(KeyCode::Tab), &mut state, &commands, &[]);
        assert_eq!(result, PopupAction::None);
        assert_eq!(state.filter, "/model");
    }

    #[test]
    fn tab_auto_completes_model_in_model_mode() {
        let mut state = CommandPopupState::new();
        state.filter = "model ".to_string();
        state.selected = 0;
        let models = vec![make_model("gpt-4"), make_model("claude-3")];

        let result = handle_popup_key(key(KeyCode::Tab), &mut state, &[], &models);
        assert_eq!(result, PopupAction::None);
        assert_eq!(state.filter, "/model gpt-4");
    }

    #[test]
    fn tab_no_op_when_no_commands_match() {
        let mut state = CommandPopupState::new();
        state.filter = "zzz".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Tab), &mut state, &commands, &[]);
        assert_eq!(result, PopupAction::None);
        // No matching command, so filter unchanged
        assert!(!state.filter.contains("/plan"));
    }

    #[test]
    fn enter_sends_command_in_command_mode() {
        let mut state = CommandPopupState::new();
        state.filter = "pl".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Enter), &mut state, &commands, &[]);
        assert_eq!(result, PopupAction::Send("/plan".to_string()));
    }

    #[test]
    fn enter_sends_model_in_model_mode() {
        let mut state = CommandPopupState::new();
        state.filter = "model ".to_string();
        state.selected = 0;
        let models = vec![make_model("gpt-4")];

        let result = handle_popup_key(key(KeyCode::Enter), &mut state, &[], &models);
        assert_eq!(result, PopupAction::Send("/model gpt-4".to_string()));
    }

    #[test]
    fn enter_no_op_when_no_command_selected() {
        let mut state = CommandPopupState::new();
        state.filter = "zzz".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Enter), &mut state, &commands, &[]);
        assert_eq!(result, PopupAction::None);
    }

    #[test]
    fn esc_closes_popup() {
        let mut state = CommandPopupState::new();
        let commands = vec![make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Esc), &mut state, &commands, &[]);
        assert_eq!(result, PopupAction::Close);
    }

    #[test]
    fn tab_copies_to_input_when_filter_complete_with_slash() {
        let mut state = CommandPopupState::new();
        state.filter = "/thinking".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/thinking"), make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Tab), &mut state, &commands, &[]);
        assert_eq!(result, PopupAction::CopyToInput("/thinking".to_string()));
        // Filter is not mutated on the CopyToInput path
        assert_eq!(state.filter, "/thinking");
    }

    #[test]
    fn tab_then_tab_copies_to_input() {
        // Simulates: type "think" + Tab → "/thinking" in filter,
        // then Tab again → CopyToInput.
        let mut state = CommandPopupState::new();
        state.filter = "think".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/thinking")];

        // First Tab fills the filter
        let first = handle_popup_key(key(KeyCode::Tab), &mut state, &commands, &[]);
        assert_eq!(first, PopupAction::None);
        assert_eq!(state.filter, "/thinking");

        // Second Tab on the now-complete filter copies to input line
        let second = handle_popup_key(key(KeyCode::Tab), &mut state, &commands, &[]);
        assert_eq!(second, PopupAction::CopyToInput("/thinking".to_string()));
    }

    #[test]
    fn tab_then_tab_copies_to_input_in_model_mode() {
        // Simulates: type "model gpt" + Tab → "/model gpt-4" in filter,
        // then Tab again → CopyToInput, mirroring the command-mode flow.
        let mut state = CommandPopupState::new();
        state.filter = "model gpt".to_string();
        state.selected = 0;
        let models = vec![make_model("gpt-4"), make_model("claude-3")];

        let first = handle_popup_key(key(KeyCode::Tab), &mut state, &[], &models);
        assert_eq!(first, PopupAction::None);
        assert_eq!(state.filter, "/model gpt-4");

        let second = handle_popup_key(key(KeyCode::Tab), &mut state, &[], &models);
        assert_eq!(second, PopupAction::CopyToInput("/model gpt-4".to_string()));
    }
}
