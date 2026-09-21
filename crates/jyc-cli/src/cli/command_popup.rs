use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use jyc_types::{CommandArg, CommandInfo};
use unicode_width::UnicodeWidthStr;

/// Strips a leading `/` from a command name for filter matching.
fn skip_slash(s: &str) -> &str {
    s.strip_prefix('/').unwrap_or(s)
}

/// The text a popup's top rule carries, styled by the caller. Shared with
/// the leader popup so the two rules always look the same.
pub(crate) fn rule_title(name: &str) -> String {
    format!("── {name} ──")
}

/// One popup row, at whatever level it sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupItem {
    /// Value this row adds to the field: a command name (`/plan`) or an
    /// argument value (`hide`, `deepseek/deepseek-chat`).
    pub text: String,
    pub description: String,
    /// Completing this row opens another level, so Tab appends a space.
    pub has_children: bool,
}

/// The popup level the field text currently selects.
#[derive(Debug)]
pub struct PopupLevel {
    /// Top-rule name: "Commands" at the root, the command path
    /// (`/model`, `/skill on`) deeper.
    pub title: String,
    /// Field text this level's values are appended to — "" at the root,
    /// "/model " one level down.
    pub prefix: String,
    /// Values at this level, already narrowed by the partial token.
    pub items: Vec<PopupItem>,
    /// The canonical line for the partial when it already equals one of this
    /// level's values — what Tab leaves in the field before closing.
    pub complete: Option<String>,
}

impl PopupLevel {
    /// The full field text `item` completes to.
    fn line(&self, item: &PopupItem) -> String {
        format!("{}{}", self.prefix, item.text)
    }
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
}

impl Default for CommandPopupState {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolves the field text to the popup level it points at.
///
/// The text *is* the path: `mo` is the root filtered by "mo", `/model de` is
/// `/model`'s first argument level filtered by "de", `/skill on po` the
/// second. Splitting on `' '` (not whitespace) keeps a trailing space meaning
/// "one level down, nothing typed yet" instead of collapsing back up.
///
/// Returns `None` when the path leads to something with nothing to complete:
/// a command whose arguments are free text (`/grant <path>`), an argument
/// position that declares no values, or a first token that isn't a command at
/// all (`/nope x`). The caller closes the popup and the field sends what was
/// typed. A typo *without* a space still shows `(no matches)` at the root.
pub fn resolve_level(filter: &str, commands: &[CommandInfo]) -> Option<PopupLevel> {
    let tokens: Vec<&str> = filter.split(' ').collect();
    let partial = tokens.last().copied().unwrap_or("");
    // Tokens before the one being typed: ["/model"] for "/model de".
    let path = &tokens[..tokens.len() - 1];

    if path.is_empty() {
        return Some(PopupLevel {
            title: "Commands".to_string(),
            prefix: String::new(),
            items: command_rows(commands, partial),
            complete: commands
                .iter()
                .find(|c| !partial.is_empty() && matches_command(&c.name, partial))
                .map(|c| c.name.clone()),
        });
    }

    let cmd = commands
        .iter()
        .find(|c| matches_command(&c.name, tokens[0]))?;
    let mut prefix = format!("{} ", cmd.name);
    let mut values = &cmd.args;
    for token in &path[1..] {
        let arg = values.iter().find(|a| a.value == *token)?;
        prefix.push_str(&arg.value);
        prefix.push(' ');
        values = &arg.args;
    }
    if values.is_empty() {
        return None;
    }
    Some(PopupLevel {
        title: prefix.trim_end().to_string(),
        items: arg_rows(values, partial),
        complete: values
            .iter()
            .find(|a| a.value == partial)
            .map(|a| format!("{prefix}{}", a.value)),
        prefix,
    })
}

/// True when `token` names `command` either with or without the slash.
fn matches_command(command: &str, token: &str) -> bool {
    !token.is_empty() && (command == token || skip_slash(command) == skip_slash(token))
}

/// Root rows: commands whose name starts with the partial, with or without
/// the leading slash (so "model" picks "/model").
fn command_rows(commands: &[CommandInfo], partial: &str) -> Vec<PopupItem> {
    let lower = partial.to_lowercase();
    commands
        .iter()
        .filter(|c| {
            lower.is_empty()
                || c.name.to_lowercase().starts_with(&lower)
                || skip_slash(&c.name).to_lowercase().starts_with(&lower)
        })
        .map(|c| PopupItem {
            text: c.name.clone(),
            description: c.description.clone(),
            has_children: !c.args.is_empty(),
        })
        .collect()
}

/// Argument rows: values *containing* the partial — an id like
/// "anthropic/claude-sonnet-4" should answer to "sonnet".
fn arg_rows(values: &[CommandArg], partial: &str) -> Vec<PopupItem> {
    let lower = partial.to_lowercase();
    values
        .iter()
        .filter(|a| lower.is_empty() || a.value.to_lowercase().contains(&lower))
        .map(|a| PopupItem {
            text: a.value.clone(),
            description: a.description.clone(),
            has_children: !a.args.is_empty(),
        })
        .collect()
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

/// Handle a key event for the command popup.
///
/// Returns the action the caller should perform. See [`PopupAction`].
pub fn handle_popup_key(
    key: crossterm::event::KeyEvent,
    state: &mut CommandPopupState,
    commands: &[CommandInfo],
) -> PopupAction {
    use crossterm::event::KeyCode;

    // The field text points at free text rather than a completable value —
    // the caller closes the popup in `sync_command_popup`; until then every
    // key belongs to the editor.
    let Some(level) = resolve_level(&state.filter, commands) else {
        return PopupAction::PassThrough;
    };

    // Clamp selection against the current rows.
    let count = level.items.len();
    if count == 0 {
        state.selected = 0;
    } else if state.selected >= count {
        state.selected = count - 1;
    }

    match key.code {
        KeyCode::Esc => PopupAction::Close,
        KeyCode::Tab => {
            let row = level.items.get(state.selected);
            if let Some(row) = row.filter(|r| r.has_children) {
                // A row with a deeper level steps into it: the trailing
                // space is what opens that level, so the user never types it.
                return PopupAction::Complete(format!("{}{} ", level.prefix, row.text));
            }
            // A partial that already equals a value at this level: leave it
            // in the input line and close (the second Tab of the two-step
            // completion, at any level).
            if let Some(line) = level.complete {
                PopupAction::CopyToInput(line)
            } else {
                match row {
                    Some(row) => PopupAction::Complete(level.line(row)),
                    None => PopupAction::None,
                }
            }
        }
        KeyCode::Enter => match level.items.get(state.selected) {
            // The root sends the command itself (`/model` lists models);
            // deeper levels send the whole line (`/model <id>`).
            Some(item) => PopupAction::Send(level.line(item)),
            // Nothing selected: let the editor send what was typed.
            None => PopupAction::PassThrough,
        },
        KeyCode::Up => {
            if state.selected > 0 {
                state.selected -= 1;
            }
            PopupAction::None
        }
        KeyCode::Down => {
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
pub fn popup_height(state: &CommandPopupState, commands: &[CommandInfo]) -> u16 {
    let rows = resolve_level(&state.filter, commands).map_or(0, |l| l.items.len());
    1 + rows.clamp(1, 10) as u16
}

/// Render the command popup as a full-width top rule plus the list, inside
/// `area` — the slot the caller reserved directly below the input field. No
/// side or bottom borders: the rule is the only chrome, so it stretches
/// across the whole pane.
pub fn render_command_popup(
    frame: &mut Frame,
    area: Rect,
    state: &CommandPopupState,
    commands: &[CommandInfo],
) {
    let title = resolve_level(&state.filter, commands)
        .map(|l| l.title)
        .unwrap_or_else(|| "Commands".to_string());
    let block = Block::default()
        .title(Line::from(Span::styled(
            rule_title(&title),
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // "Loading..." — only before the first poll has delivered commands.
    if commands.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  Loading...",
                Style::default().fg(Color::DarkGray),
            ))),
            inner,
        );
        return;
    }

    let Some(level) = resolve_level(&state.filter, commands) else {
        return;
    };
    let items = if level.items.is_empty() {
        vec![Line::from(Span::styled(
            "  (no matches)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        render_rows(&level.items, state.selected, inner.width)
    };

    frame.render_widget(Paragraph::new(items).wrap(Wrap { trim: false }), inner);
}

/// List rows. The selected row is padded to `width` so its highlight bar
/// spans the full popup width — there are no side borders to end it at.
/// Rows that open a deeper level carry a `▸` marker.
fn render_rows(items: &[PopupItem], selected: usize, width: u16) -> Vec<Line<'_>> {
    let clamped = if items.is_empty() {
        0
    } else {
        selected.min(items.len() - 1)
    };

    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let mut name = format!("  {}  ", item.text);
            if item.has_children {
                name.push('▸');
                name.push(' ');
            }
            let desc = item.description.as_str();
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

    fn cmd_args(name: &str, args: &[(&str, &[&str])]) -> CommandInfo {
        CommandInfo {
            name: name.to_string(),
            description: format!("{name} description"),
            args: args
                .iter()
                .map(|(value, children)| CommandArg {
                    value: (*value).to_string(),
                    description: String::new(),
                    args: children
                        .iter()
                        .map(|c| CommandArg {
                            value: (*c).to_string(),
                            ..Default::default()
                        })
                        .collect(),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// `/model` with the models the server would send for it.
    fn model_cmd() -> CommandInfo {
        cmd_args(
            "/model",
            &[
                ("gpt-4", &[]),
                ("claude-3", &[]),
                ("reset", &[]),
                ("deepseek/deepseek-chat", &[]),
            ],
        )
    }

    /// `/skill on|off <name>` — a three-level command.
    fn skill_cmd() -> CommandInfo {
        cmd_args(
            "/skill",
            &[
                ("on", &["ponytail", "dev-workflow"]),
                ("off", &["ponytail", "dev-workflow"]),
                ("reset", &[]),
            ],
        )
    }

    fn keys(items: &[PopupItem]) -> Vec<&str> {
        items.iter().map(|i| i.text.as_str()).collect()
    }

    #[test]
    fn root_lists_commands_with_breadcrumb() {
        let commands = vec![make_cmd("/plan"), make_cmd("/model")];
        let level = resolve_level("", &commands).unwrap();
        assert_eq!(level.title, "Commands");
        assert!(level.prefix.is_empty());
        assert_eq!(keys(&level.items), vec!["/plan", "/model"]);
        assert!(!level.items[0].has_children, "/plan declares no args");

        // Partial matches with or without the slash, and carries no complete
        // value yet ("/mod" is a prefix of "/model", not the name itself).
        let level = resolve_level("/mod", &commands).unwrap();
        assert_eq!(keys(&level.items), vec!["/model"]);
        assert_eq!(level.complete, None);
        assert_eq!(
            resolve_level("mod", &commands).unwrap().complete,
            None,
            "a prefix is not complete"
        );
    }

    #[test]
    fn trailing_space_opens_the_next_level() {
        let commands = vec![model_cmd()];
        let level = resolve_level("/model ", &commands).unwrap();
        assert_eq!(level.title, "/model");
        assert_eq!(level.prefix, "/model ");
        assert_eq!(
            keys(&level.items),
            ["gpt-4", "claude-3", "reset", "deepseek/deepseek-chat"]
        );

        // Without the space the root is still showing, and the row advertises
        // a deeper level.
        let level = resolve_level("/model", &commands).unwrap();
        assert_eq!(level.title, "Commands");
        assert!(level.items[0].has_children);
    }

    #[test]
    fn arg_filter_is_a_substring_match() {
        let commands = vec![model_cmd()];
        let level = resolve_level("/model deep", &commands).unwrap();
        assert_eq!(keys(&level.items), vec!["deepseek/deepseek-chat"]);
        // Mid-name too — a slash-qualified id must answer to its tail.
        assert_eq!(
            keys(&resolve_level("/model sonnet", &commands).unwrap().items),
            Vec::<&str>::new()
        );
        assert_eq!(
            keys(&resolve_level("/model eep-c", &commands).unwrap().items),
            vec!["deepseek/deepseek-chat"]
        );
    }

    #[test]
    fn third_level_comes_from_the_parents_value() {
        let commands = vec![skill_cmd()];
        let level = resolve_level("/skill on ", &commands).unwrap();
        assert_eq!(level.title, "/skill on");
        assert_eq!(level.prefix, "/skill on ");
        assert_eq!(keys(&level.items), vec!["ponytail", "dev-workflow"]);

        // `reset` declares no names, so there is nothing to complete after it.
        assert!(resolve_level("/skill reset ", &commands).is_none());
        // An unknown second token is not a value of this command either.
        assert!(resolve_level("/skill maybe ", &commands).is_none());
    }

    #[test]
    fn free_text_arguments_have_no_level() {
        let commands = vec![make_cmd("/plan"), make_cmd("/grant")];
        // `/grant <path>` takes free text: the popup closes and the field
        // sends what was typed.
        assert!(resolve_level("/grant /tmp/x", &commands).is_none());
        assert!(resolve_level("/plan on", &commands).is_none());
        // An unknown command with an argument too.
        assert!(resolve_level("/nope x", &commands).is_none());
    }

    #[test]
    fn tab_hands_completion_to_the_caller() {
        let mut state = CommandPopupState::new();
        state.filter = "pl".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Tab), &mut state, &commands);
        assert_eq!(result, PopupAction::Complete("/plan".to_string()));
        // The popup no longer owns the filter text: the caller writes the
        // completion into the chat input field, and the filter follows.
        assert_eq!(state.filter, "pl");
    }

    #[test]
    fn tab_steps_into_a_command_with_children() {
        let mut state = CommandPopupState::new();
        // Even when the command name is already typed in full, Tab steps in
        // rather than closing — the picker is what the user wants here.
        for filter in ["/mod", "/model"] {
            state.filter = filter.to_string();
            state.selected = 0;
            let commands = vec![model_cmd(), make_cmd("/plan")];
            assert_eq!(
                handle_popup_key(key(KeyCode::Tab), &mut state, &commands),
                PopupAction::Complete("/model ".to_string()),
                "Tab on `{filter}` must open the model level"
            );
        }
    }

    #[test]
    fn tab_completes_selected_command_not_first() {
        let mut state = CommandPopupState::new();
        state.filter = String::new(); // All commands shown
        state.selected = 1; // Second item
        let commands = vec![make_cmd("/plan"), make_cmd("/model")];

        assert_eq!(
            handle_popup_key(key(KeyCode::Tab), &mut state, &commands),
            PopupAction::Complete("/model".to_string())
        );
    }

    #[test]
    fn tab_completes_the_selected_value_at_any_level() {
        let mut state = CommandPopupState::new();
        state.filter = "/skill on ".to_string();
        state.selected = 1;
        let commands = vec![skill_cmd()];

        assert_eq!(
            handle_popup_key(key(KeyCode::Tab), &mut state, &commands),
            PopupAction::Complete("/skill on dev-workflow".to_string())
        );
    }

    #[test]
    fn tab_no_op_when_no_commands_match() {
        let mut state = CommandPopupState::new();
        state.filter = "zzz".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Tab), &mut state, &commands);
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
                handle_popup_key(key(code), &mut state, &commands),
                PopupAction::PassThrough,
                "{code:?} must reach the chat input field"
            );
        }
        assert_eq!(state.filter, "/pl", "pass-through must not edit it");
    }

    #[test]
    fn keys_pass_through_when_nothing_can_be_completed() {
        let mut state = CommandPopupState::new();
        state.filter = "/grant /tmp".to_string();
        let commands = vec![make_cmd("/grant")];

        for code in [KeyCode::Tab, KeyCode::Enter, KeyCode::Up] {
            assert_eq!(
                handle_popup_key(key(code), &mut state, &commands),
                PopupAction::PassThrough,
                "{code:?} must reach the editor on a free-text path"
            );
        }
    }

    #[test]
    fn popup_height_is_top_rule_plus_clamped_list() {
        let commands: Vec<CommandInfo> = (0..20).map(|i| make_cmd(&format!("/c{i}"))).collect();
        // 1 top rule + at most 10 list rows.
        assert_eq!(popup_height(&CommandPopupState::new(), &commands), 11);
        // A filter matching nothing still reserves one row.
        let mut empty = CommandPopupState::new();
        empty.filter = "/zzz".to_string();
        assert_eq!(popup_height(&empty, &commands), 2);
    }

    #[test]
    fn enter_sends_command_in_command_mode() {
        let mut state = CommandPopupState::new();
        state.filter = "pl".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Enter), &mut state, &commands);
        assert_eq!(result, PopupAction::Send("/plan".to_string()));
    }

    #[test]
    fn enter_sends_the_whole_line_deeper_down() {
        let mut state = CommandPopupState::new();
        state.filter = "/model ".to_string();
        state.selected = 0;
        let commands = vec![model_cmd()];

        let result = handle_popup_key(key(KeyCode::Enter), &mut state, &commands);
        assert_eq!(result, PopupAction::Send("/model gpt-4".to_string()));
    }

    #[test]
    fn enter_falls_through_when_nothing_matches() {
        let mut state = CommandPopupState::new();
        state.filter = "zzz".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Enter), &mut state, &commands);
        // No command to send: the editor gets the key and sends the text.
        assert_eq!(result, PopupAction::PassThrough);
    }

    #[test]
    fn esc_closes_popup() {
        let mut state = CommandPopupState::new();
        let commands = vec![make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Esc), &mut state, &commands);
        assert_eq!(result, PopupAction::Close);
    }

    #[test]
    fn tab_copies_to_input_when_filter_complete_with_slash() {
        let mut state = CommandPopupState::new();
        state.filter = "/thinking".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/thinking"), make_cmd("/plan")];

        let result = handle_popup_key(key(KeyCode::Tab), &mut state, &commands);
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

        let first = handle_popup_key(key(KeyCode::Tab), &mut state, &commands);
        assert_eq!(first, PopupAction::Complete("/thinking".to_string()));

        state.filter = "/thinking".to_string(); // what the editor now holds
        let second = handle_popup_key(key(KeyCode::Tab), &mut state, &commands);
        assert_eq!(second, PopupAction::CopyToInput("/thinking".to_string()));
    }

    #[test]
    fn tab_then_tab_copies_to_input_at_the_second_level() {
        // Same two-step flow for argument values: "/model gpt" + Tab completes
        // the name into the input field, Tab again closes with it in place.
        let mut state = CommandPopupState::new();
        state.filter = "/model gpt".to_string();
        state.selected = 0;
        let commands = vec![model_cmd()];

        let first = handle_popup_key(key(KeyCode::Tab), &mut state, &commands);
        assert_eq!(first, PopupAction::Complete("/model gpt-4".to_string()));

        state.filter = "/model gpt-4".to_string();
        let second = handle_popup_key(key(KeyCode::Tab), &mut state, &commands);
        assert_eq!(second, PopupAction::CopyToInput("/model gpt-4".to_string()));
    }
}
