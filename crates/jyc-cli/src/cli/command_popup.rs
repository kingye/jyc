use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
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
    /// A popup-owned key was handled (Up/Down navigation, a Tab with
    /// nothing to complete). The popup stays open.
    None,
    /// Not a popup key — text editing, cursor moves, global shortcuts.
    /// The caller feeds it to the chat input field, which IS the popup's
    /// filter (the popup has no input box of its own).
    PassThrough,
    /// Enter pressed — send the command immediately.
    Send(String),
    /// Tab on an incomplete filter — write the completion into the chat
    /// input field and keep the popup open (the filter follows on sync).
    Complete(String),
    /// Tab pressed on a complete filter — copy the command to the chat
    /// input line so the user can add arguments before sending.
    CopyToInput(String),
    /// Esc pressed — close the popup.
    Close,
}

/// State for the `/` command popup in chat input.
#[derive(Debug)]
pub struct CommandPopupState {
    /// Current filter text. The popup has no input box of its own — the
    /// caller mirrors the chat input field into this before each dispatch
    /// (see `sync_command_popup` in the chat dashboard).
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
            // model name, close and leave it in the input line (symmetric
            // with command mode). Otherwise hand the selected model to the
            // caller to write into the chat input field.
            if model_mode {
                let sub = model_subfilter(&state.filter);
                if !sub.is_empty()
                    && let Some(model) = models.iter().find(|m| m.name == sub)
                {
                    return PopupAction::CopyToInput(format!("/model {}", model.name));
                }
                match state
                    .filtered_models(models)
                    .into_iter()
                    .nth(state.selected)
                {
                    Some(model) => PopupAction::Complete(format!("/model {}", model.name)),
                    None => PopupAction::None,
                }
            } else {
                match state
                    .filtered_commands(commands)
                    .into_iter()
                    .nth(state.selected)
                {
                    Some(cmd) if is_filter_complete(&state.filter, commands) => {
                        PopupAction::CopyToInput(cmd.name.clone())
                    }
                    Some(cmd) => PopupAction::Complete(cmd.name.clone()),
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
                    // Nothing selected: let the editor send what was typed.
                    None => PopupAction::PassThrough,
                }
            } else {
                match state
                    .filtered_commands(commands)
                    .into_iter()
                    .nth(state.selected)
                {
                    Some(cmd) => PopupAction::Send(cmd.name.clone()),
                    None => PopupAction::PassThrough,
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
        // Everything else — text, editing, cursor moves — belongs to the
        // chat input field, whose text is the filter.
        _ => PopupAction::PassThrough,
    }
}

/// Rows the popup needs below the chat input field: one top rule plus the
/// (clamped) list. The chat layout reserves exactly this many rows, so the
/// renderer and the layout must agree through this single helper.
pub fn popup_height(
    state: &CommandPopupState,
    commands: &[CommandInfo],
    models: &[ModelInfo],
) -> u16 {
    let rows = if model_mode_active(&state.filter, models) {
        state.filtered_models(models).len()
    } else {
        state.filtered_commands(commands).len()
    };
    1 + rows.clamp(1, 10) as u16
}

/// Render the command/mode popup as a full-width top rule plus the list,
/// inside `area` — the slot the caller reserved directly below the input
/// field. No side or bottom borders: the rule is the only chrome, so it
/// stretches across the whole pane.
pub fn render_command_popup(
    frame: &mut Frame,
    area: Rect,
    state: &CommandPopupState,
    commands: &[CommandInfo],
    models: &[ModelInfo],
) {
    let model_mode = model_mode_active(&state.filter, models);
    let block = Block::default()
        .title(if model_mode { " Models " } else { " Commands " })
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // "Loading..." — only before any data has arrived from the first poll
    if commands.is_empty() && models.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Loading...",
                Style::default().fg(Color::DarkGray),
            ))),
            inner,
        );
        return;
    }

    let items = if model_mode {
        let filtered = state.filtered_models(models);
        if filtered.is_empty() {
            vec![Line::from(Span::styled(
                "  (no models)",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            render_model_list(&filtered, state.selected, inner.width)
        }
    } else {
        let filtered = state.filtered_commands(commands);
        if filtered.is_empty() {
            vec![Line::from(Span::styled(
                "  (no matches)",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            render_command_list(&filtered, state.selected, inner.width)
        }
    };

    frame.render_widget(Paragraph::new(items).wrap(Wrap { trim: false }), inner);
}

/// List rows. The selected row is padded to `width` so its highlight bar
/// spans the full popup width — there are no side borders to end it at.
fn render_command_list<'a>(
    filtered: &[&'a CommandInfo],
    selected: usize,
    width: u16,
) -> Vec<Line<'a>> {
    let clamped = if filtered.is_empty() {
        0
    } else {
        selected.min(filtered.len() - 1)
    };

    filtered
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let name = format!("  {}  ", cmd.name);
            let desc = cmd.description.as_str();
            if i != clamped {
                return Line::from(vec![
                    Span::raw(name),
                    Span::styled(desc, Style::default().fg(Color::DarkGray)),
                ]);
            }
            let bar = Style::default().fg(Color::Black).bg(Color::Cyan);
            let used = UnicodeWidthStr::width(name.as_str()) + 1 + UnicodeWidthStr::width(desc);
            Line::from(vec![
                Span::styled(name, bar.add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {}", desc), bar),
                Span::styled(" ".repeat((width as usize).saturating_sub(used)), bar),
            ])
        })
        .collect()
}

/// Model rows, with the same full-width highlight bar as
/// [`render_command_list`].
fn render_model_list<'a>(filtered: &[&'a ModelInfo], selected: usize, width: u16) -> Vec<Line<'a>> {
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
            if i != clamped {
                return Line::from(vec![Span::raw(name)]);
            }
            let bar = Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            let used = UnicodeWidthStr::width(name.as_str());
            Line::from(vec![
                Span::styled(name, bar),
                Span::styled(" ".repeat((width as usize).saturating_sub(used)), bar),
            ])
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
        // must not produce a "/model ..." send action — the key falls
        // through so the chat input field sends what was typed instead.
        let mut state = CommandPopupState::new();
        state.filter = "model x".to_string();
        let cmds = vec![make_cmd("toggle zen")];
        assert_eq!(
            handle_popup_key(key(KeyCode::Enter), &mut state, &cmds, &[]),
            PopupAction::PassThrough
        );
    }

    #[test]
    fn tab_hands_completion_to_the_caller() {
        let mut state = CommandPopupState::new();
        state.filter = "pl".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Tab), &mut state, &commands, &[]);
        assert_eq!(result, PopupAction::Complete("/plan".to_string()));
        // The popup no longer owns the filter text: the caller writes the
        // completion into the chat input field, and the filter follows.
        assert_eq!(state.filter, "pl");
    }

    #[test]
    fn tab_completes_selected_command_not_first() {
        let mut state = CommandPopupState::new();
        state.filter = String::new(); // All commands shown
        state.selected = 1; // Second item
        let commands = vec![make_cmd("/plan"), make_cmd("/model")];

        assert_eq!(
            handle_popup_key(key(KeyCode::Tab), &mut state, &commands, &[]),
            PopupAction::Complete("/model".to_string())
        );
    }

    #[test]
    fn tab_completes_model_in_model_mode() {
        let mut state = CommandPopupState::new();
        state.filter = "model ".to_string();
        state.selected = 0;
        let models = vec![make_model("gpt-4"), make_model("claude-3")];

        assert_eq!(
            handle_popup_key(key(KeyCode::Tab), &mut state, &[], &models),
            PopupAction::Complete("/model gpt-4".to_string())
        );
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
    fn text_keys_belong_to_the_chat_input_field() {
        // The popup has no input box of its own: text, editing and cursor
        // keys pass through to the editor, which is the filter's single
        // source of truth.
        let mut state = CommandPopupState::new();
        state.filter = "/pl".to_string();
        let commands = vec![make_cmd("/plan")];

        for code in [KeyCode::Char('a'), KeyCode::Backspace, KeyCode::Left] {
            assert_eq!(
                handle_popup_key(key(code), &mut state, &commands, &[]),
                PopupAction::PassThrough,
                "{code:?} must reach the chat input field"
            );
        }
        assert_eq!(state.filter, "/pl", "pass-through must not edit it");
    }

    #[test]
    fn popup_height_is_top_rule_plus_clamped_list() {
        let commands: Vec<CommandInfo> = (0..20).map(|i| make_cmd(&format!("/c{i}"))).collect();
        // 1 top rule + at most 10 list rows.
        assert_eq!(popup_height(&CommandPopupState::new(), &commands, &[]), 11);
        // A filter matching nothing still reserves one row.
        let mut empty = CommandPopupState::new();
        empty.filter = "/zzz".to_string();
        assert_eq!(popup_height(&empty, &commands, &[]), 2);
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
    fn enter_falls_through_when_nothing_matches() {
        let mut state = CommandPopupState::new();
        state.filter = "zzz".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Enter), &mut state, &commands, &[]);
        // No command to send: the editor gets the key and sends the text.
        assert_eq!(result, PopupAction::PassThrough);
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
        // Simulates: "/think" in the chat input field + Tab → the caller
        // writes "/thinking" back into the field (popup stays open, filter
        // follows on the next sync) → Tab again → CopyToInput (closes).
        let mut state = CommandPopupState::new();
        state.filter = "/think".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/thinking")];

        let first = handle_popup_key(key(KeyCode::Tab), &mut state, &commands, &[]);
        assert_eq!(first, PopupAction::Complete("/thinking".to_string()));

        state.filter = "/thinking".to_string(); // what the editor now holds
        let second = handle_popup_key(key(KeyCode::Tab), &mut state, &commands, &[]);
        assert_eq!(second, PopupAction::CopyToInput("/thinking".to_string()));
    }

    #[test]
    fn tab_then_tab_copies_to_input_in_model_mode() {
        // Same two-step flow for models: "/model gpt" + Tab completes the
        // name into the input field, Tab again closes with it in place.
        let mut state = CommandPopupState::new();
        state.filter = "/model gpt".to_string();
        state.selected = 0;
        let models = vec![make_model("gpt-4"), make_model("claude-3")];

        let first = handle_popup_key(key(KeyCode::Tab), &mut state, &[], &models);
        assert_eq!(first, PopupAction::Complete("/model gpt-4".to_string()));

        state.filter = "/model gpt-4".to_string();
        let second = handle_popup_key(key(KeyCode::Tab), &mut state, &[], &models);
        assert_eq!(second, PopupAction::CopyToInput("/model gpt-4".to_string()));
    }
}
