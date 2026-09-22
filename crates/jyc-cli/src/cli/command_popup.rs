use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use jyc_types::{CommandArg, CommandInfo};

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
    /// A deeper level exists below this row — Tab's trailing space opens it.
    pub has_children: bool,
}

/// The popup level the field text currently selects.
#[derive(Debug)]
pub struct PopupLevel {
    /// Top-rule name: "Commands" at the root, the command path
    /// (`/model`, `/skill on`) deeper.
    pub title: String,
    /// Field text this level's values are appended to — "" at the root,
    /// "/model " one level down. The empty prefix marks the first level, which
    /// is where Enter sends instead of completing.
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

/// True when `token` names `command`, with or without the slash and ignoring
/// case — the registry lowercases the command before dispatch, so `/MODEL x`
/// works server-side and the walk must not disagree with it. (Argument values
/// stay case-exact: the handlers are inconsistent there.)
fn matches_command(command: &str, token: &str) -> bool {
    let token = skip_slash(token);
    !token.is_empty() && skip_slash(command).eq_ignore_ascii_case(token)
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
    /// Enter pressed on the first level — send the command immediately.
    Send(String),
    /// Tab, or Enter below the first level, completed the selected row — write
    /// it into the chat input field and let the caller re-derive the level from
    /// the new text: a deeper level opens, and a row with nothing after it
    /// closes the popup. See [`completion_for`] for the exact text.
    Complete(String),
    /// Esc pressed — close the popup.
    Close,
}

/// The completion one press of Tab (or Enter below the first level) produces:
/// the selected row's line, or the canonical line when the field already names
/// a value at this level in full, always with one trailing space. That space
/// leaves the field ready for the next argument and is the marker
/// [`resolve_level`] reads: a deeper level opens there, and where nothing
/// follows, the caller's re-derivation closes the popup.
///
/// `None` when this level has no row to complete.
fn completion_for(level: &PopupLevel, selected: usize) -> Option<String> {
    let row = level.items.get(selected);
    let mut completion = if let Some(row) = row.filter(|r| r.has_children) {
        level.line(row)
    } else if let Some(complete) = &level.complete {
        // The field already names a value at this level in full.
        complete.clone()
    } else {
        level.line(row?)
    };
    if !completion.ends_with(' ') {
        completion.push(' ');
    }
    Some(completion)
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
        KeyCode::Tab => match completion_for(&level, state.selected) {
            Some(completion) => PopupAction::Complete(completion),
            None => PopupAction::None,
        },
        // Only the first level sends (`/model` fires the command that lists
        // models). Below it Enter is Tab — the value has to reach the field
        // first, so sending a command with arguments takes a second Enter. With
        // nothing to select the key belongs to the chat input field, which sends
        // what was typed.
        KeyCode::Enter if level.prefix.is_empty() => match level.items.get(state.selected) {
            Some(item) => PopupAction::Send(level.line(item)),
            None => PopupAction::PassThrough,
        },
        KeyCode::Enter => match completion_for(&level, state.selected) {
            Some(completion) => PopupAction::Complete(completion),
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
/// renderer and the layout must agree through this single helper. A list
/// longer than the reserved rows scrolls to follow the cursor — see
/// [`visible_rows`].
pub fn popup_height(state: &CommandPopupState, commands: &[CommandInfo]) -> u16 {
    let rows = resolve_level(&state.filter, commands).map_or(0, |l| l.items.len());
    1 + rows.clamp(1, 10) as u16
}

/// The slice of `items` a `height`-row list shows, with the cursor's index
/// inside that slice. The window slides one row at a time once the cursor
/// passes the bottom edge, so a list longer than its reserved rows stays fully
/// reachable instead of clipping the cursor away.
///
/// The same offset rule the topic explorer uses; the dashboard's topic table
/// gets this from ratatui's `render_stateful_widget` instead of hand-rolling it.
fn visible_rows(items: &[PopupItem], selected: usize, height: usize) -> (&[PopupItem], usize) {
    let selected = selected.min(items.len().saturating_sub(1));
    let offset = selected.saturating_sub(height.saturating_sub(1));
    let end = (offset + height).min(items.len());
    (&items[offset..end], selected - offset)
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
    let level = resolve_level(&state.filter, commands);
    let title = level.as_ref().map_or("Commands", |l| l.title.as_str());
    let block = Block::default()
        .title(Line::from(Span::styled(
            rule_title(title),
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

    // A free-text path: the caller closes the popup in `sync_command_popup`,
    // so this is only the one-frame race where the commands changed under it.
    let Some(level) = level else { return };
    let items = if level.items.is_empty() {
        vec![Line::from(Span::styled(
            "  (no matches)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        let (rows, cursor) = visible_rows(&level.items, state.selected, inner.height as usize);
        render_rows(rows, cursor)
    };

    frame.render_widget(Paragraph::new(items), inner);
}

/// List rows, one per item — no wrapping, so a row can never push the cursor
/// out of the window [`visible_rows`] picked. The selected row carries a `→` in
/// the two-column gutter and is dimmed, matching the question box's options.
/// Rows that open a deeper level carry a `▸` marker.
fn render_rows(items: &[PopupItem], selected: usize) -> Vec<Line<'_>> {
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let gutter = if i == selected { "→ " } else { "  " };
            let name = format!("{gutter}{}  ", item.text);
            let marker = if item.has_children { " ▸" } else { "" };
            if i != selected {
                return Line::from(vec![
                    Span::raw(name),
                    Span::styled(
                        item.description.as_str(),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(marker, Style::default().fg(Color::DarkGray)),
                ]);
            }
            let dim = Style::default().add_modifier(Modifier::DIM);
            let desc = format!(" {}{}", item.description, marker);
            Line::from(vec![Span::styled(name, dim), Span::styled(desc, dim)])
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
        // A value answers to any substring, across the provider slash too —
        // that is why argument levels match by `contains`, not `starts_with`.
        assert_eq!(
            keys(
                &resolve_level("/model /deepseek-chat", &commands)
                    .unwrap()
                    .items
            ),
            vec!["deepseek/deepseek-chat"]
        );
        // The partial is case-insensitive.
        assert_eq!(
            keys(&resolve_level("/model GPT", &commands).unwrap().items),
            vec!["gpt-4"]
        );
    }

    #[test]
    fn the_walk_matches_the_registries_case_folding() {
        let commands = vec![model_cmd()];
        // `/MODEL x` dispatches server-side (the registry lowercases the
        // command), so the walk must resolve it too — and hand back the
        // canonical spelling for the completion.
        let level = resolve_level("/MODEL gpt", &commands).unwrap();
        assert_eq!(level.prefix, "/model ");
        assert_eq!(level.title, "/model");
        assert_eq!(keys(&level.items), vec!["gpt-4"]);
        assert_eq!(level.complete, None);
        assert_eq!(
            resolve_level("/MODEL", &commands).unwrap().complete,
            Some("/model".to_string()),
            "a case-folded full name is still complete"
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
        // A command with nothing below it still gets the space: the field is
        // left ready for arguments, and the caller's re-derivation closes.
        assert_eq!(result, PopupAction::Complete("/plan ".to_string()));
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
            PopupAction::Complete("/model ".to_string())
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
            PopupAction::Complete("/skill on dev-workflow ".to_string())
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

    /// The window a clipped list shows: the cursor rides the bottom row once it
    /// passes the edge, so it can never be scrolled out of the popup.
    #[test]
    fn visible_rows_follows_the_cursor() {
        let items: Vec<PopupItem> = (0..20)
            .map(|i| PopupItem {
                text: format!("/c{i}"),
                description: String::new(),
                has_children: false,
            })
            .collect();

        // Short list: the whole thing, cursor untouched.
        let (rows, cursor) = visible_rows(&items[..3], 2, 10);
        assert_eq!((rows.len(), cursor), (3, 2));

        // Cursor inside the first window: nothing slides.
        let (rows, cursor) = visible_rows(&items, 9, 10);
        assert_eq!((rows.len(), cursor, rows[0].text.as_str()), (10, 9, "/c0"));

        // One past the edge: the window slides by one row, cursor on the last.
        let (rows, cursor) = visible_rows(&items, 10, 10);
        assert_eq!((rows.len(), cursor, rows[0].text.as_str()), (10, 9, "/c1"));

        // Near the top the window stays anchored on the first row; only the
        // cursor moving past the bottom edge slides it.
        let (rows, cursor) = visible_rows(&items, 5, 10);
        assert_eq!((rows.len(), cursor, rows[0].text.as_str()), (10, 5, "/c0"));

        // Past the end, and the degenerate cases, must not panic or slice out
        // of bounds.
        let (rows, cursor) = visible_rows(&items, 42, 10);
        assert_eq!((rows.len(), cursor, rows[0].text.as_str()), (10, 9, "/c10"));
        assert_eq!(visible_rows(&[], 0, 10).0.len(), 0);
        assert_eq!(visible_rows(&items, 3, 0).0.len(), 0);

        // The invariant the popup needs: for every cursor index, the row it
        // points at is inside the window.
        for i in 0..items.len() {
            let (rows, cursor) = visible_rows(&items, i, 10);
            assert_eq!(rows[cursor].text, format!("/c{i}"), "cursor {i}");
        }
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

    /// Below the first level Enter *is* Tab: the value lands in the field and
    /// the caller re-derives the level from it, so sending a command that takes
    /// arguments costs a second Enter. Asserted as literal equality with Tab so
    /// the two keys cannot drift apart.
    #[test]
    fn enter_completes_like_tab_below_the_root() {
        let commands = vec![model_cmd(), skill_cmd()];
        for filter in ["/model ", "/model g", "/skill on ", "/skill on po"] {
            let state = || {
                let mut state = CommandPopupState::new();
                state.filter = filter.to_string();
                state
            };
            let tab = handle_popup_key(key(KeyCode::Tab), &mut state(), &commands);
            let enter = handle_popup_key(key(KeyCode::Enter), &mut state(), &commands);
            assert!(matches!(tab, PopupAction::Complete(_)), "{filter}: {tab:?}");
            assert_eq!(tab, enter, "{filter}: Enter must mirror Tab");
        }
    }

    /// The first level sends even when the command has values below it — that
    /// is what makes Enter on `/model` list models instead of picking one.
    #[test]
    fn enter_sends_at_the_root_for_a_command_with_children() {
        let mut state = CommandPopupState::new();
        state.filter = "mo".to_string();
        let commands = vec![model_cmd()];

        assert_eq!(
            handle_popup_key(key(KeyCode::Enter), &mut state, &commands),
            PopupAction::Send("/model".to_string())
        );
    }

    /// Nothing to select at a deeper level: the key belongs to the input field,
    /// which sends the line as typed.
    #[test]
    fn enter_sends_the_typed_line_when_a_deeper_level_has_no_matches() {
        let mut state = CommandPopupState::new();
        state.filter = "/model zzz".to_string();
        let commands = vec![model_cmd()];

        assert_eq!(
            handle_popup_key(key(KeyCode::Enter), &mut state, &commands),
            PopupAction::PassThrough
        );
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
    fn tab_keeps_a_typed_value_over_the_selected_row() {
        let mut state = CommandPopupState::new();
        state.filter = "/thinking".to_string();
        state.selected = 1; // "/thinking-verbose", not the typed command
        let commands = vec![make_cmd("/thinking"), make_cmd("/thinking-verbose")];

        let result = handle_popup_key(key(KeyCode::Tab), &mut state, &commands);
        assert_eq!(result, PopupAction::Complete("/thinking ".to_string()));
        // The filter text itself is the caller's business (it is the input
        // field); the popup never rewrites it.
        assert_eq!(state.filter, "/thinking");
    }

    #[test]
    fn tab_on_a_leaf_command_leaves_the_caller_to_close_it() {
        // One Tab is now the whole completion. What it leaves behind
        // (`/thinking `) no longer points at a level, which is the condition
        // `sync_command_popup` closes on — proven end to end by
        // `tab_on_a_command_without_values_closes_the_popup`.
        let mut state = CommandPopupState::new();
        state.filter = "/think".to_string();
        state.selected = 0;
        let commands = vec![make_cmd("/thinking")];

        let first = handle_popup_key(key(KeyCode::Tab), &mut state, &commands);
        assert_eq!(first, PopupAction::Complete("/thinking ".to_string()));
        assert!(
            resolve_level("/thinking ", &commands).is_none(),
            "a leaf completion must leave nothing to show"
        );
    }

    #[test]
    fn tab_completes_a_value_at_the_second_level() {
        // Same for argument values: "/model gpt" + Tab writes the full id
        // followed by one space (nothing deeper to open).
        let mut state = CommandPopupState::new();
        state.filter = "/model gpt".to_string();
        state.selected = 0;
        let commands = vec![model_cmd()];

        assert_eq!(
            handle_popup_key(key(KeyCode::Tab), &mut state, &commands),
            PopupAction::Complete("/model gpt-4 ".to_string())
        );
    }
}
