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
    pub(super) messages: Vec<ChatMessage>,
    /// Recent activity entries for the activity pane. Initially loaded via
    /// `GET /thread/.../activity` and kept in sync via WebSocket push
    /// (`Tool`, `Process` events).
    pub(super) activity_messages: Vec<jyc_types::inspect::ActivityEntry>,
    /// Latest AI thinking/reasoning text, set via WebSocket `Thinking` push.
    /// Cleared when the thread finishes processing or the user navigates away.
    pub(super) thinking_text: Option<String>,
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
    /// Activity pane split state: 0=100/0, 1=80/20, 2=20/80, 3=0/100
    pub(super) activity_split: u8,
    pub(super) ws_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub(super) ws_rx: tokio::sync::mpsc::UnboundedReceiver<WsEvent>,
    pub(super) ws_connected: bool,
    /// Whether we've sent a subscribe message for the current thread this
    /// session. Reset on `open()` (new thread / new WS) and on WS disconnect;
    /// set after `subscribe_to_active_thread()`. The `WsEvent::Connected`
    /// handler only re-subscribes when this is false, which is what we want:
    /// `open()` subscribes on initial entry, `Connected` handles reconnects.
    pub(super) subscribed: bool,
    // Detail mode (non-WebSocket thread chat) state
    /// Channel name for the thread being viewed in detail mode.
    /// Set when Enter is pressed on a non-websocket thread.
    pub(super) detail_channel: Option<String>,
    /// Thread path from ThreadInfo (for loading chat history from disk).
    pub(super) detail_thread_path: Option<std::path::PathBuf>,
    /// Pending message to inject via inspect protocol (detail mode).
    /// Set by `send_message_inner` and consumed by the main poll loop.
    pub(super) pending_inject: Option<(String, String)>,
    // Command popup state
    pub(super) commands: Vec<CommandInfo>,
    pub(super) models: Vec<ModelInfo>,
    pub(super) command_popup: Option<CommandPopupState>,
    /// History of sent messages for Up/Down recall (newest appended last).
    pub(super) input_history: Vec<String>,
    /// Current position in history browsing (None = not browsing).
    pub(super) history_pos: Option<usize>,
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

    // Ctrl+W cycles activity pane split ratio
    let is_ctrl_w = key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_w && app.chat.phase == ChatPhase::Chatting {
        app.chat.activity_split = (app.chat.activity_split + 1) % 4;
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
                app.chat.populate_editor(&cmd);
                // ponytail: overwriting the editor (vs. send_message_inner
                // directly) preserves the post-send empty-editor invariant;
                // revisit if Normal-mode stale-text preservation becomes a
                // real case.
                app.chat.send_message();
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
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Channels bar
            Constraint::Length(1), // Compact info bar
            Constraint::Min(0),    // Content (chat + optional activity)
            Constraint::Length(1), // Status bar
        ])
        .split(area);

    render_channels(frame, main_chunks[0], app);
    render_compact_info(frame, main_chunks[1], app);

    match app.chat.phase {
        ChatPhase::PatternSelect => {
            render_pattern_select(frame, main_chunks[2], app);
        }
        ChatPhase::Chatting => {
            match app.chat.activity_split {
                1 => {
                    // 80/20 — chat dominant
                    let content = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(80), Constraint::Percentage(20)])
                        .split(main_chunks[2]);
                    render_chat_conversation(frame, content[0], app);
                    render_activity_log(frame, content[1], app);
                }
                2 => {
                    // 20/80 — activity dominant
                    let content = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(20), Constraint::Percentage(80)])
                        .split(main_chunks[2]);
                    render_chat_conversation(frame, content[0], app);
                    render_activity_log(frame, content[1], app);
                }
                3 => {
                    // 0/100 — full activity
                    render_activity_log(frame, main_chunks[2], app);
                }
                _ => {
                    // 0 — 100/0 (default) — full chat, no activity pane
                    render_chat_conversation(frame, main_chunks[2], app);
                }
            }
        }
    }

    render_status_bar(frame, main_chunks[3], app);
}

pub(super) fn render_compact_info(frame: &mut Frame, area: Rect, app: &App) {
    let state = match &app.state {
        Some(s) => s,
        None => {
            let text = Paragraph::new("");
            frame.render_widget(text, area);
            return;
        }
    };

    let selected = if app.chat.visible && app.chat.phase == ChatPhase::Chatting {
        app.chat
            .thread
            .as_ref()
            .and_then(|chat_name| state.threads.iter().find(|t| t.name == *chat_name))
    } else {
        app.table_state
            .selected()
            .and_then(|i| state.threads.get(i))
    };

    let text = if let Some(t) = selected {
        let mut spans = vec![
            Span::styled("Thread: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&t.name),
            Span::raw(" | "),
            Span::styled("Channel: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&t.channel),
            Span::raw(" | "),
            Span::styled("Pattern: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(t.pattern.as_deref().unwrap_or("-")),
        ];
        if let Some(ref model) = t.model {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                "Model: ",
                Style::default().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(model));
        }
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            "Mode: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(t.mode.as_deref().unwrap_or("build")));
        if let (Some(cur), Some(max)) = (t.input_tokens, t.max_tokens) {
            let pct = if max > 0 {
                cur.checked_mul(100)
                    .and_then(|v| v.checked_div(max))
                    .unwrap_or(0)
            } else {
                0
            };
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                "Tokens: ",
                Style::default().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(format!("{cur} / {max} ({pct}%)")));
        }
        if t.status == ThreadStatus::Processing {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                "⏳ AI thinking...",
                Style::default().fg(Color::Yellow),
            ));
        }
        Line::from(spans)
    } else {
        Line::from("Select a thread with ↑/↓")
    };

    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, area);
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
    let title = format!(" Chat: {} ", app.chat.thread.as_deref().unwrap_or("-"));
    let mut block = Block::default().title(title).borders(Borders::ALL);
    if app.chat.focus == ChatFocus::ChatPane {
        block = block.border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split: scrollable messages (top) + dynamic input area (bottom)
    // Input area grows with content (up to 10 rows) for multi-line editing.
    // Subtract the 2-column "> " prompt gutter from the wrap width.
    let input_line_count =
        count_wrapped_lines(&app.chat.text(), inner.width.saturating_sub(2)).clamp(1, 10) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(input_line_count)])
        .split(inner);

    // --- Messages area (markdown-rendered with colored bars) ---
    let renderer = ratatui_markdown::markdown::MarkdownRenderer::new(chunks[0].width as usize);
    let theme = ratatui_markdown::theme::ThemeConfig::default();

    let mut all_lines: Vec<Line> = Vec::new();

    let dim_style = Style::default().fg(Color::DarkGray);
    let mut box_open = false;
    let mut group_start_ts: Option<String> = None;

    for (idx, msg) in app.chat.messages.iter().enumerate() {
        let is_user = msg.sender == "user";
        let prefix = if is_user { "**You:** " } else { "**AI:** " };

        let prev_sender = if idx > 0 {
            Some(app.chat.messages[idx - 1].sender.as_str())
        } else {
            None
        };

        // Close previous group box on AI → user transition
        if is_user && prev_sender == Some("ai") && box_open {
            all_lines.push(Line::from(vec![Span::styled("│", dim_style)]));
            let last_ts = app
                .chat
                .messages
                .get(idx - 1)
                .and_then(|m| m.timestamp.clone());
            let elapsed = format_group_elapsed(&group_start_ts, &last_ts);
            let width = chunks[0].width as usize;
            let close_spans = if elapsed.is_empty() {
                let dashes = "─".repeat(width.saturating_sub(1));
                vec![Span::styled(format!("╰{dashes}"), dim_style)]
            } else {
                // ╰─── 12s ──
                let dash_count = width.saturating_sub(2 + elapsed.len() + 4); // ╰ + " " + elapsed + " " + "──"
                vec![
                    Span::styled(format!("╰{} ", "─".repeat(dash_count)), dim_style),
                    Span::styled(elapsed, dim_style),
                    Span::styled(" ──", dim_style),
                ]
            };
            all_lines.push(Line::from(close_spans));
            all_lines.push(Line::from(""));
            box_open = false;
            group_start_ts = None;
        }

        // Open new group box at the start of a user turn
        if is_user && !box_open {
            group_start_ts = msg.timestamp.clone();
            let time_str = format_msg_time(&msg.timestamp);
            let width = chunks[0].width as usize;
            let open_spans = if time_str.is_empty() {
                let dashes = "─".repeat(width.saturating_sub(2));
                vec![Span::styled(format!("╭─{}", dashes), dim_style)]
            } else {
                let used = 3 + time_str.len() + 1; // "╭─ " + time_str + " "
                let dash_count = width.saturating_sub(used);
                vec![
                    Span::styled("╭─ ", dim_style),
                    Span::styled(time_str, dim_style),
                    Span::styled(format!(" {}", "─".repeat(dash_count)), dim_style),
                ]
            };
            all_lines.push(Line::from(open_spans));
            box_open = true;
        }

        // Separator line between user and AI within a group
        if !is_user && prev_sender == Some("user") {
            let width = chunks[0].width as usize;
            let sep = format!("├{}", "─".repeat(width.saturating_sub(1)));
            let sep_style = Style::default().fg(Color::DarkGray);
            all_lines.push(Line::from(vec![Span::styled(sep, sep_style)]));
        }

        // Render message: all bars use the same dim style
        let bar_style = dim_style;
        let md_text = format!("{prefix}{}\n", msg.text);
        let blocks = renderer.parse(&md_text);
        let msg_lines = renderer.render(&blocks, &theme);

        for line in msg_lines {
            let bar_span = Span::styled("│ ", bar_style);
            let spans: Vec<Span> = std::iter::once(bar_span).chain(line).collect();
            all_lines.push(Line::from(spans));
        }
    }

    // Close any open box at the end
    if box_open {
        all_lines.push(Line::from(vec![Span::styled("│", dim_style)]));
        let last_ts = app.chat.messages.last().and_then(|m| m.timestamp.clone());
        let elapsed = format_group_elapsed(&group_start_ts, &last_ts);
        let width = chunks[0].width as usize;
        let close_spans = if elapsed.is_empty() {
            let dashes = "─".repeat(width.saturating_sub(1));
            vec![Span::styled(format!("╰{dashes}"), dim_style)]
        } else {
            // ╰─── 12s ──
            let dash_count = width.saturating_sub(2 + elapsed.len() + 4);
            vec![
                Span::styled(format!("╰{} ", "─".repeat(dash_count)), dim_style),
                Span::styled(elapsed, dim_style),
                Span::styled(" ──", dim_style),
            ]
        };
        all_lines.push(Line::from(close_spans));
        all_lines.push(Line::from(""));
    }

    // Show progress indicator
    // Determine if the thread is processing: either the inspect server
    // reports Processing status, or we've sent a message and haven't yet
    // seen the server confirm completion (covers the first-message gap
    // where the poll hasn't caught up yet).
    let server_processing = app
        .state
        .as_ref()
        .and_then(|s| {
            let chat_name = app.chat.thread.as_deref()?;
            s.threads.iter().find(|t| t.name == chat_name)
        })
        .is_some_and(|ct| ct.status == ThreadStatus::Processing);

    // Show progress if the server reports processing OR we've sent a message
    // locally and are still waiting for the server state to catch up.
    let show_progress = server_processing || app.chat.awaiting_response;

    if show_progress {
        // Thinking text and activity entries are now pushed live via
        // WebSocket into `app.chat.thinking_text` and
        // `app.chat.activity_messages`. We no longer read them from
        // /state's `ct.activity` / `ct.thinking_text` (those fields are
        // scheduled for removal in step 6 of #438).

        // Show thinking text (if any) from the live-updated state.
        if let Some(thinking) = &app.chat.thinking_text
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

        // Use the live-updated activity buffer (pushed via WebSocket).
        // Take the last 2 entries for a compact "what's happening now" view.
        let activity_entries: Vec<_> = app.chat.activity_messages.iter().rev().take(2).collect();

        let has_thinking_text = app
            .chat
            .thinking_text
            .as_deref()
            .is_some_and(|t| !t.is_empty());

        if activity_entries.is_empty() && !has_thinking_text {
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
            for (idx, a) in activity_entries.iter().rev().enumerate() {
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
        render_command_popup(frame, inner, popup, &app.chat.commands, &app.chat.models);
    }
}

pub(super) fn render_activity_log(frame: &mut Frame, area: Rect, app: &mut App) {
    let state = match &app.state {
        Some(s) => s,
        None => {
            let block = Block::default().title(" Activity ").borders(Borders::ALL);
            frame.render_widget(block, area);
            return;
        }
    };

    let selected = if app.chat.visible && app.chat.phase == ChatPhase::Chatting {
        app.chat
            .thread
            .as_ref()
            .and_then(|chat_name| state.threads.iter().find(|t| t.name == *chat_name))
    } else {
        app.table_state
            .selected()
            .and_then(|i| state.threads.get(i))
    };

    let selected = match selected {
        Some(t) => t,
        None => {
            let block = Block::default().title(" Activity ").borders(Borders::ALL);
            frame.render_widget(block, area);
            return;
        }
    };

    let focused = app.chat.visible && app.chat.focus == ChatFocus::ActivityPane;
    let inner_height = area.height.saturating_sub(2) as usize; // subtract borders
    // Thinking entries are excluded from the activity pane (they flood it with
    // identical "Thinking..." markers). They remain in the in-memory log for
    // the chat pane's AI progress area and are persisted to activity.jsonl.
    let visible_count = selected
        .activity
        .iter()
        .filter(|e| !e.text.starts_with("Thinking: "))
        .count();
    let max_skip = visible_count.saturating_sub(inner_height);
    app.chat.activity_scroll = app.chat.activity_scroll.min(max_skip);
    render_activity_log_inner(
        frame,
        area,
        selected,
        app.chat.activity_scroll,
        app.chat.activity_hscroll,
        focused,
    );
}

pub(super) fn render_activity_log_inner(
    frame: &mut Frame,
    area: Rect,
    selected: &jyc_types::ThreadInfo,
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

    if selected.activity.is_empty() {
        let text = Paragraph::new(Span::styled(
            "  No activity",
            Style::default().fg(Color::DarkGray),
        ))
        .block(block);
        frame.render_widget(text, area);
        return;
    }

    // Thinking entries are excluded from the activity pane - they appear as
    // dozens of identical "Thinking..." markers and crowd out useful events.
    // The chat pane AI progress area handles thinking display.
    let visible: Vec<_> = selected
        .activity
        .iter()
        .filter(|e| !e.text.starts_with("Thinking: "))
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
            messages: vec![],
            activity_messages: vec![],
            thinking_text: None,
            editor: empty_chat_editor(),
            handler: EditorEventHandler::default(),
            focus: ChatFocus::ChatPane,
            scroll: 0,
            activity_scroll: 0,
            pending_g: false,
            activity_hscroll: 0,
            awaiting_response: false,
            activity_split: 0,
            ws_tx: None,
            ws_rx,
            ws_connected: false,
            subscribed: false,
            detail_channel: None,
            detail_thread_path: None,
            pending_inject: None,
            commands: vec![],
            models: vec![],
            command_popup: None,
            input_history: vec![],
            history_pos: None,
        }
    }

    pub(super) fn open(&mut self, addr: &str, channel: Option<&str>, initial_thread: Option<&str>) {
        self.visible = true;
        self.phase = if initial_thread.is_some() {
            ChatPhase::Chatting
        } else {
            ChatPhase::PatternSelect
        };
        self.patterns.clear();
        self.pattern_selected = 0;
        self.thread = initial_thread.map(|s| s.to_string());
        self.messages.clear();
        self.activity_messages.clear();
        self.thinking_text = None;
        self.awaiting_response = false;
        self.editor = empty_chat_editor();
        self.focus = ChatFocus::ChatPane;
        self.scroll = 0;
        self.activity_scroll = 0;
        self.activity_hscroll = 0;
        self.pending_g = false;
        self.activity_split = 0;
        self.ws_connected = false;
        self.subscribed = false;
        self.input_history.clear();
        self.history_pos = None;

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        self.ws_tx = Some(cmd_tx);
        // Replace the old receiver with the new one
        self.ws_rx = event_rx;

        let url = match channel {
            Some(ch) => format!("ws://{}/ws/{}", addr, ch),
            None => format!("ws://{}/ws", addr),
        };
        tokio::spawn(ws_client_task(url, cmd_rx, event_tx));

        // Subscribe immediately so the server attaches event_rx and streams
        // live Thinking / Tool / Process / Chat events for this thread.
        if initial_thread.is_some() {
            self.subscribe_to_active_thread();
        }
    }

    pub(super) fn close(&mut self) {
        self.visible = false;
        self.phase = ChatPhase::PatternSelect;
        self.ws_connected = false;
        self.command_popup = None;
        self.detail_channel = None;
        self.detail_thread_path = None;
        if let Some(tx) = self.ws_tx.take() {
            // Best-effort disconnect signal
            let _ = tx.send("{\"type\":\"disconnect\"}".to_string());
        }
    }

    /// Open detail/chat mode for a non-WebSocket thread.
    ///
    /// Builds on `open()` (which resets session state and subscribes the WS
    /// connection for live `Thinking`/`Tool`/`Process`/`Chat` events), then
    /// layers detail-mode specifics: `detail_channel` set so the chat pane
    /// uses `inject_message` instead of WS for outgoing messages, and the
    /// thread's chat history loaded from disk via `load_detail_history`.
    pub(super) fn open_thread_detail(
        &mut self,
        channel: &str,
        thread_name: &str,
        state: Option<&InspectState>,
    ) {
        // open() sets the basic chat state + subscribes the WS connection to
        // the thread for live events (Thinking, Tool, Process, Chat). Layer
        // detail-mode specifics on top.
        self.open(channel, Some(channel), Some(thread_name));
        self.detail_channel = Some(channel.to_string());
        self.detail_thread_path = None;

        // Load initial chat history from disk.
        self.load_detail_history(state);
    }

    /// Send a `{type:"subscribe", thread, mode:"chat"}` message over the
    /// active WebSocket connection and mark the session as subscribed
    /// (`subscribed = true`) so the `WsEvent::Connected` handler skips
    /// a duplicate resubscribe. Called from `open` (when `initial_thread`
    /// is `Some`), `select_pattern`, and `open_thread_detail`.
    fn subscribe_to_active_thread(&mut self) {
        self.subscribed = true;
        if let (Some(thread), Some(tx)) = (&self.thread, &self.ws_tx) {
            let subscribe_msg = serde_json::json!({
                "type": "subscribe",
                "thread": thread,
                "mode": "chat",
            })
            .to_string();
            let _ = tx.send(subscribe_msg);
        }
    }

    /// Load chat history from the thread's chat_history_*.jsonl files.
    ///
    /// Reads up to 100 most recent entries (same limit as WebSocket adapter).
    /// `state` is the latest inspect snapshot, used to resolve the thread path
    /// when it has not been cached yet.
    pub(super) fn load_detail_history(&mut self, state: Option<&InspectState>) {
        let thread_path = match &self.detail_thread_path {
            Some(p) => p.clone(),
            None => {
                // Try to get thread_path from the current state
                if let Some(state) = state
                    && let Some(ref chat_name) = self.thread
                    && let Some(thread) = state.threads.iter().find(|t| t.name == *chat_name)
                    && let Some(ref path) = thread.thread_path
                {
                    let path = path.clone();
                    self.detail_thread_path = Some(path.clone());
                    path
                } else {
                    return;
                }
            }
        };

        let entries = jyc_core::chat_log_store::load_recent_chat_history(&thread_path, 100);
        self.messages = entries
            .into_iter()
            .map(|e| ChatMessage {
                sender: e.sender,
                text: e.text,
                timestamp: e.timestamp,
            })
            .collect();
    }

    /// Check whether the current chat is a detail mode (non-WebSocket) session.
    pub(super) fn is_detail_mode(&self) -> bool {
        self.detail_channel.is_some()
    }

    pub(super) fn select_pattern(&mut self, pattern: String) {
        self.phase = ChatPhase::Chatting;
        self.thread = Some(pattern.clone());
        self.editor = empty_chat_editor();
        self.scroll = 0;
        self.messages.clear();
        self.activity_messages.clear();
        self.thinking_text = None;
        self.input_history.clear();
        self.history_pos = None;

        // Subscribe with mode="chat" to get history + activity + live
        // events (thinking, tool, process, chat).
        self.subscribe_to_active_thread();
    }

    pub(super) fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            ChatFocus::ChatPane => ChatFocus::ActivityPane,
            ChatFocus::ActivityPane => ChatFocus::ChatPane,
        };
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

        let thread = match &self.thread {
            Some(t) => t.clone(),
            None => return,
        };

        if self.is_detail_mode() {
            // Detail mode (non-WebSocket): do NOT echo locally — the message
            // will appear via `recent_messages` on the next poll cycle
            // (~500ms). Echoing locally would cause duplication because the
            // `IncomingMessage` event publishes with sender="dashboard" and
            // truncated text, failing the dedup check.
            self.pending_inject = Some((thread, text));
        } else {
            // WebSocket mode: echo user message locally, send via WebSocket.
            // No duplication — the WebSocket broadcast only sends AI replies,
            // not user messages.
            self.messages.push(ChatMessage {
                sender: "user".to_string(),
                text: text.clone(),
                timestamp: Some(chrono::Utc::now().to_rfc3339()),
            });
            let msg = serde_json::json!({
                "type": "message",
                "thread": thread,
                "text": text,
            })
            .to_string();
            if let Some(tx) = &self.ws_tx {
                let _ = tx.send(msg);
            }
        }

        self.scroll = 0;
        self.awaiting_response = true;
    }

    pub(super) fn handle_ws_message(&mut self, text: &str) {
        let parsed: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };

        match parsed.get("type").and_then(|v| v.as_str()) {
            Some("patterns") => {
                if let Some(patterns) = parsed.get("patterns").and_then(|v| v.as_array()) {
                    self.patterns = patterns
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    self.pattern_selected = 0;
                }
            }
            Some("history") => {
                if let (Some(thread), Some(messages)) = (
                    parsed.get("thread").and_then(|v| v.as_str()),
                    parsed.get("messages").and_then(|v| v.as_array()),
                ) && self.thread.as_deref() == Some(thread)
                {
                    self.messages = messages
                        .iter()
                        .filter_map(|m| {
                            Some(ChatMessage {
                                sender: m.get("sender")?.as_str()?.to_string(),
                                text: m.get("text")?.as_str()?.to_string(),
                                timestamp: m
                                    .get("timestamp")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string()),
                            })
                        })
                        .collect();
                }
            }
            // New: streamed AI reasoning. Replace (not append) — the
            // server emits a single `Thinking` containing the full text
            // after each thinking chunk.
            Some("thinking") => {
                if let (Some(thread), Some(text)) = (
                    parsed.get("thread").and_then(|v| v.as_str()),
                    parsed.get("text").and_then(|v| v.as_str()),
                ) && self.thread.as_deref() == Some(thread)
                {
                    self.thinking_text = Some(text.to_string());
                }
            }
            // New: tool start/complete. Push to activity buffer.
            // `kind` is an enum serialized as snake_case: "started",
            // "completed", or "failed".
            Some("tool") => {
                if let (Some(thread), Some(kind), Some(text)) = (
                    parsed.get("thread").and_then(|v| v.as_str()),
                    parsed.get("kind").and_then(|v| v.as_str()),
                    parsed.get("text").and_then(|v| v.as_str()),
                ) && self.thread.as_deref() == Some(thread)
                {
                    let severity = match kind {
                        "failed" => jyc_types::Severity::Error,
                        _ => jyc_types::Severity::Info,
                    };
                    self.activity_messages
                        .push(jyc_types::inspect::ActivityEntry {
                            text: text.to_string(),
                            timestamp: Some(chrono::Utc::now().to_rfc3339()),
                            severity,
                        });
                    // Cap the buffer to avoid unbounded growth
                    if self.activity_messages.len() > 200 {
                        let drop = self.activity_messages.len() - 200;
                        self.activity_messages.drain(0..drop);
                    }
                }
            }
            // New: processing lifecycle. Push to the shared activity buffer
            // so the chat progress (last 2 entries) and the activity pane
            // (scrollable list) both see these events.
            Some("process") => {
                if let (Some(thread), Some(kind), Some(dur)) = (
                    parsed.get("thread").and_then(|v| v.as_str()),
                    parsed.get("kind").and_then(|v| v.as_str()),
                    parsed.get("duration_secs").and_then(|v| v.as_f64()),
                ) && self.thread.as_deref() == Some(thread)
                {
                    // Clear stale thinking text from the previous processing
                    // cycle. New thinking arrives via WS 'thinking' events.
                    self.thinking_text = None;

                    let text = match kind {
                        "started" => "Processing started".to_string(),
                        "completed" => format!("Completed ({:.0}s)", dur),
                        "failed" => format!("Failed ({:.0}s)", dur),
                        _ => format!("Processing: {kind}"),
                    };
                    self.activity_messages
                        .push(jyc_types::inspect::ActivityEntry {
                            text,
                            timestamp: Some(chrono::Utc::now().to_rfc3339()),
                            severity: if kind == "failed" {
                                jyc_types::Severity::Error
                            } else {
                                jyc_types::Severity::Info
                            },
                        });
                    if self.activity_messages.len() > 200 {
                        let drop = self.activity_messages.len() - 200;
                        self.activity_messages.drain(0..drop);
                    }
                }
            }
            // New: live chat message (replaces legacy `reply`).
            Some("chat") => {
                if let (Some(thread), Some(sender), Some(text)) = (
                    parsed.get("thread").and_then(|v| v.as_str()),
                    parsed.get("sender").and_then(|v| v.as_str()),
                    parsed.get("text").and_then(|v| v.as_str()),
                ) && self.thread.as_deref() == Some(thread)
                {
                    // Dedup by text only — the WS bus replays buffered events
                    // but they typically have the same text/sender. Using
                    // text alone is sufficient since we never display the
                    // same user message twice in practice.
                    let already = self.messages.iter().any(|m| m.text == text);
                    if !already {
                        self.messages.push(ChatMessage {
                            sender: sender.to_string(),
                            text: text.to_string(),
                            timestamp: Some(chrono::Utc::now().to_rfc3339()),
                        });
                        self.scroll = 0;
                    }
                }
            }
            // Legacy: `reply` broadcast (kept for backward compat).
            Some("reply") => {
                if let (Some(thread), Some(text)) = (
                    parsed.get("thread").and_then(|v| v.as_str()),
                    parsed.get("text").and_then(|v| v.as_str()),
                ) {
                    // Only append if it matches our subscribed thread
                    if self.thread.as_deref() == Some(thread) {
                        self.messages.push(ChatMessage {
                            sender: "ai".to_string(),
                            text: text.to_string(),
                            timestamp: Some(chrono::Utc::now().to_rfc3339()),
                        });
                        self.scroll = 0;
                        self.awaiting_response = false;
                    }
                }
            }
            _ => {}
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_pattern_clears_chat_messages() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx);

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
        app.chat.select_pattern("thread-b".to_string());

        // Messages must be cleared so stale content doesn't leak across threads
        assert!(app.chat.messages.is_empty());
        assert_eq!(app.chat.thread.as_deref(), Some("thread-b"));
    }

    #[test]
    fn scroll_to_top_and_bottom_follow_focus() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx);

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
        let mut app = App::new(rx);

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
        let mut app = App::new(rx);

        assert!(app.chat.input_history.is_empty());
        app.chat.recall_older(); // should not panic or change anything
        assert!(app.chat.history_pos.is_none());
    }

    #[test]
    fn recall_older_recalls_and_recall_newer_clears() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx);

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
        let mut app = App::new(rx);

        app.chat.input_history = vec!["msg from thread A".to_string()];
        app.chat.history_pos = Some(0);

        // Switch to a new thread
        app.chat.select_pattern("thread-b".to_string());

        // History must be cleared so it doesn't leak across threads
        assert!(app.chat.input_history.is_empty());
        assert!(app.chat.history_pos.is_none());
    }

    #[test]
    fn close_returns_to_overview_from_ws_chat() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx);

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
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx);

        // Simulate opening a non-WS thread in detail mode
        app.chat.open_thread_detail("github", "issue-197", None);
        assert!(app.chat.visible);
        assert_eq!(app.chat.phase, ChatPhase::Chatting);
        assert!(app.chat.is_detail_mode());

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
    fn open_clears_activity_messages_thinking_and_awaiting_on_reentry() {
        // Regression: open() must clear session state so stale content
        // from the previous session does not leak into the re-entered thread.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx);

        // First open: simulate a session with activity state
        app.chat
            .open("127.0.0.1:9999", Some("ws"), Some("thread-a"));
        app.chat
            .activity_messages
            .push(jyc_types::inspect::ActivityEntry {
                text: "stale tool call".into(),
                timestamp: None,
                severity: jyc_types::Severity::Info,
            });
        app.chat.thinking_text = Some("stale thinking".into());
        app.chat.awaiting_response = true;

        // Second open: re-enter a different thread — must clear all stale state
        app.chat
            .open("127.0.0.1:9999", Some("ws"), Some("thread-b"));

        assert!(
            app.chat.activity_messages.is_empty(),
            "activity_messages must be cleared on re-entry"
        );
        assert!(
            app.chat.thinking_text.is_none(),
            "thinking_text must be cleared on re-entry"
        );
        assert!(
            !app.chat.awaiting_response,
            "awaiting_response must be cleared on re-entry"
        );
        assert!(
            app.chat.subscribed,
            "open() must mark the session as subscribed after subscribe_to_active_thread()"
        );
        assert_eq!(app.chat.thread.as_deref(), Some("thread-b"));
        assert_eq!(app.chat.phase, ChatPhase::Chatting);
    }

    #[test]
    fn open_without_initial_thread_goes_to_pattern_select() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx);

        // open without initial_thread → pattern select mode
        app.chat.open("127.0.0.1:9999", None, None);
        assert_eq!(app.chat.phase, ChatPhase::PatternSelect);
        assert!(app.chat.thread.is_none());
        assert!(
            !app.chat.subscribed,
            "pattern-select open must NOT mark the session as subscribed"
        );
    }

    #[test]
    fn open_resets_subscribed_flag() {
        // Regression: the `subscribed` flag must reset on `open()` so the
        // `WsEvent::Connected` re-subscribe path fires correctly on the
        // next WS reconnect (and doesn't double-subscribe on initial
        // connect).
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx);

        // First open sets subscribed via subscribe_to_active_thread.
        app.chat
            .open("127.0.0.1:9999", Some("ws"), Some("thread-a"));
        assert!(app.chat.subscribed);

        // Pre-set subscribed=true to prove `open()` actually resets it.
        // Without the reset line in open(), this assertion would fail:
        // subscribe_to_active_thread is not called for `initial_thread=None`,
        // so a stale `true` would survive and the guard in
        // `WsEvent::Connected` would skip a needed resubscribe on the
        // next reconnect.
        app.chat.subscribed = true;
        app.chat.open("127.0.0.1:9999", None, None);
        assert!(
            !app.chat.subscribed,
            "open() must reset subscribed even when no subscribe is sent"
        );

        // Re-enter with thread: subscribed is reset, then
        // subscribe_to_active_thread sets it back to true.
        app.chat
            .open("127.0.0.1:9999", Some("ws"), Some("thread-b"));
        assert!(app.chat.subscribed);
        assert_eq!(app.chat.thread.as_deref(), Some("thread-b"));
    }
}
