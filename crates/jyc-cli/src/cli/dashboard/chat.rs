//! Chat pane: state, key handling, and rendering for the dashboard's
//! WebSocket thread chat and non-WebSocket detail mode.

use super::*;

/// Phase of the chat pane UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChatPhase {
    /// User is selecting a pattern to chat with.
    PatternSelect,
    /// User is actively chatting in a thread.
    Chatting,
}

/// Which pane has focus in chat mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChatFocus {
    /// The chat conversation pane.
    ChatPane,
    /// The activity log pane.
    ActivityPane,
}

/// A single message in the chat conversation.
#[derive(Debug, Clone)]
pub(super) struct ChatMessage {
    pub(super) sender: String,
    pub(super) text: String,
    pub(super) timestamp: Option<String>,
}

/// Chat pane state: WebSocket thread chat and non-WebSocket detail mode.
pub(super) struct ChatState {
    // Chat pane state
    pub(super) visible: bool,
    pub(super) phase: ChatPhase,
    pub(super) patterns: Vec<String>,
    pub(super) pattern_selected: usize,
    pub(super) thread: Option<String>,
    pub(super) channel: Option<String>,
    pub(super) messages: Vec<ChatMessage>,
    /// Vim-style editor state for the chat input (edtui).
    pub(super) editor: EditorState,
    /// Vim-mode key event handler for the chat input (edtui).
    pub(super) handler: EditorEventHandler,
    pub(super) focus: ChatFocus,
    pub(super) scroll: usize,
    pub(super) activity_scroll: usize,
    /// Pending `g` keypress for the `gg` (jump to top) sequence.
    pub(super) pending_g: bool,
    /// Horizontal scroll offset for the activity pane (left-right).
    pub(super) activity_hscroll: usize,
    /// Set locally when user sends a message, cleared when the poll confirms
    /// the thread is processing or has completed. Bridges the gap between
    /// sending a message and the inspect server reporting Processing status.
    pub(super) awaiting_response: bool,
    /// Activity pane visibility/size state.
    /// 0 = hidden, 1 = bottom 20%, 2 = bottom 80%, 3 = activity-only (full pane)
    pub(super) activity_split: u8,
    /// Thread info pane + bottom status bar visibility. They share state
    /// because `Ctrl+Z` toggles them together.
    /// `false` = both hidden (zen mode), `true` = both visible.
    pub(super) info_visible: bool,
    pub(super) ws_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub(super) ws_rx: tokio::sync::mpsc::UnboundedReceiver<WsEvent>,
    pub(super) ws_connected: bool,
    /// Channel name for the thread being viewed (used by live buffers).
    /// Set when chat is opened (Enter on a thread, or `c` to start fresh).
    pub(super) detail_channel: Option<String>,
    /// Thread path from ThreadInfo (for loading chat history from disk).
    /// Legacy detail-mode field; still set for the chat pane but no longer
    /// drives a different code path.
    pub(super) detail_thread_path: Option<std::path::PathBuf>,
    /// Live activity buffer — populated by REST hydrate on selection and
    /// appended to by WS `{"type":"activity",...}` events. Keyed by
    /// `(channel, thread)`. The activity pane and chat progress read
    /// exclusively from this buffer.
    pub(super) live_activity: std::collections::BTreeMap<
        (String, String),
        std::collections::VecDeque<jyc_types::ActivityEntry>,
    >,
    /// Live chat messages — populated by REST hydrate + WS `chat_message`.
    pub(super) live_chat: std::collections::BTreeMap<
        (String, String),
        std::collections::VecDeque<jyc_types::ChatMessageEntry>,
    >,
    /// Live thinking text — overwritten by WS `thinking` events.
    pub(super) live_thinking: std::collections::BTreeMap<(String, String), String>,
    /// Live processing status — updated by WS `processing` events.
    pub(super) live_processing: std::collections::BTreeMap<(String, String), (bool, bool)>,
    /// Last-seen monotonic id per (channel, thread) — used to drop duplicate
    /// WS events after reconnect / `resync`.
    pub(super) last_seen_id: std::collections::BTreeMap<(String, String), u64>,
    /// Last (channel, thread) that was REST-hydrated by the poll loop.
    /// Used to avoid re-hydrating the same thread on every poll when the
    /// user is browsing the overview.
    pub(super) last_hydrated_key: Option<(String, String)>,
    /// Address stash for `select_pattern` to call back into `open` when
    /// the user picks a pattern from the `c`-key pattern-select UI.
    pub(super) open_addr: Option<String>,
    // Command popup state
    pub(super) commands: Vec<CommandInfo>,
    pub(super) models: Vec<ModelInfo>,
    pub(super) command_popup: Option<CommandPopupState>,
    /// History of sent messages for Up/Down recall (newest appended last).
    pub(super) input_history: Vec<String>,
    /// Current position in history browsing (None = not browsing).
    pub(super) history_pos: Option<usize>,
    /// Authorization token to attach to WebSocket upgrade requests.
    pub(super) token: Option<String>,
}

/// Creates a fresh, empty chat input editor in Insert mode.
pub(super) fn empty_chat_editor() -> EditorState {
    let mut editor = EditorState::default();
    editor.mode = EditorMode::Insert;
    editor
}

impl ChatState {
    /// Replace the editor contents with `cmd`, cursor at end, Insert mode.
    /// Used by the command popup when delivering a selected command.
    pub(super) fn populate_editor(&mut self, cmd: &str) {
        self.editor = EditorState::new(Lines::from(cmd));
        self.editor.cursor.row = 0;
        self.editor.cursor.col = cmd.len();
        self.editor.mode = EditorMode::Insert;
    }
}

/// Format elapsed time from an RFC 3339 timestamp to now.
/// Returns a string like "15s" or "2m" or "" if parsing fails.
pub(super) fn format_elapsed(timestamp: &Option<String>) -> String {
    let ts = match timestamp {
        Some(t) => t,
        None => return String::new(),
    };
    let parsed = match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(_) => return String::new(),
    };
    let elapsed = chrono::Utc::now().signed_duration_since(parsed);
    let secs = elapsed.num_seconds();
    if secs < 0 {
        return String::new();
    }
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m", secs / 60)
    }
}

/// Format a message timestamp for the chat group header (╭─ line).
/// Shows "HH:MM" for today, "MM-DD HH:MM" for other dates.
pub(super) fn format_msg_time(ts: &Option<String>) -> String {
    let ts = match ts {
        Some(t) => t,
        None => return String::new(),
    };
    let parsed = match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(dt) => dt.with_timezone(&chrono::Local),
        Err(_) => return String::new(),
    };
    let now = chrono::Local::now();
    if parsed.date_naive() == now.date_naive() {
        parsed.format("%H:%M").to_string()
    } else {
        parsed.format("%m-%d %H:%M").to_string()
    }
}

/// Format elapsed time between two RFC 3339 timestamps for the chat group
/// footer (╰─ line). Falls back to now if `end` is None.
pub(super) fn format_group_elapsed(start: &Option<String>, end: &Option<String>) -> String {
    let start_ts = match start {
        Some(t) => t,
        None => return String::new(),
    };
    let start_dt = match chrono::DateTime::parse_from_rfc3339(start_ts) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(_) => return String::new(),
    };
    let end_dt = match end {
        Some(t) => match chrono::DateTime::parse_from_rfc3339(t) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(_) => return String::new(),
        },
        None => chrono::Utc::now(),
    };
    let elapsed = end_dt.signed_duration_since(start_dt);
    let secs = elapsed.num_seconds();
    if secs <= 0 {
        return String::new();
    }
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m", secs / 60)
    }
}

/// Count the number of visual lines when `text` is hard-wrapped within
/// `available_width`. Approximates the editor's own wrapping, which is
/// close enough for sizing the input area (the editor scrolls internally
/// if the estimate is off by a line).
pub(super) fn count_wrapped_lines(text: &str, available_width: u16) -> usize {
    let width = (available_width as usize).max(1);

    text.split('\n')
        .map(|line| (line.width().saturating_sub(1) / width) + 1)
        .sum::<usize>()
        .max(1)
}

/// Hard-wrap `text` to `max_width` display columns, preserving explicit
/// newlines and blank lines so the caller can render each segment as its
/// own row. Uses Unicode display widths (CJK, emoji) so wide characters
/// account for the columns they occupy. Characters wider than `max_width`
/// are placed alone on their row — a character cannot be split.
///
/// Zero-width characters (combining marks, ZWJ, etc.) attach to the
/// current row without advancing the width counter, matching how the
/// terminal renders them.
///
/// `max_width` is clamped to at least 1 to guarantee progress on extremely
/// narrow panes.
pub(super) fn wrap_text_to_width(text: &str, max_width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;

    let max_width = max_width.max(1);
    let mut out: Vec<String> = Vec::new();

    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            // Preserve blank lines from explicit `\n` sequences so vertical
            // spacing matches the source. An empty trailing segment after
            // a final `\n` is also kept for symmetric round-tripping.
            out.push(String::new());
            continue;
        }

        let mut current = String::new();
        let mut current_width: usize = 0;

        for ch in raw_line.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);

            // Flush before adding `ch` if it would overflow the row, but
            // never leave the row empty when `ch` itself is wider than
            // `max_width` — emit it on its own row instead.
            if !current.is_empty() && current_width + ch_width > max_width {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }

            current.push(ch);
            current_width += ch_width;
        }

        out.push(current);
    }

    out
}

/// Open an external editor ($VISUAL, $EDITOR, or vi) with the current chat
/// input, then replace the input with the edited contents.
///
/// The TUI is suspended (raw mode off, alternate screen left) while the
/// editor runs and restored afterwards regardless of the editor outcome.
pub(super) fn edit_input_externally(
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    let tmp = tempfile::Builder::new()
        .prefix("jyc-chat-")
        .suffix(".md")
        .tempfile()
        .context("Failed to create temp file for external editor")?;
    std::fs::write(tmp.path(), app.chat.text())
        .with_context(|| format!("Failed to write {}", tmp.path().display()))?;

    // Suspend the TUI so the editor takes over the terminal
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "vi".to_string());

    let status = std::process::Command::new(&editor).arg(tmp.path()).status();

    // Resume the TUI regardless of the editor outcome
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    terminal.clear()?;

    match status {
        Ok(s) if s.success() => {
            let edited = std::fs::read_to_string(tmp.path())
                .with_context(|| format!("Failed to read {}", tmp.path().display()))?;
            // Drop the single trailing newline editors typically append on save
            let edited = edited.strip_suffix('\n').unwrap_or(&edited);
            app.chat.editor = EditorState::new(Lines::from(edited));
            app.chat.editor.mode = EditorMode::Insert;
        }
        Ok(s) => {
            app.set_status(format!("Editor exited with {s}; input unchanged"));
        }
        Err(e) => {
            app.set_status(format!("Failed to launch editor `{editor}`: {e}"));
        }
    }
    Ok(())
}

pub(super) fn handle_chat_keys(
    app: &mut App,
    key: event::KeyEvent,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) {
    // Ctrl+Q quits the entire dashboard (consistent across all modes)
    let is_ctrl_q = key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL);

    if is_ctrl_q {
        app.should_quit = true;
        return;
    }

    // Ctrl+E opens an external editor to compose the chat input
    let is_ctrl_e = key.code == KeyCode::Char('e') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_e && app.chat.phase == ChatPhase::Chatting && app.chat.focus == ChatFocus::ChatPane {
        if let Err(e) = edit_input_externally(app, terminal) {
            app.set_status(format!("Editor error: {e:#}"));
        }
        return;
    }

    // Ctrl+A cycles the activity pane size. Replaces the previous Ctrl+W
    // binding.
    let is_ctrl_a = key.code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_a && app.chat.phase == ChatPhase::Chatting {
        app.chat.cycle_activity();
        return;
    }

    // Ctrl+Z toggles zen mode: hides/shows thread info pane + status bar,
    // and hides the activity pane if it was visible. Exiting zen mode does
    // not restore the activity pane — only info+status.
    let is_ctrl_z = key.code == KeyCode::Char('z') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_z && app.chat.phase == ChatPhase::Chatting {
        app.chat.toggle_zen_mode();
        return;
    }

    // Ctrl+C sends /cancel without modifying the input buffer (advertised in
    // CHANGELOG v0.3.12). Routes through `send_message_inner` (not
    // `send_message`) so the editor is untouched. The worker's
    // `pending_rx` select! arm in thread_manager.rs intercepts the
    // leading "/" and runs CancelCommandHandler, which fires the
    // per-thread CancellationToken. Restrict to Chatting — there is
    // no thread to cancel in PatternSelect.
    let is_ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_c && app.chat.phase == ChatPhase::Chatting {
        // Close any open command popup so the cancel path runs cleanly.
        app.chat.command_popup = None;
        app.chat.send_message_inner("/cancel".to_string());
        return;
    }

    // ── Command popup handling ─────────────────────────────────────
    if let Some(ref mut popup) = app.chat.command_popup {
        match handle_popup_key(key, popup, &app.chat.commands, &app.chat.models) {
            PopupAction::None => {}
            PopupAction::Close => {
                app.chat.command_popup = None;
            }
            PopupAction::Send(cmd) => {
                app.chat.command_popup = None;
                app.chat.send_message_inner(cmd);
            }
            PopupAction::CopyToInput(cmd) => {
                app.chat.command_popup = None;
                app.chat.populate_editor(&cmd);
            }
        }
        return;
    }

    // Check for "/" to open the command popup (before it reaches the editor)
    let is_slash = key.code == KeyCode::Char('/') && !key.modifiers.contains(KeyModifiers::CONTROL);
    if is_slash && app.chat.phase == ChatPhase::Chatting && app.chat.focus == ChatFocus::ChatPane {
        let should_open = match app.chat.editor.mode {
            // Normal mode: "/" always opens the popup
            EditorMode::Normal => true,
            // Insert mode: only when editor is empty (first char)
            EditorMode::Insert => app.chat.text().trim().is_empty(),
            _ => false,
        };
        if should_open {
            app.chat.command_popup = Some(CommandPopupState::new());
            return;
        }
    }

    match app.chat.phase {
        ChatPhase::PatternSelect => match key.code {
            KeyCode::Esc => {
                app.chat.close();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if app.chat.pattern_selected > 0 {
                    app.chat.pattern_selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.chat.pattern_selected + 1 < app.chat.patterns.len() {
                    app.chat.pattern_selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(pattern) = app.chat.patterns.get(app.chat.pattern_selected) {
                    let pattern = pattern.clone();
                    app.chat.select_pattern(pattern);
                }
            }
            _ => {}
        },
        ChatPhase::Chatting => {
            // `gg` sequence: a second consecutive `g` jumps to the top; any
            // other key resets the sequence state.
            let gg_jump = app.chat.gg_step(key.code == KeyCode::Char('g'));

            // App-level keys take precedence over the vim editor.
            match key.code {
                KeyCode::Tab => {
                    app.chat.toggle_focus();
                    return;
                }
                KeyCode::PageUp => {
                    app.chat.page_up();
                    return;
                }
                KeyCode::PageDown => {
                    app.chat.page_down();
                    return;
                }
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.chat.page_up();
                    return;
                }
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.chat.page_down();
                    return;
                }
                _ => {}
            }

            if app.chat.focus == ChatFocus::ActivityPane {
                match key.code {
                    KeyCode::Esc => {
                        app.chat.close();
                    }
                    KeyCode::Up | KeyCode::Char('k') => app.chat.scroll_up(),
                    KeyCode::Down | KeyCode::Char('j') => app.chat.scroll_down(),
                    KeyCode::Char('G') => app.chat.scroll_to_bottom(),
                    KeyCode::Char('g') if gg_jump => app.chat.scroll_to_top(),
                    KeyCode::Char('g') => {}
                    KeyCode::Left => {
                        app.chat.activity_hscroll = app.chat.activity_hscroll.saturating_sub(1)
                    }
                    KeyCode::Right => {
                        app.chat.activity_hscroll = app.chat.activity_hscroll.saturating_add(1)
                    }
                    _ => {}
                }
                return;
            }

            // Chat pane: vim editor input. Everything not matched here is
            // delegated to the edtui event handler.
            //
            // When the input holds at most one line, `j`/`k`/`gg`/`G` would
            // be no-ops in the editor, so in Normal mode they scroll the
            // message history instead. With multi-line input they remain
            // editor motions.
            let single_line_input = app.chat.editor.lines.len() <= 1;
            match (app.chat.editor.mode, key.code) {
                // Esc in Normal mode leaves the thread; in other modes the
                // editor uses it to return to Normal mode.
                (EditorMode::Normal, KeyCode::Esc) => {
                    app.chat.close();
                }
                // Plain Enter in Insert mode sends the message. Pasted
                // multi-line text goes through on_paste_event (not key events),
                // so no paste debounce is needed.
                (EditorMode::Insert, KeyCode::Enter)
                    if !key.modifiers.contains(KeyModifiers::SHIFT)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    app.chat.send_message()
                }
                // Shift/Alt+Enter in Insert mode inserts a newline.
                (EditorMode::Insert, KeyCode::Enter) => {
                    app.chat.handler.on_key_event(key, &mut app.chat.editor)
                }
                // Up/Down in Insert mode, when input is empty or browsing history, recall history.
                (EditorMode::Insert, KeyCode::Up)
                    if app.chat.text().trim().is_empty() || app.chat.history_pos.is_some() =>
                {
                    app.chat.recall_older()
                }
                (EditorMode::Insert, KeyCode::Down)
                    if app.chat.text().trim().is_empty() || app.chat.history_pos.is_some() =>
                {
                    app.chat.recall_newer()
                }
                // Enter in Normal mode also sends (newlines come from o/O).
                (EditorMode::Normal, KeyCode::Enter) => app.chat.send_message(),
                // In Normal mode, Up/Down scroll the message history (cursor
                // movement is on h/l). In other modes arrows go to the editor.
                (EditorMode::Normal, KeyCode::Up) => app.chat.scroll_up(),
                (EditorMode::Normal, KeyCode::Down) => app.chat.scroll_down(),
                // j/k and gg/G scroll the history only when the input is a
                // single line (where they are editor no-ops); multi-line
                // input keeps them as editor motions.
                (EditorMode::Normal, KeyCode::Char('k')) if single_line_input => {
                    app.chat.scroll_up()
                }
                (EditorMode::Normal, KeyCode::Char('j')) if single_line_input => {
                    app.chat.scroll_down()
                }
                (EditorMode::Normal, KeyCode::Char('G')) if single_line_input => {
                    app.chat.scroll_to_bottom()
                }
                (EditorMode::Normal, KeyCode::Char('g')) if single_line_input && gg_jump => {
                    app.chat.scroll_to_top()
                }
                (EditorMode::Normal, KeyCode::Char('g')) if single_line_input => {}
                _ => app.chat.handler.on_key_event(key, &mut app.chat.editor),
            }
        }
    }
}

pub(super) fn ui_chat_mode(frame: &mut Frame, area: Rect, app: &mut App) {
    // Layout for the chat screen — no channel bar, borderless chat pane.
    //
    //   ┌─────────── top row (chat + optional 20% info pane) ───────────┐
    //   │  chat conversation (borderless, fills horizontally)            │
    //   │  ┌── thread info pane (20% wide, only when info_visible) ──┐  │
    //   │  │   ...                                                   │  │
    //   │  └─────────────────────────────────────────────────────────┘  │
    //   ├────────── bottom area: status bar + activity pane ────────────┤
    //   │  status bar (1 line; only when info_visible)                  │
    //   │  activity pane (bottom 20% / 80% / full when visible)         │
    //   └───────────────────────────────────────────────────────────────┘
    //
    // Both status bar and thread info pane are tied to `info_visible` so
    // `Ctrl+Z` toggles them together.

    if app.chat.phase == ChatPhase::PatternSelect {
        // Pattern select is the initial screen when no thread is chosen.
        // No channel bar, no status bar — the user is picking where to go.
        let mut chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // Thread info pane
                Constraint::Min(0),    // Pattern select
                Constraint::Length(1), // Status bar
            ])
            .split(area);
        if !app.chat.info_visible {
            // Zen-style: drop info row and status row, expand pattern area.
            chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0)])
                .split(area);
        }
        render_pattern_select(frame, chunks[chunks.len() - 1], app);
        if app.chat.info_visible {
            render_thread_info_pane(frame, chunks[0], app);
            render_status_bar(frame, chunks[2], app);
        }
        return;
    }

    // Chatting phase.
    let show_status = app.chat.info_visible;
    let show_activity = app.chat.activity_split != 0;

    // Build vertical constraints for: [top row, activity_row?, status_row?].
    let activity_pct = match app.chat.activity_split {
        1 => 20,
        2 => 80,
        3 => 100,
        _ => 0,
    };
    let mut vertical_constraints: Vec<Constraint> = vec![Constraint::Min(0)];
    if show_activity {
        vertical_constraints.push(Constraint::Percentage(activity_pct));
    }
    if show_status {
        vertical_constraints.push(Constraint::Length(1));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vertical_constraints)
        .split(area);

    // Top row always holds the chat conversation + optional info pane.
    let top_row = chunks[0];
    if app.chat.info_visible {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(80), Constraint::Percentage(20)])
            .split(top_row);
        render_chat_conversation(frame, cols[0], app);
        render_thread_info_pane(frame, cols[1], app);
    } else {
        render_chat_conversation(frame, top_row, app);
    }

    if show_activity {
        // Activity pane takes the chunk just below the top row.
        let activity_chunk_idx = 1;
        let activity_chunk = chunks[activity_chunk_idx];
        if app.chat.activity_split == 3 {
            // Activity-only — also hides the chat conversation.
            render_activity_log(frame, activity_chunk, app);
        } else {
            render_activity_log(frame, activity_chunk, app);
        }
        if show_status {
            render_status_bar(frame, chunks[2], app);
        }
    } else if show_status {
        render_status_bar(frame, chunks[1], app);
    }
}

/// Render the right-hand thread info pane (always 20% wide when shown).
///
/// Displays thread name, channel, pattern, model, mode, tokens, and a
/// processing indicator. Wraps content in a bordered `Block` so it is
/// visually separable from the borderless chat pane.
pub(super) fn render_thread_info_pane(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().title(" Thread Info ").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let state = match &app.state {
        Some(s) => s,
        None => return,
    };

    let selected = app
        .chat
        .thread
        .as_ref()
        .and_then(|chat_name| state.threads.iter().find(|t| t.name == *chat_name))
        .or_else(|| {
            app.table_state
                .selected()
                .and_then(|i| state.threads.get(i))
        });

    let lines: Vec<Line> = if let Some(t) = selected {
        let mut out: Vec<Line> = Vec::new();
        out.push(Line::from(vec![
            Span::styled("Thread: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&t.name),
        ]));
        out.push(Line::from(vec![
            Span::styled("Channel: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&t.channel),
        ]));
        out.push(Line::from(vec![
            Span::styled("Pattern: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(t.pattern.as_deref().unwrap_or("-")),
        ]));
        if let Some(ref model) = t.model {
            out.push(Line::from(vec![
                Span::styled("Model: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(model),
            ]));
        }
        out.push(Line::from(vec![
            Span::styled("Mode: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(t.mode.as_deref().unwrap_or("build")),
        ]));
        if let (Some(cur), Some(max)) = (t.input_tokens, t.max_tokens) {
            let pct = if max > 0 {
                cur.checked_mul(100)
                    .and_then(|v| v.checked_div(max))
                    .unwrap_or(0)
            } else {
                0
            };
            out.push(Line::from(vec![
                Span::styled("Tokens: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("{cur} / {max} ({pct}%)")),
            ]));
        }
        if t.status == ThreadStatus::Processing {
            out.push(Line::from(Span::styled(
                "⏳ AI thinking...",
                Style::default().fg(Color::Yellow),
            )));
        }
        out
    } else {
        vec![Line::from("Select a thread")]
    };

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: true });
    frame.render_widget(paragraph, inner);
}

pub(super) fn render_pattern_select(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(" Select Pattern ")
        .borders(Borders::ALL);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.chat.patterns.is_empty() {
        let text = Paragraph::new(Span::styled(
            "  No patterns available",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(text, inner);
        return;
    }

    let lines: Vec<Line> = app
        .chat
        .patterns
        .iter()
        .enumerate()
        .map(|(i, pattern)| {
            if i == app.chat.pattern_selected {
                Line::from(vec![Span::styled(
                    format!("> {pattern}"),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )])
            } else {
                Line::from(vec![Span::raw("  "), Span::raw(pattern)])
            }
        })
        .collect();

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: true });
    frame.render_widget(paragraph, inner);
}

pub(super) fn render_chat_conversation(frame: &mut Frame, area: Rect, app: &mut App) {
    // Borderless chat pane — no outer block, no side borders, no
    // per-message `│ ` gutter. Each chat round is bounded by a horizontal
    // top rule (time on the left) and a horizontal bottom rule (duration
    // right-aligned). When the chat pane is focused, draw a faint
    // background so the cursor position is still discoverable.
    if app.chat.focus == ChatFocus::ChatPane {
        let bg = Block::default().style(Style::default().bg(Color::Reset));
        frame.render_widget(bg, area);
    }

    // Split: scrollable messages (top) + dynamic input area (bottom)
    // Input area grows with content (up to 10 rows) for multi-line editing.
    // Subtract the 2-column "> " prompt gutter from the wrap width.
    let input_line_count =
        count_wrapped_lines(&app.chat.text(), area.width.saturating_sub(2)).clamp(1, 10) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(input_line_count)])
        .split(area);

    // --- Messages area (markdown-rendered) ---
    let renderer = ratatui_markdown::markdown::MarkdownRenderer::new(chunks[0].width as usize);
    let theme = ratatui_markdown::theme::ThemeConfig::default();

    let mut all_lines: Vec<Line> = Vec::new();

    let dim_style = Style::default().fg(Color::DarkGray);
    let mut group_start_ts: Option<String> = None;

    for (idx, msg) in app.chat.messages.iter().enumerate() {
        let is_user = msg.sender == "user";
        let prefix = if is_user { "**You:** " } else { "**AI:** " };

        let prev_sender = if idx > 0 {
            Some(app.chat.messages[idx - 1].sender.as_str())
        } else {
            None
        };

        // Close previous round when transitioning AI → user. Bottom rule
        // has the duration right-aligned.
        if is_user && prev_sender == Some("ai") {
            let last_ts = app
                .chat
                .messages
                .get(idx - 1)
                .and_then(|m| m.timestamp.clone());
            let elapsed = format_group_elapsed(&group_start_ts, &last_ts);
            let width = chunks[0].width as usize;
            if elapsed.is_empty() {
                let dashes = "─".repeat(width);
                all_lines.push(Line::from(Span::styled(dashes, dim_style)));
            } else {
                // Right-align: <dashes> <elapsed>
                let dash_count = width.saturating_sub(elapsed.len());
                all_lines.push(Line::from(vec![
                    Span::styled("─".repeat(dash_count), dim_style),
                    Span::styled(elapsed, dim_style),
                ]));
            }
            all_lines.push(Line::from(""));
            group_start_ts = None;
        }

        // Open new round at the start of a user turn. Top rule has the
        // timestamp left-aligned (no time → plain rule).
        if is_user {
            group_start_ts = msg.timestamp.clone();
            let time_str = format_msg_time(&msg.timestamp);
            let width = chunks[0].width as usize;
            if time_str.is_empty() {
                all_lines.push(Line::from(Span::styled(
                    "─".repeat(width),
                    dim_style,
                )));
            } else {
                let dash_count = width.saturating_sub(time_str.len() + 1);
                all_lines.push(Line::from(vec![
                    Span::styled(time_str, dim_style),
                    Span::styled(format!(" {}", "─".repeat(dash_count)), dim_style),
                ]));
            }
        }

        // Render message (no side gutters).
        let md_text = format!("{prefix}{}\n", msg.text);
        let blocks = renderer.parse(&md_text);
        let msg_lines = renderer.render(&blocks, &theme);
        all_lines.extend(msg_lines);
    }

    // Close any open round at the end.
    if group_start_ts.is_some() {
        let last_ts = app.chat.messages.last().and_then(|m| m.timestamp.clone());
        let elapsed = format_group_elapsed(&group_start_ts, &last_ts);
        let width = chunks[0].width as usize;
        if elapsed.is_empty() {
            let dashes = "─".repeat(width);
            all_lines.push(Line::from(Span::styled(dashes, dim_style)));
        } else {
            let dash_count = width.saturating_sub(elapsed.len());
            all_lines.push(Line::from(vec![
                Span::styled("─".repeat(dash_count), dim_style),
                Span::styled(elapsed, dim_style),
            ]));
        }
        all_lines.push(Line::from(""));
    }

    // Show progress indicator
    // Determine if the thread is processing: prefer the live processing
    // status (updated via WS `processing` events), fall back to the polled
    // overview state, fall back to local `awaiting_response`.
    let live_processing = app
        .chat
        .channel
        .as_deref()
        .zip(app.chat.thread.as_deref())
        .and_then(|(c, t)| app.chat.live_processing_for(c, t));
    let server_processing = match live_processing {
        Some((p, _)) => p,
        None => app
            .state
            .as_ref()
            .and_then(|s| {
                let chat_name = app.chat.thread.as_deref()?;
                s.threads.iter().find(|t| t.name == chat_name)
            })
            .is_some_and(|ct| ct.status == ThreadStatus::Processing),
    };

    // Show progress if the server reports processing OR we've sent a message
    // locally and are still waiting for the server state to catch up.
    let show_progress = server_processing || app.chat.awaiting_response;

    if show_progress {
        // Read live activity + thinking from the WS-fed buffers, falling
        // back to the polled overview only as a last resort. The rendering
        // logic below is byte-for-byte identical to before — only the data
        // source changed.
        let live_chan = app.chat.channel.clone();
        let live_thread = app.chat.thread.clone();
        let activity_entries: Vec<jyc_types::ActivityEntry> = live_chan
            .as_deref()
            .zip(live_thread.as_deref())
            .map(|(c, t)| {
                app.chat
                    .live_activity_for(c, t)
                    .rev()
                    .take(2)
                    .cloned()
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect()
            })
            .unwrap_or_default();

        let thinking_text: Option<String> = live_chan
            .as_deref()
            .zip(live_thread.as_deref())
            .and_then(|(c, t)| app.chat.live_thinking_for(c, t))
            .map(|s| s.to_string());

        // Render thinking text first (same wrap + indent as before).
        // This comes from ThreadEvent::Thinking events and is NOT stored
        // in the activity buffer or activity.jsonl.
        if let Some(thinking) = thinking_text.as_deref()
            && !thinking.is_empty()
        {
            let gray_style = Style::default().fg(Color::Gray);
            // Hard-wrap long lines so nothing is clipped at the right edge.
            // The 2 accounts for the "  " indent prefix below; each wrapped
            // segment becomes its own `Line` so the scroll calculation sees
            // the correct visual row count.
            let avail = chunks[0].width.saturating_sub(2) as usize;
            for line in wrap_text_to_width(thinking, avail) {
                all_lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(line, gray_style),
                ]));
            }
        }

        if activity_entries.is_empty() && thinking_text.is_none() {
            all_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "⏳ AI is thinking...",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        } else {
            let total = activity_entries.len();
            for (idx, a) in activity_entries.iter().enumerate() {
                let is_last = idx == total - 1;
                let elapsed = if is_last {
                    format_elapsed(&a.timestamp)
                } else {
                    String::new()
                };
                let style = Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::ITALIC);

                // Split multi-line entries (e.g. edit diff) into separate lines.
                // Try parsing as JSON first — edit events store full data as JSON.
                let rendered_lines: Vec<String> = if let Ok(json) =
                    serde_json::from_str::<serde_json::Value>(&a.text)
                    && json.get("type").and_then(|t| t.as_str()) == Some("edit")
                {
                    let file_path = json
                        .get("file_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let line_no = json.get("line_no").and_then(|v| v.as_u64());
                    let old_str = json
                        .get("old_string")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let new_str = json
                        .get("new_string")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let location = match line_no {
                        Some(n) => format!("{file_path}:{n}"),
                        None => file_path.to_string(),
                    };
                    let mut out = Vec::new();
                    // Header line
                    if is_last {
                        if elapsed.is_empty() {
                            out.push(format!("⏳ {location}"));
                        } else {
                            out.push(format!("⏳ {location} {elapsed}"));
                        }
                    } else {
                        out.push(format!("   {location}"));
                    }
                    // Old lines
                    for line in old_str.split('\n') {
                        out.push(format!("  -{line}"));
                    }
                    // New lines
                    for line in new_str.split('\n') {
                        out.push(format!("  +{line}"));
                    }
                    out
                } else if let Ok(json) = serde_json::from_str::<serde_json::Value>(&a.text)
                    && json.get("type").and_then(|t| t.as_str()) == Some("write")
                {
                    let file_path = json
                        .get("file_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let content = json.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    let mut out = Vec::new();
                    // Header line
                    if is_last {
                        if elapsed.is_empty() {
                            out.push(format!("⏳ {file_path}"));
                        } else {
                            out.push(format!("⏳ {file_path} {elapsed}"));
                        }
                    } else {
                        out.push(format!("   {file_path}"));
                    }
                    // Content lines (truncated to avoid flooding the pane)
                    let content_lines: Vec<&str> = content.split('\n').collect();
                    let max_lines = 20;
                    for line in content_lines.iter().take(max_lines) {
                        out.push(format!("  +{line}"));
                    }
                    if content_lines.len() > max_lines {
                        out.push(format!(
                            "  … ({} more lines)",
                            content_lines.len() - max_lines
                        ));
                    }
                    out
                } else {
                    // Plain text — split by newlines for display
                    let lines: Vec<&str> = a.text.split('\n').collect();
                    lines
                        .iter()
                        .enumerate()
                        .map(|(line_idx, line)| {
                            if line_idx == 0 && is_last {
                                if elapsed.is_empty() {
                                    format!("⏳ {line}")
                                } else {
                                    format!("⏳ {line} {elapsed}")
                                }
                            } else {
                                // Pad with 3 spaces to visually align with "⏳ "
                                format!("   {line}")
                            }
                        })
                        .collect()
                };

                for label in rendered_lines {
                    let label_style = if label.starts_with("  -") {
                        Style::default().fg(Color::Gray)
                    } else {
                        style
                    };
                    all_lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(label, label_style),
                    ]));
                }
            }
        }
    }

    let inner_height = chunks[0].height as usize;
    let max_skip = all_lines.len().saturating_sub(inner_height);
    app.chat.scroll = app.chat.scroll.min(max_skip);
    let skip = max_skip.saturating_sub(app.chat.scroll);
    let visible_lines: Vec<Line> = all_lines.into_iter().skip(skip).collect();

    let messages_para = Paragraph::new(visible_lines);
    frame.render_widget(messages_para, chunks[0]);

    // --- Input area (vim editor, at bottom) ---
    // The editor renders its own wrapping, scroll-follow, and mode status
    // line. The cursor is a blinking underline in Insert mode and the
    // default inverted block otherwise; hidden when the activity pane has
    // focus. A "> " prompt sits in a 2-column gutter left of the editor.
    let theme = EditorTheme::default()
        .base(Style::default())
        .hide_status_line();
    let theme = match app.chat.focus {
        ChatFocus::ActivityPane => theme.hide_cursor(),
        ChatFocus::ChatPane => match app.chat.editor.mode {
            EditorMode::Insert => theme.cursor_style(
                Style::default()
                    .add_modifier(Modifier::UNDERLINED)
                    .add_modifier(Modifier::SLOW_BLINK),
            ),
            _ => theme,
        },
    };
    let [prompt_area, editor_area] =
        Layout::horizontal([Constraint::Length(2), Constraint::Min(0)]).areas(chunks[1]);
    frame.render_widget(
        Paragraph::new("> ").style(Style::default().fg(Color::Yellow)),
        prompt_area,
    );
    EditorView::new(&mut app.chat.editor)
        .theme(theme)
        .wrap(true)
        .render(editor_area, frame.buffer_mut());

    // ── Command popup overlay ──
    if let Some(ref popup) = app.chat.command_popup {
        render_command_popup(frame, area, popup, &app.chat.commands, &app.chat.models);
    }
}

/// Filter out activity entries that should not be shown in user-facing
/// activity panes (overview activity pane, chat activity pane, chat
/// progress). Mirrors the server-side `is_user_visible_activity` in
/// `jyc-inspect` for client-side filtering.
fn is_user_visible_activity(entry: &jyc_types::ActivityEntry) -> bool {
    if entry.is_internal {
        return false;
    }
    // Backward compat for old log files (pre-`is_internal` field).
    if entry.text.ends_with(" chars)") {
        return false;
    }
    true
}

pub(super) fn render_activity_log(frame: &mut Frame, area: Rect, app: &mut App) {
    // Activity pane source-of-truth: WS-fed `live_activity` buffer for the
    // currently focused thread. Falls back to empty slice if no live data
    // has been seeded yet (transient state during hydrate).
    let activity_vec: Vec<jyc_types::ActivityEntry> =
        if app.chat.visible && app.chat.phase == ChatPhase::Chatting {
            let (chan, thread) = (app.chat.channel.clone(), app.chat.thread.clone());
            match (chan, thread) {
                (Some(c), Some(t)) => app.chat.live_activity_for(&c, &t).cloned().collect(),
                _ => Vec::new(),
            }
        } else if let Some(state) = &app.state {
            // Overview mode: show the activity for the table-selected thread
            // (also pulled from live buffers, hydrated when the row is selected).
            let selected_idx = app.table_state.selected();
            if let Some(idx) = selected_idx {
                if let Some(t) = state.threads.get(idx) {
                    app.chat
                        .live_activity_for(&t.channel, &t.name)
                        .cloned()
                        .collect()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

    let focused = app.chat.visible && app.chat.focus == ChatFocus::ActivityPane;
    let inner_height = area.height.saturating_sub(2) as usize; // subtract borders
    // Internal entries (`is_internal=true`) and Thinking heartbeats are
    // excluded from the activity pane. The chat pane's AI progress area
    // handles thinking display; the in-memory log keeps them for debug.
    let visible_count = activity_vec
        .iter()
        .filter(|&e| is_user_visible_activity(e))
        .count();
    let max_skip = visible_count.saturating_sub(inner_height);
    app.chat.activity_scroll = app.chat.activity_scroll.min(max_skip);
    render_activity_log_inner(
        frame,
        area,
        &activity_vec,
        app.chat.activity_scroll,
        app.chat.activity_hscroll,
        focused,
    );
}

pub(super) fn render_activity_log_inner(
    frame: &mut Frame,
    area: Rect,
    activity: &[jyc_types::ActivityEntry],
    scroll_offset: usize,
    hscroll: usize,
    focused: bool,
) {
    let mut block = Block::default().title(" Activity ").borders(Borders::ALL);
    if focused {
        block = block.border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    }

    if activity.is_empty() {
        let text = Paragraph::new(Span::styled(
            "  No activity",
            Style::default().fg(Color::DarkGray),
        ))
        .block(block);
        frame.render_widget(text, area);
        return;
    }

    // Internal entries (`is_internal=true`) and Thinking heartbeats are
    // excluded from the activity pane - they appear as dozens of identical
    // "Thinking..." / "tool execution (Xs, Y chars)" markers and crowd out
    // useful events. The chat pane AI progress area handles thinking display.
    let visible: Vec<_> = activity
        .iter()
        .filter(|&e| is_user_visible_activity(e))
        .collect();

    if visible.is_empty() {
        let text = Paragraph::new(Span::styled(
            "  No activity",
            Style::default().fg(Color::DarkGray),
        ))
        .block(block);
        frame.render_widget(text, area);
        return;
    }

    let inner_height = area.height.saturating_sub(2) as usize; // subtract borders
    let max_skip = visible.len().saturating_sub(inner_height);
    let skip = max_skip.saturating_sub(scroll_offset);

    let activity_lines: Vec<Line> = visible
        .iter()
        .skip(skip)
        .map(|entry| {
            let time_str = entry
                .timestamp
                .as_deref()
                .and_then(|ts| {
                    chrono::DateTime::parse_from_rfc3339(ts)
                        .ok()
                        .map(|dt| dt.format("%H:%M:%S").to_string())
                })
                .unwrap_or_else(|| "-".to_string());
            let text_style = match entry.severity {
                Severity::Error => Style::default().fg(Color::Red),
                Severity::Warning => Style::default().fg(Color::Yellow),
                Severity::Info => Style::default(),
            };
            Line::from(vec![
                Span::styled(
                    format!("  {time_str} "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(&entry.text, text_style),
            ])
        })
        .collect();

    let text = Paragraph::new(activity_lines)
        .block(block)
        .scroll((0, hscroll as u16));
    frame.render_widget(text, area);
}

impl ChatState {
    pub(super) fn new(ws_rx: tokio::sync::mpsc::UnboundedReceiver<WsEvent>) -> Self {
        Self {
            visible: false,
            phase: ChatPhase::PatternSelect,
            patterns: vec![],
            pattern_selected: 0,
            thread: None,
            channel: None,
            messages: vec![],
            editor: empty_chat_editor(),
            handler: EditorEventHandler::default(),
            focus: ChatFocus::ChatPane,
            scroll: 0,
            activity_scroll: 0,
            pending_g: false,
            activity_hscroll: 0,
            awaiting_response: false,
            activity_split: 0,
            info_visible: false,
            ws_tx: None,
            ws_rx,
            ws_connected: false,
            detail_channel: None,
            detail_thread_path: None,
            live_activity: std::collections::BTreeMap::new(),
            live_chat: std::collections::BTreeMap::new(),
            live_thinking: std::collections::BTreeMap::new(),
            live_processing: std::collections::BTreeMap::new(),
            last_seen_id: std::collections::BTreeMap::new(),
            last_hydrated_key: None,
            open_addr: None,
            commands: vec![],
            models: vec![],
            command_popup: None,
            input_history: vec![],
            history_pos: None,
            token: None,
        }
    }

    pub(super) fn open(
        &mut self,
        addr: &str,
        channel: Option<&str>,
        initial_thread: Option<&str>,
        token: Option<String>,
    ) {
        self.visible = true;
        self.phase = if initial_thread.is_some() {
            ChatPhase::Chatting
        } else {
            ChatPhase::PatternSelect
        };
        self.patterns.clear();
        self.pattern_selected = 0;
        self.channel = channel.map(|s| s.to_string());
        self.thread = initial_thread.map(|s| s.to_string());
        self.token = token;
        self.messages.clear();
        self.editor = empty_chat_editor();
        self.focus = ChatFocus::ChatPane;
        self.scroll = 0;
        self.activity_scroll = 0;
        self.activity_hscroll = 0;
        self.pending_g = false;
        self.activity_split = 0;
        self.info_visible = false;
        self.ws_connected = false;
        self.input_history.clear();
        self.history_pos = None;
        // Clear the poll-loop's last-hydrated key so it doesn't skip hydrate
        // when we switch back to overview later.
        self.last_hydrated_key = None;

        // No WS yet — the chat starts in PatternSelect (if no initial thread)
        // and opens a scoped WS only after the user picks a pattern
        // (see `open_pattern_select` + `select_pattern`).
        if initial_thread.is_none() {
            // Drop any stale WS connection from a prior chat.
            if let Some(tx) = self.ws_tx.take() {
                let _ = tx.send("{\"type\":\"disconnect\"}".to_string());
            }
            return;
        }

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        self.ws_tx = Some(cmd_tx);
        // Replace the old receiver with the new one
        self.ws_rx = event_rx;

        let url = match (channel, initial_thread) {
            (Some(ch), Some(th)) => format!("ws://{}/ws/{}/{}", addr, ch, th),
            (Some(ch), None) => format!("ws://{}/ws/{}", addr, ch),
            (None, _) => format!("ws://{}/ws", addr),
        };
        tokio::spawn(ws_client_task(url, cmd_rx, event_tx, self.token.clone()));
    }

    /// Open the chat pane in PatternSelect mode for the `c` key.
    /// Fetches enabled pattern names via REST (replaces the old WebSocket
    /// `list_patterns` command). No WS is opened until the user picks a
    /// pattern (then `select_pattern` opens a scoped WS).
    pub(super) async fn open_pattern_select(
        &mut self,
        addr: &str,
        channel: &str,
        client: &mut InspectClient,
        token: Option<String>,
    ) {
        self.visible = true;
        self.phase = ChatPhase::PatternSelect;
        self.channel = Some(channel.to_string());
        self.thread = None;
        self.token = token;
        self.patterns = client.list_patterns(channel).await.unwrap_or_default();
        self.pattern_selected = 0;
        self.messages.clear();
        self.editor = empty_chat_editor();
        self.focus = ChatFocus::ChatPane;
        self.scroll = 0;
        self.activity_scroll = 0;
        self.activity_hscroll = 0;
        self.pending_g = false;
        self.activity_split = 0;
        self.info_visible = false;
        self.ws_connected = false;
        self.input_history.clear();
        self.history_pos = None;
        self.last_hydrated_key = None;
        // Drop any stale WS connection from a prior chat.
        if let Some(tx) = self.ws_tx.take() {
            let _ = tx.send("{\"type\":\"disconnect\"}".to_string());
        }
        // Stash addr for the eventual `select_pattern` call. The polling
        // loop in mod.rs owns the actual `addr` parameter; here we just
        // store it so select_pattern can call back into open.
        self.open_addr = Some(addr.to_string());
    }

    pub(super) fn close(&mut self) {
        self.visible = false;
        self.phase = ChatPhase::PatternSelect;
        self.ws_connected = false;
        self.command_popup = None;
        self.detail_channel = None;
        self.detail_thread_path = None;
        self.last_hydrated_key = None;
        if let Some(tx) = self.ws_tx.take() {
            // Best-effort disconnect signal
            let _ = tx.send("{\"type\":\"disconnect\"}".to_string());
        }
    }

    /// Open the chat pane for any thread, regardless of channel type.
    ///
    /// All channels are now reached via the unified `/ws/<channel>/<thread>`
    /// endpoint, so this method just initializes the chat UI state.
    /// The actual WS connection and message routing happen in the dashboard
    /// poll loop (see `mod.rs::run`).
    pub(super) fn open_thread_detail(
        &mut self,
        channel: &str,
        thread_name: &str,
        _state: Option<&jyc_types::InspectOverview>,
    ) {
        self.visible = true;
        self.phase = ChatPhase::Chatting;
        self.thread = Some(thread_name.to_string());
        self.channel = Some(channel.to_string());
        self.detail_channel = Some(channel.to_string());
        self.detail_thread_path = None;
        self.messages.clear();
        self.editor = empty_chat_editor();
        self.focus = ChatFocus::ChatPane;
        self.scroll = 0;
        self.activity_scroll = 0;
        self.activity_hscroll = 0;
        self.pending_g = false;
        self.activity_split = 0;
        self.info_visible = false;
        self.ws_connected = false;
        self.input_history.clear();
        self.history_pos = None;
        // The mod.rs Enter handler triggers hydrate_live after this; clear
        // the last-hydrated key so the poll loop doesn't re-hydrate over us.
        self.last_hydrated_key = None;
    }

    /// Legacy no-op kept for compatibility with the test suite.
    #[allow(dead_code)]
    pub(super) fn load_detail_history(&mut self, _state: Option<&jyc_types::InspectOverview>) {}

    /// Legacy no-op kept for compatibility with the test suite.
    #[allow(dead_code)]
    pub(super) fn is_detail_mode(&self) -> bool {
        self.detail_channel.is_some()
    }

    pub(super) fn select_pattern(&mut self, pattern: String) {
        let channel = match &self.channel {
            Some(c) => c.clone(),
            None => return,
        };
        let addr = match &self.open_addr {
            Some(a) => a.clone(),
            None => return,
        };

        self.select_pattern_inner(pattern.clone());

        let url = format!("ws://{}/ws/{}/{}", addr, channel, pattern);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        self.ws_tx = Some(cmd_tx);
        self.ws_rx = event_rx;
        tokio::spawn(super::ws::ws_client_task(
            url,
            cmd_rx,
            event_tx,
            self.token.clone(),
        ));
    }

    /// Clear state and set thread — used by `select_pattern` for the WS flow
    /// and directly by tests to verify state-clearing without a tokio runtime.
    fn select_pattern_inner(&mut self, pattern: String) {
        self.phase = ChatPhase::Chatting;
        self.thread = Some(pattern);
        self.editor = empty_chat_editor();
        self.scroll = 0;
        self.messages.clear();
        self.input_history.clear();
        self.history_pos = None;
        self.last_hydrated_key = None;
    }

    pub(super) fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            ChatFocus::ChatPane => ChatFocus::ActivityPane,
            ChatFocus::ActivityPane => ChatFocus::ChatPane,
        };
    }

    /// Cycle the activity pane size. Replaces the legacy `Ctrl+W` behavior.
    /// 0 (hidden) → 1 (bottom 20%) → 2 (bottom 80%) → 3 (activity-only) → 0.
    pub(super) fn cycle_activity(&mut self) {
        self.activity_split = (self.activity_split + 1) % 4;
    }

    /// Toggle zen mode. Zen mode hides the thread info pane, the bottom
    /// status bar, and any visible activity pane. Exiting zen mode
    /// restores the thread info pane and status bar only — the activity
    /// pane stays hidden until the user re-opens it via `Ctrl+A`.
    pub(super) fn toggle_zen_mode(&mut self) {
        let was_info_visible = self.info_visible;
        // Hide auxiliary UI unconditionally.
        self.info_visible = false;
        self.activity_split = 0;
        // If anything was visible, we're now in zen mode (exit).
        // Otherwise restore info+status.
        if !was_info_visible {
            self.info_visible = true;
        }
    }

    pub(super) fn scroll_up(&mut self) {
        match self.focus {
            ChatFocus::ChatPane => self.scroll = self.scroll.saturating_add(1),
            ChatFocus::ActivityPane => {
                self.activity_scroll = self.activity_scroll.saturating_add(1)
            }
        }
    }

    /// Advance the `gg` key-sequence state machine.
    ///
    /// Returns `true` when `pressed_g` completes the sequence (the previous
    /// key was also `g`); the caller should then jump to the top. Any non-`g`
    /// key resets the state.
    pub(super) fn gg_step(&mut self, pressed_g: bool) -> bool {
        let jump = self.pending_g && pressed_g;
        self.pending_g = pressed_g && !jump;
        jump
    }

    pub(super) fn scroll_down(&mut self) {
        match self.focus {
            ChatFocus::ChatPane => self.scroll = self.scroll.saturating_sub(1),
            ChatFocus::ActivityPane => {
                self.activity_scroll = self.activity_scroll.saturating_sub(1)
            }
        }
    }

    /// Jump to the oldest message (top) of the focused pane.
    ///
    /// The offset is clamped to the actual maximum during rendering, so
    /// setting it to `usize::MAX` is a safe "scroll all the way up".
    pub(super) fn scroll_to_top(&mut self) {
        match self.focus {
            ChatFocus::ChatPane => self.scroll = usize::MAX,
            ChatFocus::ActivityPane => self.activity_scroll = usize::MAX,
        }
    }

    /// Jump to the latest message (bottom) of the focused pane.
    pub(super) fn scroll_to_bottom(&mut self) {
        match self.focus {
            ChatFocus::ChatPane => self.scroll = 0,
            ChatFocus::ActivityPane => self.activity_scroll = 0,
        }
    }

    pub(super) fn page_size(&self) -> usize {
        let base = crossterm::terminal::size()
            .map(|(_, h)| h.saturating_sub(7) as usize)
            .unwrap_or(10);
        match self.focus {
            ChatFocus::ChatPane => {
                let term_width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
                // Editor rows: wrapped text lines (1-10).
                // Subtract the 2-column "> " prompt gutter from the width.
                let input_lines =
                    count_wrapped_lines(&self.text(), term_width.saturating_sub(2)).clamp(1, 10);
                base.saturating_sub(input_lines).max(1)
            }
            ChatFocus::ActivityPane => base.max(1),
        }
    }

    pub(super) fn page_up(&mut self) {
        let page = self.page_size();
        match self.focus {
            ChatFocus::ChatPane => self.scroll = self.scroll.saturating_add(page),
            ChatFocus::ActivityPane => {
                self.activity_scroll = self.activity_scroll.saturating_add(page)
            }
        }
    }

    pub(super) fn page_down(&mut self) {
        let page = self.page_size();
        match self.focus {
            ChatFocus::ChatPane => self.scroll = self.scroll.saturating_sub(page),
            ChatFocus::ActivityPane => {
                self.activity_scroll = self.activity_scroll.saturating_sub(page)
            }
        }
    }

    /// Current chat input text (editor lines joined with newlines).
    pub(super) fn text(&self) -> String {
        self.editor.lines.to_string()
    }

    pub(super) fn send_message(&mut self) {
        let text = self.text().trim().to_string();
        self.send_message_inner(text);
        // Normal send clears the editor input field.
        self.editor = empty_chat_editor();
    }

    /// Send a programmatic text as a chat message, echoing locally and sending
    /// via WebSocket.
    pub(super) fn send_message_inner(&mut self, text: String) {
        if text.is_empty() {
            return;
        }

        // Record sent text in input history (newest last, capped at 100).
        self.input_history.push(text.clone());
        if self.input_history.len() > 100 {
            self.input_history.remove(0);
        }

        // WebSocket-only flow: echo user message locally, send via WebSocket.
        let _ = self.thread.as_ref(); // thread must be set before send
        self.messages.push(ChatMessage {
            sender: "user".to_string(),
            text: text.clone(),
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
        });
        // The /ws/<channel>/<thread> URL already carries the thread name.
        // Both ScopedWsHandler (websocket channel) and ThreadProxyHandler
        // (any other channel) bind the thread from the URL, so the payload
        // doesn't need a `thread` field.
        let msg = serde_json::json!({
            "type": "message",
            "text": text,
        })
        .to_string();
        if let Some(tx) = &self.ws_tx {
            let _ = tx.send(msg);
        }

        self.scroll = 0;
        self.awaiting_response = true;
    }

    pub(super) fn handle_ws_message(&mut self, text: &str) {
        let parsed: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };

        // The new ThreadProxyHandler / ScopedWsHandler publish these events
        // on the inspect-broadcast bus. Forward them to the live buffers
        // (activity pane, chat progress, chat message stream).
        let event_type = parsed.get("type").and_then(|v| v.as_str());
        match event_type {
            Some("activity") | Some("chat_message") | Some("thinking") | Some("processing")
            | Some("resync") => {
                self.handle_live_event(&parsed);
            }
            _ => {}
        }

        // The legacy WebsocketInboundAdapter per-channel broadcast_tx
        // used to carry `reply` events for AI messages. Now all AI
        // messages for both channel types arrive via inspect_broadcast
        // as `chat_message` events (handled above by handle_live_event)
        // — the `reply` path below has been removed to eliminate
        // duplicates between the two event sources.
        //
        // `list_patterns` and `subscribe` moved to REST; `history` is
        // no longer needed (REST `get_thread_chat`).
    }

    /// Recall an older entry from input history into the editor.
    /// Bounded by the oldest (first) entry.
    pub(super) fn recall_older(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let pos = self.history_pos.map_or(self.input_history.len(), |p| p);
        if pos == 0 {
            return; // Already at oldest
        }
        let new_pos = pos - 1;
        self.editor = EditorState::new(Lines::from(self.input_history[new_pos].as_str()));
        self.editor.mode = EditorMode::Insert;
        self.history_pos = Some(new_pos);
    }

    /// Recall a newer entry from input history into the editor.
    /// At the newest, clears the editor and exits history mode.
    pub(super) fn recall_newer(&mut self) {
        match self.history_pos {
            Some(pos) if pos + 1 < self.input_history.len() => {
                let new_pos = pos + 1;
                self.editor = EditorState::new(Lines::from(self.input_history[new_pos].as_str()));
                self.editor.mode = EditorMode::Insert;
                self.history_pos = Some(new_pos);
            }
            _ => {
                // No newer entry or not browsing — clear back to empty
                self.editor = empty_chat_editor();
                self.history_pos = None;
            }
        }
    }

    /// Seed the live activity/chat buffers with the result of a REST hydrate
    /// (initial fetch on thread selection). Sets `last_seen_id` to the
    /// highest id seen so duplicate WS events are dropped.
    #[allow(dead_code)]
    pub(super) fn seed_live(
        &mut self,
        channel: &str,
        thread: &str,
        activity: Vec<jyc_types::ActivityEntry>,
        chat: Vec<jyc_types::ChatMessageEntry>,
    ) {
        let key = (channel.to_string(), thread.to_string());
        let mut max_id = 0u64;
        let activity_buf: std::collections::VecDeque<_> = activity
            .into_iter()
            .inspect(|e| {
                if e.id > max_id {
                    max_id = e.id;
                }
            })
            .collect();
        let chat_buf: std::collections::VecDeque<_> = chat
            .into_iter()
            .inspect(|e| {
                if e.id > max_id {
                    max_id = e.id;
                }
            })
            .collect();
        // Cap buffer sizes to match the in-memory cap in jyc-inspect.
        const MAX_ACTIVITY: usize = 180;
        const MAX_CHAT: usize = 50;
        let mut activity_buf = activity_buf;
        while activity_buf.len() > MAX_ACTIVITY {
            activity_buf.pop_front();
        }
        let mut chat_buf = chat_buf;
        while chat_buf.len() > MAX_CHAT {
            chat_buf.pop_front();
        }
        self.live_activity.insert(key.clone(), activity_buf);
        self.live_chat.insert(key.clone(), chat_buf);
        self.last_seen_id.insert(key, max_id);
    }

    /// Handle a `{"type":"resync", "channel":..., "thread":...}` event by
    /// clearing the live buffers for that thread. The caller should re-run
    /// the REST hydrate (`get_thread_activity` + `get_thread_chat`) and
    /// re-seed via `seed_live`.
    #[allow(dead_code)]
    pub(super) fn clear_live(&mut self, channel: &str, thread: &str) {
        let key = (channel.to_string(), thread.to_string());
        self.live_activity.remove(&key);
        self.live_chat.remove(&key);
        self.live_thinking.remove(&key);
        self.live_processing.remove(&key);
        self.last_seen_id.remove(&key);
    }

    /// Handle a parsed `{"type":"activity",...}` or similar WS payload.
    /// Filters out duplicate / older events using `last_seen_id`.
    #[allow(dead_code)]
    pub(super) fn handle_live_event(&mut self, payload: &serde_json::Value) {
        let channel = match payload.get("channel").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => return,
        };
        let thread = match payload.get("thread").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => return,
        };
        let key = (channel.clone(), thread.clone());
        let id = payload.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let last = self.last_seen_id.get(&key).copied().unwrap_or(0);
        if id != 0 && id <= last {
            return; // duplicate or older
        }
        if id != 0 {
            self.last_seen_id.insert(key.clone(), id);
        }

        let event_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            "activity" => {
                if let Some(entry) = payload.get("entry").and_then(|v| {
                    serde_json::from_value::<jyc_types::ActivityEntry>(v.clone()).ok()
                }) {
                    let buf = self.live_activity.entry(key).or_default();
                    buf.push_back(entry);
                    if buf.len() > 180 {
                        buf.pop_front();
                    }
                }
            }
            "chat_message" => {
                if let Some(entry) = payload.get("entry").and_then(|v| {
                    serde_json::from_value::<jyc_types::ChatMessageEntry>(v.clone()).ok()
                }) {
                    // AI reply delivered — clear local waiting flag so the
                    // progress indicator disappears immediately instead of
                    // waiting for the next poll cycle.
                    if entry.sender == "ai" {
                        self.awaiting_response = false;
                    }
                    let buf = self.live_chat.entry(key).or_default();
                    buf.push_back(entry);
                    if buf.len() > 50 {
                        buf.pop_front();
                    }
                }
            }
            "thinking" => {
                if let Some(text) = payload.get("text").and_then(|v| v.as_str()) {
                    self.live_thinking.insert(key, text.to_string());
                }
            }
            "processing" => {
                let is_processing = payload
                    .get("is_processing")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let has_error = payload
                    .get("has_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.live_processing
                    .insert(key.clone(), (is_processing, has_error));
                if !is_processing {
                    // Processing completed - clear per-round transient
                    // artifacts but keep live_activity as the audit trail
                    // across rounds. Buffer is bounded at 180 entries.
                    self.live_thinking.remove(&key);
                    self.awaiting_response = false;
                } else {
                    // New round started - also clear thinking (in case
                    // the first Thinking event for this round is delayed).
                    self.live_thinking.remove(&key);
                }
            }
            "resync" => {
                // Server fell behind (Lagged); clear local state so the
                // caller re-hydrates via REST.
                self.live_activity.remove(&key);
                self.live_chat.remove(&key);
                self.live_thinking.remove(&key);
                self.live_processing.remove(&key);
                self.last_seen_id.remove(&key);
            }
            _ => {}
        }
    }

    /// Get a snapshot of the live activity buffer for the given (channel, thread).
    /// Returns an empty slice if no live data has been seeded yet.
    #[allow(dead_code)]
    pub(super) fn live_activity_for(
        &self,
        channel: &str,
        thread: &str,
    ) -> std::collections::vec_deque::Iter<'_, jyc_types::ActivityEntry> {
        self.live_activity
            .get(&(channel.to_string(), thread.to_string()))
            .map(|v| v.iter())
            .unwrap_or_else(|| EMPTY_VEC_DEQUE.iter())
    }

    /// Get the current thinking text for the given (channel, thread), if any.
    pub(super) fn live_thinking_for(&self, channel: &str, thread: &str) -> Option<&str> {
        self.live_thinking
            .get(&(channel.to_string(), thread.to_string()))
            .map(|s| s.as_str())
    }

    /// Get the current processing status for the given (channel, thread).
    /// Returns `None` if no status has been received yet (fall back to polled state).
    pub(super) fn live_processing_for(&self, channel: &str, thread: &str) -> Option<(bool, bool)> {
        self.live_processing
            .get(&(channel.to_string(), thread.to_string()))
            .copied()
    }
    /// Iterate over the live chat messages for the given (channel, thread).
    /// Used by the dashboard's poll loop to append new messages to the
    /// `chat.messages` vec shown in the chat pane.
    #[allow(dead_code)]
    pub(super) fn live_chat_for(
        &self,
        channel: &str,
        thread: &str,
    ) -> std::collections::vec_deque::Iter<'_, jyc_types::ChatMessageEntry> {
        self.live_chat
            .get(&(channel.to_string(), thread.to_string()))
            .map(|v| v.iter())
            .unwrap_or_else(|| EMPTY_CHAT_DEQUE.iter())
    }
}

/// Static empty deque used as a fallback when no live data is seeded for a
/// (channel, thread) — lets us return a concrete `Iter` from the accessors.
static EMPTY_VEC_DEQUE: std::sync::LazyLock<std::collections::VecDeque<jyc_types::ActivityEntry>> =
    std::sync::LazyLock::new(std::collections::VecDeque::new);
static EMPTY_CHAT_DEQUE: std::sync::LazyLock<
    std::collections::VecDeque<jyc_types::ChatMessageEntry>,
> = std::sync::LazyLock::new(std::collections::VecDeque::new);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_pattern_clears_chat_messages() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        // Simulate messages from a previous thread
        app.chat.messages.push(ChatMessage {
            sender: "user".to_string(),
            text: "hello from thread A".to_string(),
            timestamp: None,
        });
        app.chat.messages.push(ChatMessage {
            sender: "ai".to_string(),
            text: "reply from thread A".to_string(),
            timestamp: None,
        });
        assert_eq!(app.chat.messages.len(), 2);

        // Switch to a new thread
        app.chat.select_pattern_inner("thread-b".to_string());

        // Messages must be cleared so stale content doesn't leak across threads
        assert!(app.chat.messages.is_empty());
        assert_eq!(app.chat.thread.as_deref(), Some("thread-b"));
    }

    #[test]
    fn scroll_to_top_and_bottom_follow_focus() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        // Chat pane focused
        app.chat.focus = ChatFocus::ChatPane;
        app.chat.scroll_to_top();
        assert_eq!(app.chat.scroll, usize::MAX);
        assert_eq!(app.chat.activity_scroll, 0);
        app.chat.scroll_to_bottom();
        assert_eq!(app.chat.scroll, 0);

        // Activity pane focused
        app.chat.focus = ChatFocus::ActivityPane;
        app.chat.scroll_to_top();
        assert_eq!(app.chat.activity_scroll, usize::MAX);
        assert_eq!(app.chat.scroll, 0);
        app.chat.scroll_to_bottom();
        assert_eq!(app.chat.activity_scroll, 0);
    }

    #[test]
    fn gg_step_completes_only_on_consecutive_g() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        // Single `g` arms the sequence without jumping
        assert!(!app.chat.gg_step(true));
        assert!(app.chat.pending_g);
        // Second consecutive `g` completes the jump and resets
        assert!(app.chat.gg_step(true));
        assert!(!app.chat.pending_g);
        // Third `g` starts a fresh sequence
        assert!(!app.chat.gg_step(true));
        assert!(app.chat.pending_g);
        // A non-`g` key resets the sequence
        assert!(!app.chat.gg_step(false));
        assert!(!app.chat.pending_g);
        // `g` after reset does not jump
        assert!(!app.chat.gg_step(true));
        assert!(app.chat.pending_g);
    }

    #[test]
    fn recall_older_on_empty_history_does_nothing() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        assert!(app.chat.input_history.is_empty());
        app.chat.recall_older(); // should not panic or change anything
        assert!(app.chat.history_pos.is_none());
    }

    #[test]
    fn recall_older_recalls_and_recall_newer_clears() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        app.chat.input_history = vec![
            "first msg".to_string(),
            "second msg".to_string(),
            "third msg".to_string(),
        ];

        // Up x3: third → second → first → stays at first
        // Note: recall_older operates on the full history; it starts from newest (pos=len).
        // Initial press: len=3 → pos=2 → "third msg"
        app.chat.recall_older();
        assert_eq!(app.chat.history_pos, Some(2));
        assert_eq!(app.chat.text(), "third msg");

        // Next older: pos 2 → 1 → "second msg"
        app.chat.recall_older();
        assert_eq!(app.chat.history_pos, Some(1));
        assert_eq!(app.chat.text(), "second msg");

        // Next older: pos 1 → 0 → "first msg"
        app.chat.recall_older();
        assert_eq!(app.chat.history_pos, Some(0));
        assert_eq!(app.chat.text(), "first msg");

        // Already at oldest — no change
        app.chat.recall_older();
        assert_eq!(app.chat.history_pos, Some(0));
        assert_eq!(app.chat.text(), "first msg");

        // Down: pos 0 → 1 → "second msg"
        app.chat.recall_newer();
        assert_eq!(app.chat.history_pos, Some(1));
        assert_eq!(app.chat.text(), "second msg");

        // Down: pos 1 → 2 → "third msg"
        app.chat.recall_newer();
        assert_eq!(app.chat.history_pos, Some(2));
        assert_eq!(app.chat.text(), "third msg");

        // Down at newest — clears to empty
        app.chat.recall_newer();
        assert!(app.chat.history_pos.is_none());
        assert!(app.chat.text().is_empty());
    }

    #[test]
    fn select_pattern_clears_input_history() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        app.chat.input_history = vec!["msg from thread A".to_string()];
        app.chat.history_pos = Some(0);

        // Switch to a new thread
        app.chat.select_pattern_inner("thread-b".to_string());

        // History must be cleared so it doesn't leak across threads
        assert!(app.chat.input_history.is_empty());
        assert!(app.chat.history_pos.is_none());
    }

    #[test]
    fn close_returns_to_overview_from_ws_chat() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        // Simulate post-open WS chat state (what Enter on a WS row produces).
        // We set fields directly instead of calling open() because open()
        // spawns a tokio task requiring a runtime.
        app.chat.visible = true;
        app.chat.phase = ChatPhase::Chatting;
        app.chat.thread = Some("jyc".to_string());
        app.chat.focus = ChatFocus::ChatPane;
        app.chat.detail_channel = None;
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        app.chat.ws_tx = Some(cmd_tx);

        assert!(app.chat.visible);
        assert_eq!(app.chat.phase, ChatPhase::Chatting);
        assert_eq!(app.chat.thread.as_deref(), Some("jyc"));
        assert!(!app.chat.is_detail_mode());

        // close() is what Esc invokes — must return to overview
        app.chat.close();
        assert!(!app.chat.visible);
        assert_eq!(app.chat.phase, ChatPhase::PatternSelect);
        assert!(app.chat.detail_channel.is_none());
        assert!(app.chat.ws_tx.is_none());
    }

    #[test]
    fn close_returns_to_overview_from_detail_mode() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        // Simulate opening a non-WS thread in detail mode
        app.chat.open_thread_detail("github", "issue-197", None);
        assert!(app.chat.visible);
        assert_eq!(app.chat.phase, ChatPhase::Chatting);

        // close() must return to overview and clear detail state
        app.chat.close();
        assert!(!app.chat.visible);
        assert!(app.chat.detail_channel.is_none());
        assert!(app.chat.detail_thread_path.is_none());
    }

    #[test]
    fn wrap_short_text_returns_one_line() {
        let out = wrap_text_to_width("hello", 80);
        assert_eq!(out, vec!["hello".to_string()]);
    }

    #[test]
    fn wrap_long_ascii_text_breaks_at_width() {
        // 30 chars, max width 10 → expect 3 wrapped rows
        let text = "abcdefghijklmnopqrstuvwxyz0123";
        let out = wrap_text_to_width(text, 10);
        assert_eq!(out.len(), 3);
        // Each row should not exceed 10 display columns
        for row in &out {
            assert!(
                row.width() <= 10,
                "row {:?} is {} cols, exceeds 10",
                row,
                row.width()
            );
        }
        // The joined output must reconstruct the original (no chars lost)
        let joined: String = out.join("");
        assert_eq!(joined, text);
    }

    #[test]
    fn wrap_preserves_explicit_newlines_and_blank_lines() {
        let text = "first line\nsecond line\n\nfourth line";
        let out = wrap_text_to_width(text, 80);
        assert_eq!(
            out,
            vec![
                "first line".to_string(),
                "second line".to_string(),
                "".to_string(),
                "fourth line".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_wide_unicode_counts_two_columns_per_char() {
        // Each CJK char is 2 cols. With max_width=4, each pair fits exactly.
        let text = "你好你好";
        let out = wrap_text_to_width(text, 4);
        assert_eq!(out, vec!["你好".to_string(), "你好".to_string()]);
    }

    #[test]
    fn wrap_max_width_zero_clamps_to_one() {
        // Should not panic on zero-width panes and should still emit every char.
        let out = wrap_text_to_width("abc", 0);
        let joined: String = out.join("");
        assert_eq!(joined, "abc");
    }

    #[test]
    fn handle_ws_message_routes_activity_events_to_live_buffer() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        // Simulate hydrate: seed an activity entry, then a WS event with
        // the same id arrives — should be deduped (id <= last_seen_id).
        let entry = jyc_types::ActivityEntry {
            text: "Tool: bash".to_string(),
            timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            severity: jyc_types::Severity::Info,
            id: 42,
            is_internal: false,
        };
        app.chat.seed_live("github", "pr-1", vec![entry], vec![]);
        assert_eq!(app.chat.live_activity_for("github", "pr-1").count(), 1);

        // WS event with NEW id should be appended.
        let payload = serde_json::json!({
            "type": "activity",
            "channel": "github",
            "thread": "pr-1",
            "id": 43,
            "entry": {
                "text": "Completed",
                "timestamp": "2026-01-01T00:00:05Z",
                "severity": "info",
                "id": 0,
            }
        });
        app.chat.handle_ws_message(&payload.to_string());
        assert_eq!(app.chat.live_activity_for("github", "pr-1").count(), 2);

        // WS event with OLD id should be deduped.
        let payload = serde_json::json!({
            "type": "activity",
            "channel": "github",
            "thread": "pr-1",
            "id": 42,
            "entry": {
                "text": "Old",
                "timestamp": "2026-01-01T00:00:00Z",
                "severity": "info",
                "id": 0,
            }
        });
        app.chat.handle_ws_message(&payload.to_string());
        assert_eq!(app.chat.live_activity_for("github", "pr-1").count(), 2);
    }

    #[test]
    fn handle_ws_message_routes_chat_message_to_live_buffer() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        let payload = serde_json::json!({
            "type": "chat_message",
            "channel": "github",
            "thread": "pr-1",
            "id": 1,
            "entry": {
                "sender": "ai",
                "text": "Hello",
                "timestamp": "2026-01-01T00:00:00Z",
                "id": 0,
            }
        });
        app.chat.handle_ws_message(&payload.to_string());
        assert_eq!(app.chat.live_chat_for("github", "pr-1").count(), 1);
    }

    #[test]
    fn handle_ws_message_routes_thinking_to_live_buffer() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        let payload = serde_json::json!({
            "type": "thinking",
            "channel": "github",
            "thread": "pr-1",
            "text": "I am thinking about the problem"
        });
        app.chat.handle_ws_message(&payload.to_string());
        assert_eq!(
            app.chat.live_thinking_for("github", "pr-1"),
            Some("I am thinking about the problem")
        );
    }

    #[test]
    fn handle_ws_message_routes_resync_clears_buffer() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        // Seed first
        app.chat.seed_live(
            "github",
            "pr-1",
            vec![jyc_types::ActivityEntry {
                text: "old".to_string(),
                timestamp: Some("2026-01-01T00:00:00Z".to_string()),
                severity: jyc_types::Severity::Info,
                id: 1,
                is_internal: false,
            }],
            vec![],
        );
        assert_eq!(app.chat.live_activity_for("github", "pr-1").count(), 1);

        // Resync event should clear the live buffer
        let payload = serde_json::json!({
            "type": "resync",
            "channel": "github",
            "thread": "pr-1",
            "dropped": 5
        });
        app.chat.handle_ws_message(&payload.to_string());
        assert_eq!(app.chat.live_activity_for("github", "pr-1").count(), 0);
    }

    #[test]
    fn is_user_visible_activity_filters_internal_and_thinking() {
        use jyc_types::ActivityEntry;
        use jyc_types::Severity;

        let visible = ActivityEntry {
            text: "Tool: bash (done, 1s)".to_string(),
            timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            severity: Severity::Info,
            id: 1,
            is_internal: false,
        };
        assert!(is_user_visible_activity(&visible));

        // New flag: ProcessingProgress events (is_internal=true) hidden.
        let internal = ActivityEntry {
            text: "tool execution (10s, 200 chars)".to_string(),
            timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            severity: Severity::Info,
            id: 2,
            is_internal: true,
        };
        assert!(!is_user_visible_activity(&internal));

        // Legacy: text shape for ProcessingProgress.
        let legacy = ActivityEntry {
            text: "tool execution (5s, 120 chars)".to_string(),
            timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            severity: Severity::Info,
            id: 3,
            is_internal: false,
        };
        assert!(!is_user_visible_activity(&legacy));
    }

    #[test]
    fn command_popup_send_preserves_editor_text() {
        // Regression: the PopupAction::Send arm used to populate the editor
        // with the selected command and then call `send_message()`, which
        // cleared the editor (`self.editor = empty_chat_editor()` inside
        // `send_message`), wiping any pre-existing text. It now routes
        // through `send_message_inner`, which never touches the editor —
        // so the editor keeps whatever was there before the popup opened.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        // Simulate pre-existing Normal-mode-style editor text — the case
        // the bug actually broke.
        app.chat.populate_editor("draft message");
        let pre_text = app.chat.text();
        assert_eq!(pre_text, "draft message");

        // Mirror the fixed PopupAction::Send handler (chat.rs:366-368).
        app.chat.command_popup = None;
        app.chat.send_message_inner("/model gpt-4".to_string());

        // Editor must be preserved.
        assert_eq!(app.chat.text(), pre_text, "editor must not be cleared");

        // Send-side effects still fire.
        assert_eq!(app.chat.messages.len(), 1);
        assert_eq!(app.chat.messages[0].sender, "user");
        assert_eq!(app.chat.messages[0].text, "/model gpt-4");
        assert_eq!(
            app.chat.input_history.last(),
            Some(&"/model gpt-4".to_string())
        );
        assert!(app.chat.awaiting_response);
    }

    #[test]
    fn command_popup_send_on_empty_editor_stays_empty() {
        // Insert-mode popup path: editor must be empty before the popup
        // opens (per the gating at chat.rs:385-390). After the Send arm
        // fires, the editor should still be empty — i.e. nothing was
        // populated and nothing was cleared (the trivially-empty case).
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        assert!(app.chat.text().is_empty());

        app.chat.command_popup = None;
        app.chat.send_message_inner("/plan".to_string());

        assert!(app.chat.text().is_empty(), "editor must stay empty");
        assert_eq!(app.chat.messages.last().unwrap().text, "/plan");
    }

    #[test]
    fn opens_with_info_and_activity_hidden() {
        // All visibility flags default to hidden when a new ChatState is
        // constructed. Mirrors the "borderless chat, no chrome" UX.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let app = App::new(rx, None);
        assert!(!app.chat.info_visible);
        assert_eq!(app.chat.activity_split, 0);
    }

    #[test]
    fn cycle_activity_rotates_through_four_states() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        // 0 (hidden) → 1 (bottom 20%) → 2 (bottom 80%) → 3 (activity-only) → 0
        assert_eq!(app.chat.activity_split, 0);
        app.chat.cycle_activity();
        assert_eq!(app.chat.activity_split, 1);
        app.chat.cycle_activity();
        assert_eq!(app.chat.activity_split, 2);
        app.chat.cycle_activity();
        assert_eq!(app.chat.activity_split, 3);
        app.chat.cycle_activity();
        assert_eq!(app.chat.activity_split, 0);
    }

    #[test]
    fn zen_mode_hides_info_and_activity() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        // Start in zen mode (info hidden, activity hidden).
        assert!(!app.chat.info_visible);
        assert_eq!(app.chat.activity_split, 0);

        // Press Ctrl+Z → exit zen mode: info+status visible, activity still hidden.
        app.chat.toggle_zen_mode();
        assert!(app.chat.info_visible);
        assert_eq!(app.chat.activity_split, 0);

        // User opens activity via Ctrl+A. Now both info and activity are visible.
        app.chat.cycle_activity();
        assert_eq!(app.chat.activity_split, 1);
        assert!(app.chat.info_visible);

        // Press Ctrl+Z → enter zen mode: info hidden AND activity hidden,
        // regardless of its current size.
        app.chat.toggle_zen_mode();
        assert!(!app.chat.info_visible);
        assert_eq!(app.chat.activity_split, 0);

        // Press Ctrl+Z again → exit zen mode: info+status restored, activity
        // stays hidden (not auto-restored).
        app.chat.toggle_zen_mode();
        assert!(app.chat.info_visible);
        assert_eq!(app.chat.activity_split, 0);
    }

    #[test]
    fn cycle_resets_after_zen_mode() {
        // Regression: after Ctrl+Z hides the activity pane, the next Ctrl+A
        // should restart the cycle from the 20% bottom size, not from
        // wherever activity was previously.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        // Drive activity to "activity-only" (size 3).
        app.chat.cycle_activity();
        app.chat.cycle_activity();
        app.chat.cycle_activity();
        assert_eq!(app.chat.activity_split, 3);
        // Enter zen mode — activity is reset to 0.
        app.chat.toggle_zen_mode();
        assert_eq!(app.chat.activity_split, 0);
        // First Ctrl+A after zen mode must reach the 20% size.
        app.chat.cycle_activity();
        assert_eq!(app.chat.activity_split, 1);
    }
}
