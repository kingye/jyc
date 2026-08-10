//! Chat pane: state, key handling, and rendering for the dashboard's
//! WebSocket thread chat (all channel types via `/ws/<channel>/<thread>`).

use super::token_render::{
    input_token_pct, push_cache_creation_span, push_cache_hit_span, push_cost_span,
    push_output_span, push_tokens_span, push_total_input_span,
};
use super::*;

/// Width of the input prompt gutter ("╰─❯ ").
const PROMPT_GUTTER_WIDTH: u16 = 4;

/// Color for box-drawing characters in the chat header and input gutter
/// ("╭─", "╰─", and the "─" padding run). The ❮/❯ arrows are yellow.
const LINE_DRAWING: Style = Style::new().fg(Color::Rgb(0x39, 0x35, 0x52));

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
    /// The chat input field (vim editor).
    ChatPane,
    /// The scrollable message area above the input field.
    MessageArea,
    /// The right-hand thread info pane (thread metadata + changed files).
    InfoPane,
    /// The activity log pane.
    ActivityPane,
    /// The left-side thread explorer pane.
    ExplorerPane,
}

/// A single message in the chat conversation.
#[derive(Debug, Clone)]
pub(super) struct ChatMessage {
    pub(super) sender: String,
    pub(super) text: String,
    pub(super) timestamp: Option<String>,
}

/// Aux-pane visibility snapshot taken when entering zen mode and restored
/// on exit (see `ChatState::zen_saved`).
#[derive(Debug, Clone, Copy)]
pub(super) struct ZenSnapshot {
    activity_split: u8,
    info_visible: bool,
    status_visible: bool,
    explorer_visible: bool,
}

/// Chat pane state: WebSocket thread chat for any channel type.
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
    pub(super) info_scroll: usize,
    pub(super) activity_scroll: usize,
    /// Last rendered rectangle of the scrollable message area (top chunk
    /// inside the chat pane). Stored during render and used by mouse-wheel
    /// hit-testing so scrolling only happens when the cursor is over the
    /// message area, not the editor / activity / explorer / info panes.
    pub(super) last_message_area: Option<Rect>,
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
    /// Thread info pane (right side, 20% width) visibility. Default
    /// visible; toggled via the leader-key popup (`i`).
    pub(super) info_visible: bool,
    /// Bottom status bar visibility. Default visible; toggled via the
    /// leader-key popup (`s`).
    pub(super) status_visible: bool,
    /// Thread explorer pane (left side, 20% width). Default hidden;
    /// toggled via the leader-key popup (`e`).
    pub(super) explorer_visible: bool,
    /// Aux-pane snapshot taken when entering zen mode (`Ctrl+Z` / leader
    /// `z`), restored exactly on exit. `Some` = currently in zen mode.
    pub(super) zen_saved: Option<ZenSnapshot>,
    /// Selected row in the explorer pane.
    pub(super) explorer_selected: usize,
    pub(super) ws_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pub(super) ws_rx: tokio::sync::mpsc::UnboundedReceiver<WsEvent>,
    pub(super) ws_connected: bool,
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
    /// Live loop duration in milliseconds — updated by WS `loop_tick`
    /// events (1 Hz while a loop is running, with the first tick fired
    /// immediately at t=0). Drives the live-duration ticker in the
    /// dashboard's Details panel, the chat-mode info pane, and the chat
    /// progress line.
    pub(super) live_tick_ms: std::collections::BTreeMap<(String, String), u64>,
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
    /// TUI-local leader-key popup (navigation, zen mode, activity pane, ...).
    /// Never sent to the backend.
    pub(super) leader: Option<leader::Leader>,
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

/// Format a wall-clock duration in milliseconds for the live loop ticker.
/// Below 60s renders one decimal (`"12.4s"`); at or above 60s renders as
/// `"1m05s"` style. Used by the dashboard Details panel, chat-mode info
/// pane, and chat progress line.
pub(super) fn format_elapsed_ms(ms: u64) -> String {
    if ms < 60_000 {
        format!("{}.{}s", ms / 1000, (ms / 100) % 10)
    } else {
        let s = ms / 1000;
        format!("{}m{:02}s", s / 60, s % 60)
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
pub(super) fn edit_input_externally<B: ratatui::backend::Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
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

/// Move the explorer selection by `delta` rows, clamped to the current
/// thread list.
fn explorer_move(app: &mut App, delta: i64) {
    let len = app.state.as_ref().map(|s| s.threads.len()).unwrap_or(0);
    if len == 0 {
        app.chat.explorer_selected = 0;
        return;
    }
    let cur = app.chat.explorer_selected as i64;
    app.chat.explorer_selected = cur.saturating_add(delta).clamp(0, len as i64 - 1) as usize;
}

/// Open the thread currently selected in the explorer pane. All channel
/// types use the unified `/ws/<channel>/<thread>` endpoint.
fn explorer_open_selected(app: &mut App) {
    let info = app.state.as_ref().and_then(|s| {
        s.threads
            .get(app.chat.explorer_selected)
            .map(|t| (t.name.clone(), t.channel.clone()))
    });
    let Some((name, channel)) = info else {
        return;
    };
    match app.chat.open_addr.clone() {
        Some(addr) => {
            let token = app.chat.token.clone();
            app.chat.open(&addr, Some(&channel), Some(&name), token);
            // Hydration runs on the async poll loop (sync key handler
            // can't await on InspectClient).
            app.pending_hydrate = Some((channel, name));
            // Focus on the new thread's input; hide the explorer so
            // the user lands in the chat, not the pane they used to
            // pick the thread.
            app.chat.explorer_visible = false;
        }
        None => app.set_status("No server address available".to_string()),
    }
}

/// Execute a TUI-local action selected from the leader-key popup.
pub(super) fn execute_local_action<B: ratatui::backend::Backend>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    action: local_commands::LocalAction,
) {
    use local_commands::LocalAction;
    match action {
        LocalAction::OpenDashboard => app.chat.close(),
        // Dashboard-scoped; never offered on the chat screen.
        LocalAction::OpenChat => {}
        LocalAction::NewChat => app.pending_new_chat = true,
        LocalAction::ReloadConfig => app.pending_reload_config = true,
        LocalAction::Quit => app.should_quit = true,
        LocalAction::ToggleExplorer => toggle_explorer_snapped(app),
        LocalAction::ToggleZen => app.chat.toggle_zen_mode(),
        LocalAction::ToggleActivity => app.chat.toggle_activity(),
        LocalAction::ToggleStatus => app.chat.toggle_status_bar(),
        LocalAction::ToggleInfo => app.chat.toggle_info_pane(),
        LocalAction::OpenExternalEditor => {
            if app.chat.focus == ChatFocus::ChatPane
                && let Err(e) = edit_input_externally(app, terminal)
            {
                app.set_status(format!("Editor error: {e:#}"));
            }
        }
        LocalAction::ScrollTop => app.chat.scroll_to_top(),
        LocalAction::ScrollBottom => app.chat.scroll_to_bottom(),
        LocalAction::ToggleMouseCapture => super::toggle_mouse_capture(app),
    }
}

/// Toggle the thread explorer. When opening, snap the selection to the
/// thread currently open in the chat pane — `sync_explorer_selection`
/// only follows the chat thread while the explorer is *unfocused*, so
/// without this the explorer would open on a stale row.
fn toggle_explorer_snapped(app: &mut App) {
    app.chat.toggle_explorer();
    if !app.chat.explorer_visible {
        return;
    }
    let idx = app.state.as_ref().and_then(|s| {
        let thread = app.chat.thread.as_deref()?;
        let channel = app.chat.channel.as_deref()?;
        s.threads
            .iter()
            .position(|t| t.name == thread && t.channel == channel)
    });
    if let Some(idx) = idx {
        app.chat.explorer_selected = idx;
    }
}

pub(super) fn handle_chat_keys<B: ratatui::backend::Backend>(
    app: &mut App,
    key: event::KeyEvent,
    terminal: &mut Terminal<B>,
) {
    // Ctrl+Q quits the entire dashboard (consistent across all modes)
    let is_ctrl_q = key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL);

    if is_ctrl_q {
        app.should_quit = true;
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
        app.chat.leader = None;
        app.chat.send_message_inner("/cancel".to_string());
        return;
    }

    // ── Leader-key popup handling (TUI-local commands, never sent) ──
    if let Some(ref mut leader) = app.chat.leader {
        match leader.handle_key(key) {
            leader::LeaderResult::Consumed => {}
            leader::LeaderResult::Closed => {
                app.chat.leader = None;
            }
            leader::LeaderResult::Action(action) => {
                app.chat.leader = None;
                execute_local_action(app, terminal, action);
            }
        }
        return;
    }

    // Ctrl+P is the leader (works from any editor mode and in any chat
    // phase — it is the only way back to the dashboard from
    // PatternSelect).
    let is_ctrl_p = key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_p {
        app.chat.command_popup = None;
        app.chat.leader = Some(leader::Leader::new(local_commands::CommandScope::Chat));
        return;
    }

    // Space opens the leader in Normal mode (vim-style, symmetric with
    // "/" opening the backend command popup). Suppressed while the
    // command popup is open — there Space is legitimate editor input.
    let is_space = key.code == KeyCode::Char(' ') && !key.modifiers.contains(KeyModifiers::CONTROL);
    if is_space
        && app.chat.phase == ChatPhase::Chatting
        && app.chat.focus == ChatFocus::ChatPane
        && app.chat.editor.mode == EditorMode::Normal
        && app.chat.command_popup.is_none()
    {
        app.chat.command_popup = None;
        app.chat.leader = Some(leader::Leader::new(local_commands::CommandScope::Chat));
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
            app.chat.leader = None;
            app.chat.command_popup = Some(CommandPopupState::new());
            return;
        }
    }

    match app.chat.phase {
        ChatPhase::PatternSelect => match key.code {
            // No Esc-back here: returning to the dashboard is done via the
            // leader-key popup (`open dashboard`, Ctrl+P / Space).
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
                    if app.chat.focus == ChatFocus::ExplorerPane {
                        explorer_move(app, -10);
                    } else {
                        app.chat.page_up();
                    }
                    return;
                }
                KeyCode::PageDown => {
                    if app.chat.focus == ChatFocus::ExplorerPane {
                        explorer_move(app, 10);
                    } else {
                        app.chat.page_down();
                    }
                    return;
                }
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if app.chat.focus == ChatFocus::ExplorerPane {
                        explorer_move(app, -10);
                    } else {
                        app.chat.page_up();
                    }
                    return;
                }
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if app.chat.focus == ChatFocus::ExplorerPane {
                        explorer_move(app, 10);
                    } else {
                        app.chat.page_down();
                    }
                    return;
                }
                _ => {}
            }

            // Explorer pane: navigate the thread list; Enter switches the
            // chat to the selected thread. Esc returns focus to the input.
            if app.chat.focus == ChatFocus::ExplorerPane {
                match key.code {
                    KeyCode::Esc => {
                        app.chat.focus = ChatFocus::ChatPane;
                    }
                    KeyCode::Up | KeyCode::Char('k') => explorer_move(app, -1),
                    KeyCode::Down | KeyCode::Char('j') => explorer_move(app, 1),
                    KeyCode::Char('g') if gg_jump => explorer_move(app, i64::MIN),
                    KeyCode::Char('G') => explorer_move(app, i64::MAX),
                    KeyCode::Enter => explorer_open_selected(app),
                    _ => {}
                }
                return;
            }

            if app.chat.focus == ChatFocus::InfoPane {
                // Vertical scroll only — file paths are short enough
                // that horizontal overflow isn't a concern. No Esc-back:
                // leaving the info pane is via Tab (focus cycle) or
                // the leader-key popup, same as ActivityPane.
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => app.chat.scroll_up(),
                    KeyCode::Down | KeyCode::Char('j') => app.chat.scroll_down(),
                    KeyCode::Char('G') => app.chat.scroll_to_bottom(),
                    KeyCode::Char('g') if gg_jump => app.chat.scroll_to_top(),
                    KeyCode::Char('g') => {}
                    KeyCode::PageUp => app.chat.page_up(),
                    KeyCode::PageDown => app.chat.page_down(),
                    _ => {}
                }
                return;
            }

            if app.chat.focus == ChatFocus::ActivityPane {
                match key.code {
                    // No Esc-back here: returning to the dashboard is done
                    // via the leader-key popup (`open dashboard`, Ctrl+P / Space).
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

            // Message area: scroll the conversation with arrows / vim keys.
            // Esc returns focus to the input field (does not exit the chat).
            // Any other printable key refocuses the input and is forwarded
            // to the editor, so the user can scroll then just start typing.
            if app.chat.focus == ChatFocus::MessageArea {
                match key.code {
                    KeyCode::Esc => {
                        app.chat.focus = ChatFocus::ChatPane;
                    }
                    KeyCode::Up | KeyCode::Char('k') => app.chat.scroll_up(),
                    KeyCode::Down | KeyCode::Char('j') => app.chat.scroll_down(),
                    KeyCode::Char('G') => app.chat.scroll_to_bottom(),
                    KeyCode::Char('g') if gg_jump => app.chat.scroll_to_top(),
                    KeyCode::Char('g') => {}
                    _ => {
                        app.chat.focus = ChatFocus::ChatPane;
                        app.chat.handler.on_key_event(key, &mut app.chat.editor);
                    }
                }
                return;
            }

            // Chat input field: vim editor. Everything not matched here is
            // delegated to the edtui event handler.
            match (app.chat.editor.mode, key.code) {
                // Esc does not leave the thread: returning to the dashboard
                // is done via the leader-key popup (`open dashboard`, Ctrl+P / Space).
                // The editor uses Esc to return to Normal mode.
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
                _ => app.chat.handler.on_key_event(key, &mut app.chat.editor),
            }
        }
    }
}

/// Handle a mouse event while the chat screen is visible.
///
/// Mouse capture is enabled in `dashboard::run`, so this only fires for the
/// chat screen (the event loop filters with `app.chat.visible`). We only
/// act on `ScrollUp` / `ScrollDown`; all other mouse kinds (clicks, moves,
/// drags) are intentionally ignored to avoid hijacking the input field
/// while the user is editing.
///
/// Hit-testing: the message area is the only pane that responds to the
/// wheel. The activity, explorer, info, and input areas silently absorb
/// the event so wheel-over-them keeps the editor's IME-like behaviour
/// (no accidental focus theft). When the wheel does land on the message
/// area we move focus to `MessageArea` first, so the focus-routed
/// `scroll_up` / `scroll_down` advance the message offset regardless of
/// which pane the user was last navigating — otherwise `ActivityPane` /
/// `ExplorerPane` focus would silently redirect the scroll elsewhere.
pub(super) fn handle_chat_mouse(app: &mut App, mouse: MouseEvent) {
    // Defensive guard — crossterm shouldn't deliver mouse events when
    // capture is off, but if one sneaks through (e.g. a queued event
    // from the toggle moment), do nothing.
    if !app.mouse_capture_enabled {
        return;
    }
    if app.chat.phase != ChatPhase::Chatting {
        return;
    }
    let Some(rect) = app.chat.last_message_area else {
        return;
    };
    if !rect.contains(Position::new(mouse.column, mouse.row)) {
        return;
    }
    app.chat.focus = ChatFocus::MessageArea;
    match mouse.kind {
        MouseEventKind::ScrollUp => app.chat.scroll_up(),
        MouseEventKind::ScrollDown => app.chat.scroll_down(),
        _ => {}
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
    // Status bar and thread info pane have independent visibility flags
    // (leader `s` / `i`); zen mode hides both.

    if app.chat.phase == ChatPhase::PatternSelect {
        // Pattern select is the initial screen when no thread is chosen.
        // Info row and status row are independent; zen hides both.
        let mut constraints = Vec::with_capacity(3);
        if app.chat.info_visible {
            constraints.push(Constraint::Length(1)); // Thread info pane
        }
        constraints.push(Constraint::Min(0)); // Pattern select
        if app.chat.status_visible {
            constraints.push(Constraint::Length(1)); // Status bar
        }
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);
        let mut i = 0;
        if app.chat.info_visible {
            render_thread_info_pane(frame, chunks[i], app);
            i += 1;
        }
        render_pattern_select(frame, chunks[i], app);
        if app.chat.status_visible {
            render_status_bar(frame, chunks[i + 1], app);
        }
        return;
    }

    // Chatting phase.
    let show_status = app.chat.status_visible;
    let show_activity = app.chat.activity_split != 0;
    let show_explorer = app.chat.explorer_visible;
    if show_explorer {
        sync_explorer_selection(app);
    }

    // Outer vertical split: [main, status?]. The status bar (when visible)
    // spans the full width across both columns.
    let (main_area, status_area) = if show_status {
        let v = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
        (v[0], Some(v[1]))
    } else {
        (area, None)
    };

    // Main area: horizontal [explorer?, right column]. The right column
    // holds chat, info, and activity.
    let (explorer_area, right_area) = if show_explorer {
        let h = Layout::horizontal([Constraint::Percentage(20), Constraint::Percentage(80)])
            .split(main_area);
        (Some(h[0]), h[1])
    } else {
        (None, main_area)
    };

    // Right column: vertical [top(chat+info), activity?].
    let activity_pct = match app.chat.activity_split {
        1 => 20,
        2 => 80,
        3 => 100,
        _ => 0,
    };
    let right_constraints: Vec<Constraint> = if show_activity {
        vec![Constraint::Min(0), Constraint::Percentage(activity_pct)]
    } else {
        vec![Constraint::Min(0)]
    };
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(right_constraints)
        .split(right_area);

    // Top row inside the right column: chat + optional info pane.
    let top_row = right_chunks[0];
    let top_cols = if app.chat.info_visible {
        Layout::horizontal([Constraint::Percentage(80), Constraint::Percentage(20)]).split(top_row)
    } else {
        Layout::horizontal([Constraint::Percentage(100)]).split(top_row)
    };
    render_chat_conversation(frame, top_cols[0], app);
    if app.chat.info_visible {
        render_thread_info_pane(frame, top_cols[1], app);
    }

    if let Some(exp) = explorer_area {
        render_explorer(frame, exp, app);
    }

    if show_activity {
        render_activity_log(frame, right_chunks[1], app);
    }

    if let Some(status) = status_area {
        render_status_bar(frame, status, app);
    }
}

/// Keep the explorer selection valid and, while the explorer is not
/// focused, following the thread currently open in the chat pane.
fn sync_explorer_selection(app: &mut App) {
    let Some(s) = app.state.as_ref() else {
        app.chat.explorer_selected = 0;
        return;
    };
    let len = s.threads.len();
    if len == 0 {
        app.chat.explorer_selected = 0;
        return;
    }
    if app.chat.explorer_selected >= len {
        app.chat.explorer_selected = len - 1;
    }
    if app.chat.focus != ChatFocus::ExplorerPane
        && let (Some(thread), Some(channel)) = (&app.chat.thread, &app.chat.channel)
        && let Some(idx) = s
            .threads
            .iter()
            .position(|t| &t.name == thread && &t.channel == channel)
    {
        app.chat.explorer_selected = idx;
    }
}

/// Render the left-side thread explorer pane (20% wide when shown).
///
/// Lists all threads from the latest overview poll with a status dot
/// (green = processing, yellow = queued, cyan = waiting, red = error),
/// highlighting the thread currently open in the chat pane. The list is
/// rebuilt from `app.state` on every render, so it stays live.
pub(super) fn render_explorer(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.chat.focus == ChatFocus::ExplorerPane;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    // The right edge (against the chat pane) gets a vertical border, and the
    // top edge carries the title inline with the top border so the title
    // row acts as a separator between the heading and the thread list
    // below.
    let block = Block::default()
        .title("── Threads ")
        .borders(Borders::TOP | Borders::RIGHT)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(s) = app.state.as_ref() else {
        return;
    };

    let current = app.chat.thread.as_ref().zip(app.chat.channel.as_ref());
    let selected = app.chat.explorer_selected;

    // Scroll window: keep the selected row visible.
    let height = inner.height as usize;
    let offset = if selected >= height {
        selected - height + 1
    } else {
        0
    };

    let lines: Vec<Line> = s
        .threads
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, t)| {
            let dot_style = match t.status {
                ThreadStatus::Processing => Style::default().fg(Color::Green),
                ThreadStatus::Queued => Style::default().fg(Color::Yellow),
                ThreadStatus::WaitingForAnswer => Style::default().fg(Color::Cyan),
                ThreadStatus::Idle => Style::default().fg(Color::DarkGray),
                ThreadStatus::Error => Style::default().fg(Color::Red),
            };
            let is_current = current == Some((&t.name, &t.channel));
            let is_selected = i == selected && focused;
            let name_style = if is_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if is_current {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            // For the focused selection row, paint the full row width with
            // the highlight background so the selection visually fills the
            // row instead of stopping at the end of the thread name.
            if is_selected {
                let sel_style = Style::default().fg(Color::Black).bg(Color::Cyan);
                let mut spans = Vec::with_capacity(3);
                spans.push(Span::styled("● ", sel_style));
                spans.push(Span::styled(t.name.as_str(), name_style));
                let used = "● ".width() + t.name.as_str().width();
                let pad = (inner.width as usize).saturating_sub(used);
                if pad > 0 {
                    spans.push(Span::styled(" ".repeat(pad), sel_style));
                }
                Line::from(spans)
            } else {
                Line::from(vec![
                    Span::styled("● ", dot_style),
                    Span::styled(t.name.as_str(), name_style),
                ])
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Find the `ThreadSummary` currently in scope. Used by both the
/// thread info pane and the chat header so the two views agree on
/// which thread is "selected".
///
/// Lookup order matches the legacy chat-pane behavior:
/// 1. The thread the chat pane is currently bound to (`app.chat.thread`).
/// 2. The currently-selected row of the thread table.
fn selected_thread_summary(app: &App) -> Option<&jyc_types::ThreadSummary> {
    let state = app.state.as_ref()?;
    state
        .threads
        .iter()
        .find(|t| Some(&t.name) == app.chat.thread.as_ref())
        .or_else(|| {
            app.table_state
                .selected()
                .and_then(|i| state.threads.get(i))
        })
}

/// Render the right-hand thread info pane (always 20% wide when shown).
///
/// Displays thread name, channel, pattern, model, mode, tokens, a
/// processing indicator, and the changed-files list (which can scroll
/// when it overflows the pane). Wraps content in a bordered `Block`
/// so it is visually separable from the borderless chat pane. Takes
/// `&mut App` because the changed-files section owns
/// `app.chat.info_scroll`, which is clamped on every render.
pub(super) fn render_thread_info_pane(frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.chat.focus == ChatFocus::InfoPane;
    // The left edge (against the chat pane) gets a vertical border, and the
    // top edge carries the title inline with the top border so the title
    // row acts as a separator between the heading and the content below.
    // When focused, paint the border yellow so the user knows they own
    // the scroll keys (mirrors render_activity_log_inner).
    let mut block = Block::default()
        .title("── Thread Info ")
        .borders(Borders::TOP | Borders::LEFT);
    if focused {
        block = block.border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines: Vec<Line> = if let Some(t) = selected_thread_summary(app) {
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
        // Branch is resolved server-side and shipped on ThreadSummary.branch.
        // Skipped when the selected thread's thread_path isn't a git repo
        // (most chat-channel threads: feishu/wecom).
        if let Some(branch) = t.branch.as_deref() {
            out.push(Line::from(vec![
                Span::styled("Branch: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(branch),
            ]));
        }
        // Tokens row — push tokens span into a fresh Vec, wrap in a Line.
        let mut token_spans = Vec::with_capacity(2);
        push_tokens_span(&mut token_spans, t);
        if !token_spans.is_empty() {
            out.push(Line::from(token_spans));
        }
        // Output row — same pattern.
        let mut output_spans = Vec::with_capacity(2);
        push_output_span(&mut output_spans, t);
        if !output_spans.is_empty() {
            out.push(Line::from(output_spans));
        }
        // Total input row — accumulated lifetime sum across all LLM calls.
        let mut total_input_spans = Vec::with_capacity(2);
        push_total_input_span(&mut total_input_spans, t);
        if !total_input_spans.is_empty() {
            out.push(Line::from(total_input_spans));
        }
        // Cache hits row — accumulated prompt-cache hits across all LLM
        // calls in the session. Distinct from `total_input_tokens`
        // (which counts all tokens billed as input); this counts only
        // the portion served from the provider's prompt cache.
        let mut cache_hit_spans = Vec::with_capacity(2);
        push_cache_hit_span(&mut cache_hit_spans, t);
        if !cache_hit_spans.is_empty() {
            out.push(Line::from(cache_hit_spans));
        }
        // Cache create row — Anthropic-only cache **write** tokens
        // billed at the (typically 1.25× input) creation rate.
        // Rendered only when the running total is non-zero, so
        // non-Anthropic sessions see no extra row.
        let mut cache_creation_spans = Vec::with_capacity(2);
        push_cache_creation_span(&mut cache_creation_spans, t);
        if !cache_creation_spans.is_empty() {
            out.push(Line::from(cache_creation_spans));
        }
        // Cost row — session-scoped spend plus today's durable total.
        // Omitted entirely when the model has no configured pricing.
        let mut cost_spans = Vec::with_capacity(2);
        push_cost_span(&mut cost_spans, t);
        if !cost_spans.is_empty() {
            out.push(Line::from(cost_spans));
        }
        if t.status == ThreadStatus::Processing {
            let mut thinking_line: Vec<Span> = vec![Span::styled(
                "⏳ AI thinking...",
                Style::default().fg(Color::Yellow),
            )];
            // Append the live-duration ticker when a tick has arrived.
            // Falls back to the plain `⏳ AI thinking...` line when no
            // tick has arrived yet — the first tick fires at t=0 so
            // this is essentially instantaneous for any loop that runs
            // long enough to render this view.
            if let Some(ms) = app.chat.live_tick_ms_for(&t.channel, &t.name) {
                thinking_line.push(Span::styled(
                    format!(" ({})", format_elapsed_ms(ms)),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
            out.push(Line::from(thinking_line));
        }
        // Separated section at the end: files changed relative to `main`,
        // resolved server-side and shipped on `ThreadSummary.changed_files`
        // as `Vec<ChangedFileEntry>`. The whole list is rendered (no
        // cap) — the parent pane scrolls when the list overflows. Each
        // path is plain when only committed on the branch, yellow when
        // currently dirty in the working tree. The section is skipped
        // entirely when the field is `None` (not a git repo, both
        // `git diff` invocations failed). Empty `Some(vec![])` is shown
        // as `Files: (none)` so the user knows the field resolved to
        // "no changes".
        if let Some(files) = t.changed_files.as_deref() {
            out.push(Line::default());
            if files.is_empty() {
                out.push(Line::from(vec![
                    Span::styled("Files: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled("(none)", Style::default().fg(Color::DarkGray)),
                ]));
            } else {
                out.push(Line::from(Span::styled(
                    format!("Files ({}):", files.len()),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                for entry in files {
                    // One-column glyph + one space, then the path.
                    // Two-space prefix for Modified keeps the path
                    // column aligned with Added/Deleted rows so the
                    // eye can scan vertically.
                    let prefix = match entry.change {
                        jyc_types::ChangeKind::Added => "+ ",
                        jyc_types::ChangeKind::Deleted => "- ",
                        jyc_types::ChangeKind::Modified => "  ",
                    };
                    let style = if entry.uncommitted {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default()
                    };
                    out.push(Line::from(Span::styled(
                        format!("{}{}", prefix, entry.path),
                        style,
                    )));
                }
            }
        }
        out
    } else {
        vec![Line::from("Select a thread")]
    };

    // Slice-skip in Rust (matching the activity pane's pattern) so we
    // never feed `usize::MAX` into `Paragraph::scroll` — that overflows
    // ratatui's `offset_y + height` math and panics the TUI.
    // Offset-from-top: `info_scroll == 0` shows the first rows, the
    // max shows the last. The precise upper bound
    // (`lines.len() - inner_height`) is computed in the same scope that
    // owns `lines`, so the clamp is exact.
    let inner_height = inner.height as usize;
    let max_skip = lines.len().saturating_sub(inner_height);
    // `scroll` and `skip` are read while `lines` is still in scope
    // (lines borrows app via the ThreadSummary snapshot). Write the
    // clamped value back after rendering, when the borrow has ended.
    let scroll = app.chat.info_scroll;
    let skip = scroll.min(max_skip);
    let visible_lines: Vec<Line> = lines.into_iter().skip(skip).collect();
    // `Wrap { trim: false }` is required so the leading 2-space prefix
    // on `Modified` rows survives (default `trim: true` strips
    // leading whitespace per the `Wrap` doc). Wrap is still needed
    // for long paths that exceed the 20%-wide pane.
    let paragraph = Paragraph::new(visible_lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
    app.chat.info_scroll = skip;
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

/// Zero-alloc snapshot of the data the chat header needs. All fields
/// borrow directly from the polled `InspectOverview`. Missing fields
/// fall back to placeholders so the header still reads as
/// `╭─ build · local_dev · pattern` before the first poll.
struct ChatHeaderCtx<'a> {
    mode: &'a str,
    channel: Option<&'a str>,
    pattern: Option<&'a str>,
    branch: Option<&'a str>,
    model: Option<&'a str>,
    pct: Option<u32>,
}

fn resolve_header_ctx(app: &App) -> ChatHeaderCtx<'_> {
    let t = selected_thread_summary(app);
    ChatHeaderCtx {
        mode: t.and_then(|t| t.mode.as_deref()).unwrap_or("build"),
        channel: t.map(|t| t.channel.as_str()),
        pattern: t.and_then(|t| t.pattern.as_deref()),
        // Server resolves branch per poll — read it straight off the summary.
        branch: t.and_then(|t| t.branch.as_deref()),
        model: t.and_then(|t| t.model.as_deref()),
        pct: t.and_then(input_token_pct),
    }
}

/// Build the chat header row: "╭─ {mode} · {channel} · {pattern}[ · {branch}]"
/// left-aligned, ─ padding filling the rest of the chat-pane width, and
/// a right-aligned "[ {model} · {pct}% ]" chip showing the current model
/// and context-window usage. No bottom or right border. Falls back
/// gracefully when any field is missing — the chip is dropped before
/// the left segment starts truncating.
fn build_chat_header_line(
    width: usize,
    ctx: &ChatHeaderCtx<'_>,
    header_style: Style,
    line_style: Style,
) -> Line<'static> {
    // --- Left segment: "╭─ {mode} · {channel} · {pattern}[ · {branch}]" ---
    // Divergence from the Thread Info pane: when `pattern` is `None`
    // we omit the segment entirely instead of rendering "-". The
    // header is width-constrained, so omitting the segment looks
    // cleaner than `╭─ plan · local_dev · -`.
    let mut left = String::with_capacity(48);
    left.push_str(ctx.mode);
    if let Some(ch) = ctx.channel {
        left.push_str(" · ");
        left.push_str(ch);
    }
    if let Some(pat) = ctx.pattern {
        left.push_str(" · ");
        left.push_str(pat);
    }
    if let Some(branch) = ctx.branch {
        left.push_str(" · ");
        left.push_str(branch);
    }
    // The "╭─ " prefix is accounted for separately so it can be styled in
    // the line-drawing color (3 display columns).
    let left_w = 3 + left.width();

    // --- Right chip: "[ {model} · {pct}% ]" ---
    // Omit the chip entirely when both fields are missing (e.g., before
    // the first poll). When only one is set, render a partial chip
    // showing the available side. Matches the info-pane convention of
    // skipping rows for missing data.
    let chip: Option<String> = match (ctx.model, ctx.pct) {
        (Some(m), Some(p)) => Some(format!("[ {m} · {p}% ]")),
        (Some(m), None) => Some(format!("[ {m} ]")),
        (None, Some(p)) => Some(format!("[ {p}% ]")),
        (None, None) => None,
    };
    let chip_w = chip.as_ref().map(|c| c.width()).unwrap_or(0);

    // Width budget: pad = width - left - chip. If negative (or zero, so
    // we can't fit a space separator), drop the chip first, then
    // truncate the left segment.
    if width < left_w + chip_w + 1 {
        // Try without the chip.
        if width >= left_w {
            return Line::from(vec![
                Span::styled("╭─", line_style),
                Span::styled(format!(" {left}"), header_style),
                Span::styled("─".repeat(width.saturating_sub(left_w + 1)), line_style),
            ]);
        }
        // Left itself doesn't fit; best-effort segments over
        // [channel, pattern, branch], adding the separator only when there is
        // room for at least one column of content after it.
        let mut compact = ctx.mode.to_string();
        for seg in [ctx.channel, ctx.pattern, ctx.branch].into_iter().flatten() {
            // +3 accounts for the "╭─ " prefix.
            let used = 3 + compact.width();
            // Need room for " · " (3 cols) plus at least 1 col of content.
            if width < used + 4 {
                break;
            }
            let avail = width - used - 3;
            compact.push_str(" · ");
            compact.push_str(&truncate_to_width(seg, avail));
        }
        return Line::from(vec![
            Span::styled("╭─", line_style),
            Span::styled(format!(" {compact}"), header_style),
        ]);
    }

    let pad = width - left_w - chip_w;
    let mut spans = Vec::with_capacity(5);
    spans.push(Span::styled("╭─", line_style));
    spans.push(Span::styled(format!(" {left}"), header_style));
    // Separator between left segment and chip. Always emit at least
    // a single space when the chip is rendered (so it never sits flush
    // against the left); fill the gap with `─` runs when there's room.
    match chip.as_deref() {
        Some(_) if pad >= 2 => {
            spans.push(Span::styled(" ", header_style));
            spans.push(Span::styled("─".repeat(pad - 2), line_style));
            spans.push(Span::styled(" ", header_style));
        }
        Some(_) if pad == 1 => {
            spans.push(Span::styled(" ", header_style));
        }
        // pad == 0 with chip: no separator; line was packed exactly.
        Some(c) => {
            spans.push(Span::styled(c.to_string(), header_style));
            return Line::from(spans);
        }
        None if pad > 0 => {
            // No chip, but padding available — fill with dashes.
            spans.push(Span::styled("─".repeat(pad), line_style));
        }
        None => {}
    }
    if let Some(c) = chip {
        spans.push(Span::styled(c, header_style));
    }
    Line::from(spans)
}

/// Truncate `s` to at most `max_width` display columns (per
/// `unicode-width`); if the input is wider, replace the tail with `…`.
fn truncate_to_width(s: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if max_width == 0 {
        return String::new();
    }
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let keep = max_width - 1; // reserve 1 col for the ellipsis
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > keep {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push('…');
    out
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
    // Input area = 1 mode header row ("╭─ build") + editor rows (grows with
    // content, up to 10). Subtract the prompt gutter from the wrap width.
    let input_line_count = (count_wrapped_lines(
        &app.chat.text(),
        area.width.saturating_sub(PROMPT_GUTTER_WIDTH),
    ) + 1)
        .clamp(2, 11) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(input_line_count)])
        .split(area);
    // Cache the message-area rect so mouse-wheel events can hit-test
    // against it from the input loop.
    app.chat.last_message_area = Some(chunks[0]);

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
        // has the duration right-aligned with breathing space:
        // "──────── 1m ──"
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
                // <dashes> <elapsed> ──
                let dash_count = width.saturating_sub(elapsed.len() + 3);
                all_lines.push(Line::from(vec![
                    Span::styled(format!("{} ", "─".repeat(dash_count)), dim_style),
                    Span::styled(elapsed, dim_style),
                    Span::styled(" ──", dim_style),
                ]));
            }
            all_lines.push(Line::from(""));
            group_start_ts = None;
        }

        // Open new round at the start of a user turn. Top rule has the
        // timestamp left-aligned with breathing space:
        // "── 09:50 ────────"
        if is_user {
            group_start_ts = msg.timestamp.clone();
            let time_str = format_msg_time(&msg.timestamp);
            let width = chunks[0].width as usize;
            if time_str.is_empty() {
                all_lines.push(Line::from(Span::styled("─".repeat(width), dim_style)));
            } else {
                // ── <time> <dashes>
                let dash_count = width.saturating_sub(time_str.len() + 3);
                all_lines.push(Line::from(vec![
                    Span::styled("── ", dim_style),
                    Span::styled(time_str, dim_style),
                    Span::styled(format!(" {}", "─".repeat(dash_count)), dim_style),
                ]));
            }
        }

        // Separator between the user message and the AI response within
        // a round: a light dashed rule, visually subordinate to the
        // solid "─" round rules.
        if !is_user && prev_sender == Some("user") {
            let width = chunks[0].width as usize;
            all_lines.push(Line::from(""));
            all_lines.push(Line::from(Span::styled("┄".repeat(width), dim_style)));
            all_lines.push(Line::from(""));
        }

        // Render message (no side gutters).
        let md_text = format!("{prefix}{}\n", msg.text);
        let blocks = renderer.parse(&md_text);
        let msg_lines = renderer.render(&blocks, &theme);
        all_lines.extend(msg_lines);
    }

    // Close any open round at the end (same bottom-rule format as above).
    if group_start_ts.is_some() {
        let last_ts = app.chat.messages.last().and_then(|m| m.timestamp.clone());
        let elapsed = format_group_elapsed(&group_start_ts, &last_ts);
        let width = chunks[0].width as usize;
        if elapsed.is_empty() {
            let dashes = "─".repeat(width);
            all_lines.push(Line::from(Span::styled(dashes, dim_style)));
        } else {
            let dash_count = width.saturating_sub(elapsed.len() + 3);
            all_lines.push(Line::from(vec![
                Span::styled(format!("{} ", "─".repeat(dash_count)), dim_style),
                Span::styled(elapsed, dim_style),
                Span::styled(" ──", dim_style),
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

        // Live wall-clock ticker (1 Hz, with the first tick at t=0). When
        // present, it is the authoritative elapsed-time display for the
        // last in-progress line — the polled `last_active_at` it
        // normally shows goes stale during silent LLM/tool work. Falls
        // back to the polled value when no tick has arrived yet.
        let live_tick_ms: Option<u64> = live_chan
            .as_deref()
            .zip(live_thread.as_deref())
            .and_then(|(c, t)| app.chat.live_tick_ms_for(c, t));

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
            // Pre-activity placeholder: shown only between ProcessingStarted
            // and the first ToolStarted/LLMRequestStarted event. There's
            // no "since-event" numerator to pair with, so we display the
            // live ticker alone in `(12.4s)` form. This intentionally
            // diverges from the dual-time format below — there's literally
            // no `a.timestamp` to compute the left half from.
            let placeholder = match live_tick_ms {
                Some(ms) => format!("⏳ AI is thinking... ({})", format_elapsed_ms(ms)),
                None => "⏳ AI is thinking...".to_string(),
            };
            all_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    placeholder,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        } else {
            let total = activity_entries.len();
            for (idx, a) in activity_entries.iter().enumerate() {
                let is_last = idx == total - 1;
                // Dual-time display on the last in-progress line:
                //   left  = time since the most recent activity event
                //           (polled `last_active_at` — coarse, freezes
                //           during silent LLM/tool work)
                //   right = wall-clock elapsed since the loop started
                //           (live ticker at 1 Hz — fresh throughout)
                // Diverging numbers mean the loop is in a long silent
                // stretch: the polled one stops moving, the live one
                // keeps ticking. The single-value fallback (`X`) is
                // preserved for the case where neither source has
                // produced a usable timestamp yet.
                let elapsed = if is_last {
                    let since_event = format_elapsed(&a.timestamp);
                    match (since_event.is_empty(), live_tick_ms) {
                        (false, Some(ms)) => format!("{} / {}", since_event, format_elapsed_ms(ms)),
                        (false, None) => since_event,
                        (true, Some(ms)) => format_elapsed_ms(ms),
                        (true, None) => String::new(),
                    }
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
    // default inverted block otherwise; hidden when the input field does
    // not have focus. A two-line prompt gutter sits left of the editor:
    // the header row shows "╭─ {mode} · {channel} · {pattern}[ · {branch}]",
    // and "╰─❯" (Insert mode) / "╰─❮" (other vim modes) on the first editor
    // row; both dim when the input field loses focus.
    let theme = EditorTheme::default()
        .base(Style::default())
        .hide_status_line();
    let theme = match app.chat.focus {
        ChatFocus::MessageArea
        | ChatFocus::ActivityPane
        | ChatFocus::ExplorerPane
        | ChatFocus::InfoPane => theme.hide_cursor(),
        ChatFocus::ChatPane => match app.chat.editor.mode {
            EditorMode::Insert => theme.cursor_style(
                Style::default()
                    .add_modifier(Modifier::UNDERLINED)
                    .add_modifier(Modifier::SLOW_BLINK),
            ),
            _ => theme,
        },
    };
    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(chunks[1]);
    let [prompt_area, editor_area] =
        Layout::horizontal([Constraint::Length(PROMPT_GUTTER_WIDTH), Constraint::Min(0)])
            .areas(body_area);
    let focused = app.chat.focus == ChatFocus::ChatPane;
    // Resolve mode/channel/pattern/model/tokens for the header line, all
    // from the polled overview (same source as the Thread Info pane).
    let header_ctx = resolve_header_ctx(app);
    // Header info text: sapphire + bold when focused, otherwise the same
    // inactive #393552 as the line-drawing characters.
    let header_style = if focused {
        Style::default()
            .fg(Color::Rgb(116, 199, 236)) // Catppuccin sapphire
            .add_modifier(Modifier::BOLD)
    } else {
        LINE_DRAWING
    };
    // Box-drawing characters (header border + gutter line) match the
    // message-area separator color when focused, and go inactive (#393552)
    // alongside the text when focus moves away.
    let line_style = if focused {
        Style::default().fg(Color::DarkGray)
    } else {
        LINE_DRAWING
    };
    let header_line = build_chat_header_line(
        header_area.width as usize,
        &header_ctx,
        header_style,
        line_style,
    );
    frame.render_widget(Paragraph::new(header_line), header_area);
    // Vim-mode arrow: "╰─❯ " in Insert mode, "╰─❮ " otherwise. The
    // box-drawing prefix uses the focus-dependent line style, the arrow is
    // yellow when focused and dims to #393552 when not. The full vim-mode
    // chip lives in the status bar.
    let arrow_style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        LINE_DRAWING
    };
    let arrow = if app.chat.editor.mode == EditorMode::Insert {
        "❯ "
    } else {
        "❮ "
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("╰─", line_style),
            Span::styled(arrow, arrow_style),
        ])),
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

    // ── Leader-key popup overlay (TUI-local commands) ──
    if let Some(ref leader) = app.chat.leader {
        leader.render(frame, area);
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
    // Borders::TOP subtracts one row from the inner area.
    let inner_height = area.height.saturating_sub(1) as usize;
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
    // Only the top edge (against the chat pane) gets a border. Bottom,
    // left, and right are at the screen edge or redundant.
    let mut block = Block::default().title("── Activity ").borders(Borders::TOP);
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

    // Borders::TOP subtracts one row from the inner area.
    let inner_height = area.height.saturating_sub(1) as usize;
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
            info_scroll: 0,
            activity_scroll: 0,
            last_message_area: None,
            pending_g: false,
            activity_hscroll: 0,
            awaiting_response: false,
            activity_split: 0,
            info_visible: true,
            status_visible: true,
            explorer_visible: false,
            zen_saved: None,
            explorer_selected: 0,
            ws_tx: None,
            ws_rx,
            ws_connected: false,
            live_activity: std::collections::BTreeMap::new(),
            live_chat: std::collections::BTreeMap::new(),
            live_thinking: std::collections::BTreeMap::new(),
            live_processing: std::collections::BTreeMap::new(),
            live_tick_ms: std::collections::BTreeMap::new(),
            last_seen_id: std::collections::BTreeMap::new(),
            last_hydrated_key: None,
            open_addr: None,
            commands: vec![],
            models: vec![],
            command_popup: None,
            leader: None,
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
        self.info_scroll = 0;
        self.last_message_area = None;
        self.activity_hscroll = 0;
        self.pending_g = false;
        self.activity_split = 0;
        self.info_visible = true;
        self.status_visible = true;
        self.zen_saved = None;
        self.ws_connected = false;
        self.input_history.clear();
        self.history_pos = None;
        // Clear the poll-loop's last-hydrated key so it doesn't skip hydrate
        // when we switch back to overview later.
        self.last_hydrated_key = None;
        // Stash addr so the explorer pane can switch threads later.
        self.open_addr = Some(addr.to_string());

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
        client: &InspectClient,
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
        self.info_scroll = 0;
        self.last_message_area = None;
        self.activity_hscroll = 0;
        self.pending_g = false;
        self.activity_split = 0;
        self.info_visible = true;
        self.status_visible = true;
        self.zen_saved = None;
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
        self.last_hydrated_key = None;
        if let Some(tx) = self.ws_tx.take() {
            // Best-effort disconnect signal
            let _ = tx.send("{\"type\":\"disconnect\"}".to_string());
        }
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

    /// Cycle focus: Input → MessageArea → InfoPane → ActivityPane →
    /// ExplorerPane → Input. Each pane is skipped when it is hidden so
    /// the cycle never lands on an invisible pane. The info pane's
    /// "hidden" state is `!self.info_visible`; the activity pane's is
    /// `self.activity_split == 0`.
    pub(super) fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            ChatFocus::ChatPane => ChatFocus::MessageArea,
            ChatFocus::MessageArea => {
                if self.info_visible {
                    ChatFocus::InfoPane
                } else if self.activity_split != 0 {
                    ChatFocus::ActivityPane
                } else if self.explorer_visible {
                    ChatFocus::ExplorerPane
                } else {
                    ChatFocus::ChatPane
                }
            }
            ChatFocus::InfoPane => {
                if self.activity_split != 0 {
                    ChatFocus::ActivityPane
                } else if self.explorer_visible {
                    ChatFocus::ExplorerPane
                } else {
                    ChatFocus::ChatPane
                }
            }
            ChatFocus::ActivityPane => {
                if self.explorer_visible {
                    ChatFocus::ExplorerPane
                } else {
                    ChatFocus::ChatPane
                }
            }
            ChatFocus::ExplorerPane => ChatFocus::ChatPane,
        };
    }

    /// Toggle the activity pane on/off. Showing it restores the bottom 20%
    /// size (`activity_split = 1`); hiding it zeroes the state and moves
    /// focus back to the input field when the pane was focused, or when
    /// the info pane was focused (since the info pane sits "behind" the
    /// activity pane in the focus cycle, hiding the activity pane would
    /// otherwise leave focus on a pane whose neighbor just disappeared).
    pub(super) fn toggle_activity(&mut self) {
        if self.activity_split == 0 {
            self.activity_split = 1;
        } else {
            self.activity_split = 0;
            if self.focus == ChatFocus::ActivityPane || self.focus == ChatFocus::InfoPane {
                self.focus = ChatFocus::ChatPane;
            }
        }
    }

    /// Toggle zen mode. Entering zen snapshots the aux-pane state
    /// (activity, thread info, status bar, explorer) and hides all of
    /// them, leaving only the chat pane. Exiting zen restores the
    /// snapshot exactly — panes toggled individually while in zen are
    /// discarded in favor of the snapshot.
    pub(super) fn toggle_zen_mode(&mut self) {
        if let Some(saved) = self.zen_saved.take() {
            self.activity_split = saved.activity_split;
            self.info_visible = saved.info_visible;
            self.status_visible = saved.status_visible;
            self.explorer_visible = saved.explorer_visible;
            return;
        }
        self.zen_saved = Some(ZenSnapshot {
            activity_split: self.activity_split,
            info_visible: self.info_visible,
            status_visible: self.status_visible,
            explorer_visible: self.explorer_visible,
        });
        self.activity_split = 0;
        self.info_visible = false;
        self.status_visible = false;
        self.explorer_visible = false;
        if self.focus == ChatFocus::ActivityPane
            || self.focus == ChatFocus::InfoPane
            || self.focus == ChatFocus::ExplorerPane
        {
            self.focus = ChatFocus::ChatPane;
        }
    }

    /// Toggle the bottom status bar (leader-key popup `s`).
    pub(super) fn toggle_status_bar(&mut self) {
        self.status_visible = !self.status_visible;
    }

    /// Toggle the thread info pane (leader-key popup `i`). Hiding it
    /// while focused moves focus back to the chat pane.
    pub(super) fn toggle_info_pane(&mut self) {
        self.info_visible = !self.info_visible;
        if !self.info_visible && self.focus == ChatFocus::InfoPane {
            self.focus = ChatFocus::ChatPane;
        }
    }

    /// Toggle the thread explorer pane (left side). Opening moves focus
    /// into it so j/k/Enter are immediately usable; closing returns
    /// focus to the chat input.
    pub(super) fn toggle_explorer(&mut self) {
        self.explorer_visible = !self.explorer_visible;
        if self.explorer_visible {
            self.focus = ChatFocus::ExplorerPane;
        } else if self.focus == ChatFocus::ExplorerPane {
            self.focus = ChatFocus::ChatPane;
        }
    }

    pub(super) fn scroll_up(&mut self) {
        match self.focus {
            ChatFocus::ChatPane | ChatFocus::MessageArea => {
                self.scroll = self.scroll.saturating_add(1)
            }
            ChatFocus::ActivityPane => {
                self.activity_scroll = self.activity_scroll.saturating_add(1)
            }
            // Info pane uses offset-from-top semantics (vs activity's
            // offset-from-bottom). `scroll_up` → earlier rows → smaller
            // offset.
            ChatFocus::InfoPane => self.info_scroll = self.info_scroll.saturating_sub(1),
            ChatFocus::ExplorerPane => {}
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
            ChatFocus::ChatPane | ChatFocus::MessageArea => {
                self.scroll = self.scroll.saturating_sub(1)
            }
            ChatFocus::ActivityPane => {
                self.activity_scroll = self.activity_scroll.saturating_sub(1)
            }
            // Info pane: scroll down → later rows → larger offset.
            ChatFocus::InfoPane => self.info_scroll = self.info_scroll.saturating_add(1),
            ChatFocus::ExplorerPane => {}
        }
    }

    /// Jump to the oldest message (top) of the focused pane.
    ///
    /// The offset is clamped to the actual maximum during rendering, so
    /// setting it to `usize::MAX` is a safe "scroll all the way up".
    pub(super) fn scroll_to_top(&mut self) {
        match self.focus {
            ChatFocus::ChatPane | ChatFocus::MessageArea => self.scroll = usize::MAX,
            ChatFocus::ActivityPane => self.activity_scroll = usize::MAX,
            // Info pane: offset-from-top, so "top" is offset = 0.
            ChatFocus::InfoPane => self.info_scroll = 0,
            ChatFocus::ExplorerPane => {}
        }
    }

    /// Jump to the latest message (bottom) of the focused pane.
    pub(super) fn scroll_to_bottom(&mut self) {
        match self.focus {
            ChatFocus::ChatPane | ChatFocus::MessageArea => self.scroll = 0,
            ChatFocus::ActivityPane => self.activity_scroll = 0,
            // Info pane: clamped to max in render.
            ChatFocus::InfoPane => self.info_scroll = usize::MAX,
            ChatFocus::ExplorerPane => {}
        }
    }

    pub(super) fn page_size(&self) -> usize {
        let base = crossterm::terminal::size()
            .map(|(_, h)| h.saturating_sub(7) as usize)
            .unwrap_or(10);
        match self.focus {
            ChatFocus::ChatPane | ChatFocus::MessageArea => {
                let term_width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
                // Editor rows: 1 mode header row + wrapped text lines (1-10).
                // Subtract the prompt gutter from the width.
                let input_lines = (count_wrapped_lines(
                    &self.text(),
                    term_width.saturating_sub(PROMPT_GUTTER_WIDTH),
                ) + 1)
                    .clamp(2, 11);
                base.saturating_sub(input_lines).max(1)
            }
            ChatFocus::ActivityPane | ChatFocus::ExplorerPane | ChatFocus::InfoPane => base.max(1),
        }
    }

    pub(super) fn page_up(&mut self) {
        let page = self.page_size();
        match self.focus {
            ChatFocus::ChatPane | ChatFocus::MessageArea => {
                self.scroll = self.scroll.saturating_add(page)
            }
            ChatFocus::ActivityPane => {
                self.activity_scroll = self.activity_scroll.saturating_add(page)
            }
            // Info pane: offset-from-top, so "page up" → smaller offset.
            ChatFocus::InfoPane => self.info_scroll = self.info_scroll.saturating_sub(page),
            ChatFocus::ExplorerPane => {}
        }
    }

    pub(super) fn page_down(&mut self) {
        let page = self.page_size();
        match self.focus {
            ChatFocus::ChatPane | ChatFocus::MessageArea => {
                self.scroll = self.scroll.saturating_sub(page)
            }
            ChatFocus::ActivityPane => {
                self.activity_scroll = self.activity_scroll.saturating_sub(page)
            }
            // Info pane: offset-from-top, so "page down" → larger offset.
            ChatFocus::InfoPane => self.info_scroll = self.info_scroll.saturating_add(page),
            ChatFocus::ExplorerPane => {}
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
            | Some("resync") | Some("loop_tick") => {
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
                    self.live_tick_ms.remove(&key);
                    self.awaiting_response = false;
                } else {
                    // New round started - also clear thinking (in case
                    // the first Thinking event for this round is delayed).
                    self.live_thinking.remove(&key);
                    self.live_tick_ms.remove(&key);
                }
            }
            "loop_tick" => {
                // Live wall-clock duration (1 Hz while the loop is alive, with the
                // first tick fired immediately at t=0). Drives the
                // duration ticker in the dashboard Details panel,
                // chat-mode info pane, and chat progress line.
                if let Some(ms) = payload.get("elapsed_ms").and_then(|v| v.as_u64()) {
                    self.live_tick_ms.insert(key, ms);
                }
            }
            "resync" => {
                // Server fell behind (Lagged); clear local state so the
                // caller re-hydrates via REST.
                self.live_activity.remove(&key);
                self.live_chat.remove(&key);
                self.live_thinking.remove(&key);
                self.live_processing.remove(&key);
                self.live_tick_ms.remove(&key);
                self.last_seen_id.remove(&key);
            }
            _ => {}
        }
    }

    /// Clear the transient per-thread live state (`live_thinking` and
    /// `live_processing`) without touching the activity/chat buffers.
    ///
    /// Called on REST hydrate when switching threads: `live_processing` is
    /// only updated by WS `processing` events received while the thread is
    /// watched, so entries go stale while unwatched (a missed completion
    /// leaves `true` → phantom progress; a missed start leaves `false` →
    /// no progress). Clearing makes the renderer fall back to the polled
    /// overview status until fresh WS events arrive.
    pub(super) fn clear_live_transient(&mut self, channel: &str, thread: &str) {
        let key = (channel.to_string(), thread.to_string());
        self.live_thinking.remove(&key);
        self.live_processing.remove(&key);
        self.live_tick_ms.remove(&key);
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
    /// Get the live wall-clock elapsed time (milliseconds) for an active
    /// agent loop on the given (channel, thread). Returns `None` when no
    /// tick has arrived yet (loop just started) or the loop has ended.
    /// Used by all three render sites: the dashboard Details panel, the
    /// chat-mode info pane, and the chat progress line.
    pub(super) fn live_tick_ms_for(&self, channel: &str, thread: &str) -> Option<u64> {
        self.live_tick_ms
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
    fn format_elapsed_ms_below_60s() {
        assert_eq!(format_elapsed_ms(0), "0.0s");
        assert_eq!(format_elapsed_ms(250), "0.2s");
        assert_eq!(format_elapsed_ms(999), "0.9s");
        assert_eq!(format_elapsed_ms(12_400), "12.4s");
        assert_eq!(format_elapsed_ms(59_999), "59.9s");
    }

    #[test]
    fn format_elapsed_ms_at_and_above_60s() {
        assert_eq!(format_elapsed_ms(60_000), "1m00s");
        assert_eq!(format_elapsed_ms(65_000), "1m05s");
        assert_eq!(format_elapsed_ms(125_000), "2m05s");
        assert_eq!(format_elapsed_ms(3_600_000), "60m00s");
    }

    #[test]
    fn live_tick_ms_for_round_trip() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut chat = ChatState::new(rx);
        // Seed via the WS handler entry-point so we cover the production
        // path, not a direct map insert.
        let payload = serde_json::json!({
            "type": "loop_tick",
            "channel": "chan",
            "thread": "t1",
            "elapsed_ms": 12_400,
        });
        chat.handle_live_event(&payload);
        assert_eq!(chat.live_tick_ms_for("chan", "t1"), Some(12_400));
        assert_eq!(chat.live_tick_ms_for("chan", "missing"), None);

        // `processing: false` should clear the tick (mirror of new-round).
        chat.handle_live_event(&serde_json::json!({
            "type": "processing",
            "channel": "chan",
            "thread": "t1",
            "is_processing": false,
            "has_error": false,
        }));
        assert_eq!(chat.live_tick_ms_for("chan", "t1"), None);

        // And a second tick updates the value.
        chat.handle_live_event(&serde_json::json!({
            "type": "loop_tick",
            "channel": "chan",
            "thread": "t1",
            "elapsed_ms": 7_500,
        }));
        assert_eq!(chat.live_tick_ms_for("chan", "t1"), Some(7_500));
    }

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
    fn tab_cycles_input_messages_activity_when_activity_visible() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.activity_split = 1; // activity pane visible

        assert_eq!(app.chat.focus, ChatFocus::ChatPane);
        app.chat.toggle_focus();
        assert_eq!(app.chat.focus, ChatFocus::MessageArea);
        app.chat.toggle_focus();
        assert_eq!(app.chat.focus, ChatFocus::ActivityPane);
        app.chat.toggle_focus();
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);
    }

    #[test]
    fn tab_skips_hidden_activity_pane() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        assert_eq!(app.chat.activity_split, 0); // activity hidden

        app.chat.toggle_focus();
        assert_eq!(app.chat.focus, ChatFocus::MessageArea);
        app.chat.toggle_focus();
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);
    }

    #[test]
    fn hiding_activity_refocuses_input() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        // Activity visible and focused.
        app.chat.activity_split = 1;
        app.chat.focus = ChatFocus::ActivityPane;
        // Toggle off — focus must fall back to the input field.
        app.chat.toggle_activity();
        assert_eq!(app.chat.activity_split, 0);
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);

        // Same guard when entering zen mode with the activity pane focused.
        app.chat.activity_split = 1;
        app.chat.info_visible = true;
        app.chat.focus = ChatFocus::ActivityPane;
        app.chat.toggle_zen_mode();
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);
    }

    #[test]
    fn tab_cycles_through_info_pane_when_visible() {
        // info_visible defaults to false; flip it on so the cycle stops.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.info_visible = true;
        // activity hidden, explorer hidden → cycle is Chat → MessageArea
        // → InfoPane → Chat (per the skip rules).
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);
        app.chat.toggle_focus();
        assert_eq!(app.chat.focus, ChatFocus::MessageArea);
        app.chat.toggle_focus();
        assert_eq!(app.chat.focus, ChatFocus::InfoPane);
        app.chat.toggle_focus();
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);
    }

    #[test]
    fn tab_skips_info_pane_when_hidden() {
        // info_visible defaults to false; cycle must skip InfoPane.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);
        app.chat.toggle_focus();
        assert_eq!(app.chat.focus, ChatFocus::MessageArea);
        app.chat.toggle_focus();
        assert_eq!(
            app.chat.focus,
            ChatFocus::ChatPane,
            "info pane must be skipped when info_visible=false"
        );
    }

    #[test]
    fn info_pane_scroll_uses_offset_from_top() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.focus = ChatFocus::InfoPane;
        assert_eq!(app.chat.info_scroll, 0);
        // scroll_down → later rows → larger offset.
        app.chat.scroll_down();
        assert_eq!(app.chat.info_scroll, 1);
        // scroll_up → earlier rows → smaller offset (toward 0).
        app.chat.scroll_up();
        assert_eq!(app.chat.info_scroll, 0);
        // scroll_up at 0 stays at 0 (saturating).
        app.chat.scroll_up();
        assert_eq!(app.chat.info_scroll, 0);
        // scroll_to_top pins to 0.
        app.chat.info_scroll = 5;
        app.chat.scroll_to_top();
        assert_eq!(app.chat.info_scroll, 0);
        // scroll_to_bottom overshoots; render clamps.
        app.chat.scroll_to_bottom();
        assert_eq!(app.chat.info_scroll, usize::MAX);
    }

    #[test]
    fn hiding_info_pane_via_zen_falls_back_to_chat() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.info_visible = true;
        app.chat.focus = ChatFocus::InfoPane;
        app.chat.toggle_zen_mode();
        assert_eq!(
            app.chat.focus,
            ChatFocus::ChatPane,
            "info-focused pane must not be left dangling when zen hides it"
        );
    }

    #[test]
    fn hiding_activity_refocuses_input_from_info_pane() {
        // Mirrors `hiding_activity_refocuses_input` for the InfoPane
        // focus — hiding the activity pane while InfoPane is focused
        // must also fall back to the chat input (because InfoPane
        // sits "behind" ActivityPane in the cycle).
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.activity_split = 1;
        app.chat.info_visible = true;
        app.chat.focus = ChatFocus::InfoPane;
        app.chat.toggle_activity();
        assert_eq!(app.chat.activity_split, 0);
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);
    }

    #[test]
    fn message_area_scrolls_chat_history() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.focus = ChatFocus::MessageArea;
        app.chat.scroll_to_top();
        assert_eq!(app.chat.scroll, usize::MAX);
        app.chat.scroll_to_bottom();
        assert_eq!(app.chat.scroll, 0);
        app.chat.scroll_up();
        assert_eq!(app.chat.scroll, 1);
        app.chat.scroll_down();
        assert_eq!(app.chat.scroll, 0);
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
    fn clear_live_transient_removes_stale_state_for_switched_thread() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        // Stale entries from earlier watches of two threads:
        // - "done": missed completion event → phantom (true) progress
        // - "busy": missed start event → false suppresses overview fallback
        let done = ("chan".to_string(), "done".to_string());
        let busy = ("chan".to_string(), "busy".to_string());
        app.chat.live_processing.insert(done.clone(), (true, false));
        app.chat
            .live_thinking
            .insert(done.clone(), "old thinking".into());
        app.chat
            .live_processing
            .insert(busy.clone(), (false, false));
        app.chat
            .live_activity
            .insert(done.clone(), Default::default());

        // Switching to "done" hydrates it: transient state must clear so
        // the renderer falls back to the polled overview status.
        app.chat.clear_live_transient("chan", "done");

        assert!(app.chat.live_processing_for("chan", "done").is_none());
        assert!(app.chat.live_thinking_for("chan", "done").is_none());
        // Activity/chat buffers are preserved (re-seeded by REST hydrate).
        assert!(app.chat.live_activity.contains_key(&done));
        // Other threads' live state is untouched.
        assert_eq!(
            app.chat.live_processing_for("chan", "busy"),
            Some((false, false))
        );
    }

    fn esc_key() -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn test_terminal() -> Terminal<ratatui::backend::TestBackend> {
        Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap()
    }

    #[test]
    fn esc_does_not_close_chat_in_editor_normal_mode() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.visible = true;
        app.chat.phase = ChatPhase::Chatting;
        app.chat.thread = Some("jyc".to_string());
        app.chat.focus = ChatFocus::ChatPane;
        app.chat.editor.mode = EditorMode::Normal;

        handle_chat_keys(&mut app, esc_key(), &mut test_terminal());
        assert!(app.chat.visible, "Esc must not close the chat screen");
    }

    #[test]
    fn esc_does_not_close_chat_in_activity_pane() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.visible = true;
        app.chat.phase = ChatPhase::Chatting;
        app.chat.thread = Some("jyc".to_string());
        app.chat.focus = ChatFocus::ActivityPane;

        handle_chat_keys(&mut app, esc_key(), &mut test_terminal());
        assert!(app.chat.visible, "Esc must not close the chat screen");
        assert_eq!(app.chat.focus, ChatFocus::ActivityPane);
    }

    fn mouse_event(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn mouse_scroll_in_message_area_advances_scroll_offset() {
        // Render the chat pane to a 80x24 backend; the message area sits
        // above the input editor.
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.visible = true;
        app.chat.phase = ChatPhase::Chatting;
        app.chat.thread = Some("jyc".to_string());
        // Focus the input so the wheel hit-test is the only thing moving
        // focus, mirroring the user experience of scrolling with the
        // cursor over the message area while typing into the input.
        app.chat.focus = ChatFocus::ChatPane;
        app.chat.scroll = 0;
        // Need at least a couple of messages so the scroll offset is
        // non-zero once we step up.
        app.chat.messages.push(ChatMessage {
            sender: "user".into(),
            text: "hi".into(),
            timestamp: Some("2026-01-01T00:00:00Z".into()),
        });
        app.chat.messages.push(ChatMessage {
            sender: "ai".into(),
            text: "hello".into(),
            timestamp: Some("2026-01-01T00:00:01Z".into()),
        });

        terminal
            .draw(|f| ui_chat_mode(f, f.area(), &mut app))
            .unwrap();
        let rect = app
            .chat
            .last_message_area
            .expect("render should cache the message rect");
        // Hit-test inside the message rect.
        let inside = mouse_event(MouseEventKind::ScrollUp, rect.x + 1, rect.y);
        let outside = mouse_event(MouseEventKind::ScrollUp, rect.x, rect.y + rect.height);

        handle_chat_mouse(&mut app, inside);
        assert_eq!(app.chat.scroll, 1, "wheel-up over message area scrolls up");
        handle_chat_mouse(&mut app, outside);
        assert_eq!(
            app.chat.scroll, 1,
            "wheel outside the message area must be ignored"
        );
        handle_chat_mouse(
            &mut app,
            mouse_event(MouseEventKind::ScrollDown, rect.x + 1, rect.y),
        );
        assert_eq!(
            app.chat.scroll, 0,
            "wheel-down over message area scrolls down"
        );
    }

    #[test]
    fn mouse_scroll_ignored_outside_chatting_phase() {
        // PatternSelect has no scrollable message area; the wheel must
        // not change focus or scroll state.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.visible = true;
        app.chat.phase = ChatPhase::PatternSelect;
        app.chat.focus = ChatFocus::ChatPane;
        app.chat.scroll = 0;

        handle_chat_mouse(&mut app, mouse_event(MouseEventKind::ScrollUp, 10, 10));
        assert_eq!(app.chat.scroll, 0);
    }

    #[test]
    fn mouse_capture_defaults_to_on() {
        // PR #484 enabled capture at startup; the toggle should only
        // opt out, not change the default.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let app = App::new(rx, None);
        assert!(
            app.mouse_capture_enabled,
            "default mouse_capture_enabled must be true"
        );
    }

    #[test]
    fn mouse_capture_flip_is_pure_state_change() {
        // `flip_mouse_capture` must not perform I/O — it only toggles
        // the bool and returns the new state. Tests can't observe the
        // escape write, but they can verify the flag and return value.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        assert!(app.mouse_capture_enabled);
        assert!(!app.flip_mouse_capture(), "first flip turns capture off");
        assert!(!app.mouse_capture_enabled);
        assert!(
            app.flip_mouse_capture(),
            "second flip turns capture back on"
        );
        assert!(app.mouse_capture_enabled);
    }

    #[test]
    fn mouse_scroll_ignored_when_capture_disabled() {
        // The defensive guard in `handle_chat_mouse`: even with cursor
        // inside the message area, a wheel event must be a no-op when
        // the user has toggled capture off (tmux mode).
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.visible = true;
        app.chat.phase = ChatPhase::Chatting;
        app.chat.thread = Some("jyc".to_string());
        app.chat.focus = ChatFocus::ChatPane;
        app.chat.scroll = 0;
        app.chat.messages.push(ChatMessage {
            sender: "user".into(),
            text: "hi".into(),
            timestamp: Some("2026-01-01T00:00:00Z".into()),
        });

        // Opt out of capture (simulating the `toggle mouse` leader-key
        // action reaching `apply_mouse_capture`, which we don't exercise
        // here because it writes to real stdout).
        app.mouse_capture_enabled = false;

        terminal
            .draw(|f| ui_chat_mode(f, f.area(), &mut app))
            .unwrap();
        let rect = app
            .chat
            .last_message_area
            .expect("render should cache the message rect");
        let inside = mouse_event(MouseEventKind::ScrollUp, rect.x + 1, rect.y);

        handle_chat_mouse(&mut app, inside);
        assert_eq!(
            app.chat.scroll, 0,
            "wheel must be ignored when mouse capture is off"
        );
    }

    #[test]
    fn apply_mouse_capture_writes_enable_escape_when_on() {
        // Default state is capture on; `apply_mouse_capture_to` must
        // emit the EnableMouseCapture sequence. crossterm sets DECSET
        // modes 1000, 1002, 1003, 1015, and 1006 in one call.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let app = App::new(rx, None);
        assert!(app.mouse_capture_enabled);
        let mut buf = Vec::new();
        app.apply_mouse_capture_to(&mut buf).unwrap();
        assert_eq!(
            buf, *b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h",
            "capture-on must emit EnableMouseCapture"
        );
    }

    #[test]
    fn apply_mouse_capture_writes_disable_escape_when_off() {
        // After toggling off, `apply_mouse_capture_to` must emit the
        // DisableMouseCapture sequence (the same DECSET modes cleared
        // in reverse order).
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        assert!(!app.flip_mouse_capture());
        assert!(!app.mouse_capture_enabled);
        let mut buf = Vec::new();
        app.apply_mouse_capture_to(&mut buf).unwrap();
        assert_eq!(
            buf, *b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l",
            "capture-off must emit DisableMouseCapture"
        );
    }

    #[test]
    fn mouse_scroll_over_message_area_moves_focus_from_other_panes() {
        // Regression: when focus is on ActivityPane or ExplorerPane and the
        // user wheels over the message area, the wheel must advance the
        // message scroll counter and switch focus to MessageArea — not
        // silently scroll the activity pane or be a no-op.
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.visible = true;
        app.chat.phase = ChatPhase::Chatting;
        app.chat.thread = Some("jyc".to_string());
        app.chat.messages.push(ChatMessage {
            sender: "user".into(),
            text: "hi".into(),
            timestamp: Some("2026-01-01T00:00:00Z".into()),
        });
        app.chat.messages.push(ChatMessage {
            sender: "ai".into(),
            text: "hello".into(),
            timestamp: Some("2026-01-01T00:00:01Z".into()),
        });

        // --- ActivityPane focus ---
        app.chat.focus = ChatFocus::ActivityPane;
        app.chat.scroll = 0;
        app.chat.activity_scroll = 0;
        terminal
            .draw(|f| ui_chat_mode(f, f.area(), &mut app))
            .unwrap();
        let rect = app.chat.last_message_area.expect("rect cached");
        handle_chat_mouse(
            &mut app,
            mouse_event(MouseEventKind::ScrollUp, rect.x + 1, rect.y),
        );
        assert_eq!(
            app.chat.focus,
            ChatFocus::MessageArea,
            "focus moves to MessageArea"
        );
        assert_eq!(app.chat.scroll, 1, "message scroll advances");
        assert_eq!(app.chat.activity_scroll, 0, "activity pane must not scroll");

        // --- ExplorerPane focus ---
        let (_tx2, rx2) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app2 = App::new(rx2, None);
        app2.chat.visible = true;
        app2.chat.phase = ChatPhase::Chatting;
        app2.chat.thread = Some("jyc".to_string());
        app2.chat.messages.push(ChatMessage {
            sender: "user".into(),
            text: "hi".into(),
            timestamp: Some("2026-01-01T00:00:00Z".into()),
        });
        app2.chat.messages.push(ChatMessage {
            sender: "ai".into(),
            text: "hello".into(),
            timestamp: Some("2026-01-01T00:00:01Z".into()),
        });
        app2.chat.focus = ChatFocus::ExplorerPane;
        app2.chat.scroll = 0;
        terminal
            .draw(|f| ui_chat_mode(f, f.area(), &mut app2))
            .unwrap();
        let rect2 = app2.chat.last_message_area.expect("rect cached");
        handle_chat_mouse(
            &mut app2,
            mouse_event(MouseEventKind::ScrollUp, rect2.x + 1, rect2.y),
        );
        assert_eq!(
            app2.chat.focus,
            ChatFocus::MessageArea,
            "focus moves to MessageArea"
        );
        assert_eq!(app2.chat.scroll, 1, "message scroll advances");
    }

    #[test]
    fn esc_does_not_close_chat_in_pattern_select() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.visible = true;
        app.chat.phase = ChatPhase::PatternSelect;

        handle_chat_keys(&mut app, esc_key(), &mut test_terminal());
        assert!(app.chat.visible, "Esc must not close pattern select");
        assert_eq!(app.chat.phase, ChatPhase::PatternSelect);
    }

    #[test]
    fn leader_open_dashboard_closes_chat() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.visible = true;
        app.chat.phase = ChatPhase::Chatting;
        app.chat.thread = Some("jyc".to_string());

        execute_local_action(
            &mut app,
            &mut test_terminal(),
            local_commands::LocalAction::OpenDashboard,
        );
        assert!(!app.chat.visible);
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
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        app.chat.ws_tx = Some(cmd_tx);

        assert!(app.chat.visible);
        assert_eq!(app.chat.phase, ChatPhase::Chatting);
        assert_eq!(app.chat.thread.as_deref(), Some("jyc"));

        // close() is what Esc invokes — must return to overview
        app.chat.close();
        assert!(!app.chat.visible);
        assert_eq!(app.chat.phase, ChatPhase::PatternSelect);
        assert!(app.chat.ws_tx.is_none());
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
    fn zen_mode_hides_explorer_and_does_not_restore_it() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        // Explorer open alongside visible info pane.
        app.chat.toggle_zen_mode(); // exit default zen: info visible
        app.chat.toggle_explorer();
        assert!(app.chat.explorer_visible);
        assert!(app.chat.info_visible);

        // Enter zen → explorer hidden.
        app.chat.toggle_zen_mode();
        assert!(!app.chat.explorer_visible);
        assert!(!app.chat.info_visible);

        // Exit zen → info restored, explorer stays hidden.
        app.chat.toggle_zen_mode();
        assert!(app.chat.info_visible);
        assert!(!app.chat.explorer_visible);
    }

    #[test]
    fn explorer_move_clamps_and_saturates() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.state = Some(jyc_types::InspectOverview {
            threads: (0..5)
                .map(|i| jyc_types::ThreadSummary {
                    name: format!("t{i}"),
                    channel: "test".to_string(),
                    pattern: None,
                    status: jyc_types::ThreadStatus::Idle,
                    model: None,
                    mode: None,
                    branch: None,
                    changed_files: None,
                    context_input_tokens: None,
                    total_input_tokens: None,
                    total_cache_hit_tokens: None,
                    total_cache_creation_tokens: None,
                    max_tokens: None,
                    output_tokens: None,
                    last_active_at: None,
                    skills: vec![],
                    thread_path: None,
                    cost: None,
                })
                .collect(),
            ..Default::default()
        });

        explorer_move(&mut app, 1);
        assert_eq!(app.chat.explorer_selected, 1);
        // G-jump: saturates to the last row without overflow.
        explorer_move(&mut app, i64::MAX);
        assert_eq!(app.chat.explorer_selected, 4);
        // gg-jump: saturates to the first row.
        explorer_move(&mut app, i64::MIN);
        assert_eq!(app.chat.explorer_selected, 0);
    }

    #[test]
    fn opening_explorer_snaps_selection_to_chat_thread() {
        // Regression: the explorer opened on a stale row because
        // sync_explorer_selection only follows the chat thread while
        // the explorer is unfocused — and opening focuses it.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.state = Some(jyc_types::InspectOverview {
            threads: (0..5)
                .map(|i| jyc_types::ThreadSummary {
                    name: format!("t{i}"),
                    channel: "test".to_string(),
                    pattern: None,
                    status: jyc_types::ThreadStatus::Idle,
                    model: None,
                    mode: None,
                    branch: None,
                    changed_files: None,
                    context_input_tokens: None,
                    total_input_tokens: None,
                    total_cache_hit_tokens: None,
                    total_cache_creation_tokens: None,
                    max_tokens: None,
                    output_tokens: None,
                    last_active_at: None,
                    skills: vec![],
                    thread_path: None,
                    cost: None,
                })
                .collect(),
            ..Default::default()
        });
        app.chat.thread = Some("t2".to_string());
        app.chat.channel = Some("test".to_string());
        app.chat.explorer_selected = 0; // stale row

        toggle_explorer_snapped(&mut app);
        assert!(app.chat.explorer_visible);
        assert_eq!(app.chat.explorer_selected, 2);

        // Closing keeps the selection where it is.
        toggle_explorer_snapped(&mut app);
        assert!(!app.chat.explorer_visible);
        assert_eq!(app.chat.explorer_selected, 2);
    }

    #[test]
    fn opening_explorer_keeps_selection_when_chat_thread_not_in_list() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.state = Some(jyc_types::InspectOverview {
            threads: vec![jyc_types::ThreadSummary {
                name: "t0".to_string(),
                channel: "test".to_string(),
                pattern: None,
                status: jyc_types::ThreadStatus::Idle,
                model: None,
                mode: None,
                branch: None,
                changed_files: None,
                context_input_tokens: None,
                total_input_tokens: None,
                total_cache_hit_tokens: None,
                total_cache_creation_tokens: None,
                max_tokens: None,
                output_tokens: None,
                last_active_at: None,
                skills: vec![],
                thread_path: None,
                cost: None,
            }],
            ..Default::default()
        });
        // Chat is bound to a thread absent from the overview (e.g. a
        // fresh adhoc thread not yet polled).
        app.chat.thread = Some("missing".to_string());
        app.chat.channel = Some("test".to_string());
        app.chat.explorer_selected = 0;

        toggle_explorer_snapped(&mut app);
        assert!(app.chat.explorer_visible);
        assert_eq!(app.chat.explorer_selected, 0);
    }

    #[test]
    fn hiding_explorer_returns_focus_to_chat_pane() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.toggle_explorer();
        app.chat.focus = ChatFocus::ExplorerPane;
        app.chat.toggle_explorer();
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);
    }

    #[test]
    fn focus_cycle_includes_explorer_only_when_visible() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);

        // Hidden: ChatPane → MessageArea → ChatPane (no activity pane).
        app.chat.toggle_focus();
        assert_eq!(app.chat.focus, ChatFocus::MessageArea);
        app.chat.toggle_focus();
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);

        // Opening the explorer jumps focus straight into it so j/k/Enter
        // are immediately usable; Tab then returns to the chat input.
        app.chat.toggle_explorer();
        assert_eq!(app.chat.focus, ChatFocus::ExplorerPane);
        app.chat.toggle_focus();
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);
    }

    #[test]
    fn opening_explorer_moves_focus_into_it() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);
        app.chat.toggle_explorer();
        assert!(app.chat.explorer_visible);
        assert_eq!(app.chat.focus, ChatFocus::ExplorerPane);
    }

    #[tokio::test]
    async fn explorer_switch_sets_pending_hydrate_and_hides_explorer() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.open_addr = Some("test-addr".to_string());
        app.chat.token = None;
        app.state = Some(jyc_types::InspectOverview {
            channels: vec![jyc_types::ChannelInfo {
                name: "local_dev".to_string(),
                channel_type: "websocket".to_string(),
                active_workers: 0,
                max_concurrent: 0,
            }],
            threads: vec![
                jyc_types::ThreadSummary {
                    name: "current".to_string(),
                    channel: "local_dev".to_string(),
                    pattern: None,
                    status: jyc_types::ThreadStatus::Idle,
                    model: None,
                    mode: None,
                    branch: None,
                    changed_files: None,
                    context_input_tokens: None,
                    total_input_tokens: None,
                    total_cache_hit_tokens: None,
                    total_cache_creation_tokens: None,
                    max_tokens: None,
                    output_tokens: None,
                    last_active_at: None,
                    skills: vec![],
                    thread_path: None,
                    cost: None,
                },
                jyc_types::ThreadSummary {
                    name: "other".to_string(),
                    channel: "local_dev".to_string(),
                    pattern: None,
                    status: jyc_types::ThreadStatus::Idle,
                    model: None,
                    mode: None,
                    branch: None,
                    changed_files: None,
                    context_input_tokens: None,
                    total_input_tokens: None,
                    total_cache_hit_tokens: None,
                    total_cache_creation_tokens: None,
                    max_tokens: None,
                    output_tokens: None,
                    last_active_at: None,
                    skills: vec![],
                    thread_path: None,
                    cost: None,
                },
            ],
            ..Default::default()
        });
        app.chat.explorer_visible = true;
        app.chat.explorer_selected = 1;

        explorer_open_selected(&mut app);

        assert!(!app.chat.explorer_visible);
        assert_eq!(app.chat.thread.as_deref(), Some("other"));
        assert_eq!(app.chat.focus, ChatFocus::ChatPane);
        assert_eq!(
            app.pending_hydrate.as_ref(),
            Some(&("local_dev".to_string(), "other".to_string()))
        );
    }

    #[test]
    fn toggle_activity_shows_and_hides() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        // 0 (hidden) → 1 (bottom 20%) on first toggle; 1 → 0 on second.
        assert_eq!(app.chat.activity_split, 0);
        app.chat.toggle_activity();
        assert_eq!(app.chat.activity_split, 1);
        app.chat.toggle_activity();
        assert_eq!(app.chat.activity_split, 0);
        // Re-show after re-hide still lands on the bottom 20% size.
        app.chat.toggle_activity();
        assert_eq!(app.chat.activity_split, 1);
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

        // User opens activity via the leader popup (`Ctrl+P` then `a`).
        // Now both info and activity are visible.
        app.chat.toggle_activity();
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
    fn toggle_resets_after_zen_mode() {
        // Regression: after Ctrl+Z hides the activity pane, the next toggle
        // must show the bottom 20% size, not whatever (no-longer-meaningful)
        // intermediate state.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        // Open the activity pane, then hide it again.
        app.chat.toggle_activity();
        assert_eq!(app.chat.activity_split, 1);
        app.chat.toggle_activity();
        assert_eq!(app.chat.activity_split, 0);
        // Enter zen mode — activity is reset to 0 (already there).
        app.chat.toggle_zen_mode();
        assert_eq!(app.chat.activity_split, 0);
        // First toggle after zen mode must reach the 20% size.
        app.chat.toggle_activity();
        assert_eq!(app.chat.activity_split, 1);
    }

    #[test]
    fn explorer_selected_row_fills_full_width() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // Short thread name so the missing highlight (the bug) would leave
        // most of the row uncolored. The selection background must extend
        // to the pane's right edge, not just under the thread-name text.
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.explorer_visible = true;
        app.chat.focus = ChatFocus::ExplorerPane;
        app.state = Some(jyc_types::InspectOverview {
            threads: vec![jyc_types::ThreadSummary {
                name: "x".to_string(),
                channel: "test".to_string(),
                pattern: None,
                status: jyc_types::ThreadStatus::Idle,
                model: None,
                mode: None,
                branch: None,
                changed_files: None,
                context_input_tokens: None,
                total_input_tokens: None,
                total_cache_hit_tokens: None,
                total_cache_creation_tokens: None,
                max_tokens: None,
                output_tokens: None,
                last_active_at: None,
                skills: vec![],
                thread_path: None,
                cost: None,
            }],
            ..Default::default()
        });
        app.chat.explorer_selected = 0;

        let width = 20;
        let height = 5;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_explorer(frame, frame.area(), &app))
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        // Selected row sits at the top of the inner area (y=1 once the
        // title/border row is taken into account). Every cell across it
        // must have the cyan selection background.
        for x in 1..(width - 1) {
            let cell = &buffer[(x, 1)];
            assert_eq!(
                cell.bg,
                Color::Cyan,
                "explorer selection bg should fill row at x={x}, got {:?}",
                cell.bg
            );
        }

        // Title row (y=0) must start with the `──` prefix followed by the
        // title text. This guards against regressions in the title format.
        let title_row: String = (0..width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            title_row.starts_with("── Threads"),
            "explorer title row should start with `── Threads`, got: {title_row:?}"
        );
    }

    /// Regression: the activity pane title row must start with the `──`
    /// prefix so the heading and the top border form a continuous stripe.
    #[test]
    fn activity_pane_title_has_double_dash_prefix() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.visible = true;
        app.chat.phase = ChatPhase::Chatting;
        app.chat.thread = Some("jyc".to_string());
        app.chat.channel = Some("local_dev".to_string());
        app.chat.focus = ChatFocus::ActivityPane;

        let width = 40;
        let height = 5;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_activity_log(frame, frame.area(), &mut app))
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        let title_row: String = (0..width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            title_row.starts_with("── Activity"),
            "activity title row should start with `── Activity`, got: {title_row:?}"
        );
    }

    /// Regression: the thread info pane title row must start with the `──`
    /// prefix and the inner content area must start at y=1 (the top border
    /// row acts as a separator).
    #[test]
    fn thread_info_pane_title_has_double_dash_prefix() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.visible = true;
        app.chat.phase = ChatPhase::Chatting;
        app.chat.thread = Some("jyc".to_string());
        app.chat.channel = Some("local_dev".to_string());
        app.chat.info_visible = true;
        app.state = Some(jyc_types::InspectOverview {
            threads: vec![jyc_types::ThreadSummary {
                name: "jyc".to_string(),
                channel: "local_dev".to_string(),
                pattern: Some("jyc".to_string()),
                status: jyc_types::ThreadStatus::Idle,
                model: None,
                mode: None,
                branch: None,
                changed_files: None,
                context_input_tokens: None,
                total_input_tokens: None,
                total_cache_hit_tokens: None,
                total_cache_creation_tokens: None,
                max_tokens: None,
                output_tokens: None,
                last_active_at: None,
                skills: vec![],
                thread_path: None,
                cost: None,
            }],
            ..Default::default()
        });
        app.table_state.select(Some(0));

        let width = 20;
        let height = 8;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_thread_info_pane(frame, frame.area(), &mut app))
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        let title_row: String = (0..width)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            title_row.contains("── Thread Info"),
            "thread info title row should contain `── Thread Info`, got: {title_row:?}"
        );
    }

    /// Regression: the Files section must color `uncommitted: true`
    /// entries yellow and leave `uncommitted: false` entries plain,
    /// and must prefix each row with the kind glyph (`+` Added,
    /// `-` Deleted, two-space Modified). Driven by ratatui's
    /// `TestBackend` so we read styles off the rendered buffer
    /// rather than asserting on internal state.
    #[test]
    fn files_section_colors_uncommitted_paths_yellow() {
        use jyc_types::ChangeKind;
        use jyc_types::ChangedFileEntry;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.visible = true;
        app.chat.phase = ChatPhase::Chatting;
        app.chat.thread = Some("jyc".to_string());
        app.chat.channel = Some("local_dev".to_string());
        app.chat.info_visible = true;
        app.state = Some(jyc_types::InspectOverview {
            threads: vec![jyc_types::ThreadSummary {
                name: "jyc".to_string(),
                channel: "local_dev".to_string(),
                pattern: Some("jyc".to_string()),
                status: jyc_types::ThreadStatus::Idle,
                model: None,
                mode: None,
                branch: None,
                // One of each kind — sorted alphabetically on render.
                // added.rs       Added      clean
                // deleted.rs     Deleted    clean
                // dirty_add.rs   Added      dirty (yellow)
                // dirty_mod.rs   Modified   dirty (yellow)
                // modified.rs    Modified   clean
                changed_files: Some(vec![
                    ChangedFileEntry {
                        path: "modified.rs".into(),
                        uncommitted: false,
                        change: ChangeKind::Modified,
                    },
                    ChangedFileEntry {
                        path: "added.rs".into(),
                        uncommitted: false,
                        change: ChangeKind::Added,
                    },
                    ChangedFileEntry {
                        path: "deleted.rs".into(),
                        uncommitted: false,
                        change: ChangeKind::Deleted,
                    },
                    ChangedFileEntry {
                        path: "dirty_add.rs".into(),
                        uncommitted: true,
                        change: ChangeKind::Added,
                    },
                    ChangedFileEntry {
                        path: "dirty_mod.rs".into(),
                        uncommitted: true,
                        change: ChangeKind::Modified,
                    },
                ]),
                context_input_tokens: None,
                total_input_tokens: None,
                total_cache_hit_tokens: None,
                total_cache_creation_tokens: None,
                max_tokens: None,
                output_tokens: None,
                last_active_at: None,
                skills: vec![],
                thread_path: None,
                cost: None,
            }],
            ..Default::default()
        });
        app.table_state.select(Some(0));

        // Tall enough pane that nothing scrolls — both rows must appear.
        let width = 30;
        let height = 24;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_thread_info_pane(frame, frame.area(), &mut app))
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();

        // Find the row that contains `needle` and return the cell at column
        // `column_offset` of that row (used to inspect the prefix glyph
        // at column 0). Returns (symbol, fg).
        let find = |needle: &str, column_offset: usize| -> Option<(char, Color)> {
            for y in 0..buffer.area.height {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect();
                if row.contains(needle) {
                    let col = column_offset as u16;
                    return buffer
                        .cell((col, y))
                        .map(|c| (c.symbol().chars().next().unwrap_or(' '), c.fg));
                }
            }
            None
        };

        // The block uses Borders::TOP | Borders::LEFT, so column 0 is the
        // left border and column 1 is the start of the content. The
        // prefix glyph is at column 1; the path begins at column 3
        // (after the glyph and its trailing space).
        let content_col = 1usize;

        // Color assertions.
        assert_eq!(
            find("modified.rs", content_col)
                .expect("modified.rs must render")
                .1,
            Color::Reset,
            "clean Modified must use the default foreground"
        );
        assert_eq!(
            find("added.rs", content_col)
                .expect("added.rs must render")
                .1,
            Color::Reset,
            "clean Added must use the default foreground"
        );
        assert_eq!(
            find("deleted.rs", content_col)
                .expect("deleted.rs must render")
                .1,
            Color::Reset,
            "clean Deleted must use the default foreground"
        );
        assert_eq!(
            find("dirty_mod.rs", content_col)
                .expect("dirty_mod.rs must render")
                .1,
            Color::Yellow,
            "uncommitted Modified must be rendered in yellow"
        );
        assert_eq!(
            find("dirty_add.rs", content_col)
                .expect("dirty_add.rs must render")
                .1,
            Color::Yellow,
            "uncommitted Added must be rendered in yellow"
        );

        // Prefix-glyph assertions.
        assert_eq!(
            find("added.rs", content_col).expect("added.rs prefix").0,
            '+',
            "Added rows must start with '+'"
        );
        assert_eq!(
            find("deleted.rs", content_col)
                .expect("deleted.rs prefix")
                .0,
            '-',
            "Deleted rows must start with '-'"
        );
        assert_eq!(
            find("modified.rs", content_col)
                .expect("modified.rs prefix")
                .0,
            ' ',
            "Modified rows must start with a space (2-space prefix for alignment)"
        );
        assert_eq!(
            find("dirty_mod.rs", content_col)
                .expect("dirty_mod.rs prefix")
                .0,
            ' ',
            "uncommitted Modified still uses 2-space prefix"
        );
        assert_eq!(
            find("dirty_add.rs", content_col)
                .expect("dirty_add.rs prefix")
                .0,
            '+',
            "uncommitted Added still uses '+' prefix"
        );
    }

    /// Regression: when `info_scroll` is set past the pane height, the
    /// post-render clamp must bring it back to `inner.height - 1`.
    /// Otherwise the next render scrolls past the end and shows an
    /// empty pane.
    #[test]
    fn info_scroll_is_clamped_after_render() {
        use jyc_types::ChangedFileEntry;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx, None);
        app.chat.visible = true;
        app.chat.phase = ChatPhase::Chatting;
        app.chat.thread = Some("jyc".to_string());
        app.chat.channel = Some("local_dev".to_string());
        app.chat.info_visible = true;
        app.state = Some(jyc_types::InspectOverview {
            threads: vec![jyc_types::ThreadSummary {
                name: "jyc".to_string(),
                channel: "local_dev".to_string(),
                pattern: Some("jyc".to_string()),
                status: jyc_types::ThreadStatus::Idle,
                model: None,
                mode: None,
                branch: None,
                // Three files — fits easily in a tall pane.
                changed_files: Some(vec![
                    ChangedFileEntry {
                        path: "a.rs".into(),
                        uncommitted: false,
                        change: jyc_types::ChangeKind::Modified,
                    },
                    ChangedFileEntry {
                        path: "b.rs".into(),
                        uncommitted: false,
                        change: jyc_types::ChangeKind::Modified,
                    },
                    ChangedFileEntry {
                        path: "c.rs".into(),
                        uncommitted: false,
                        change: jyc_types::ChangeKind::Modified,
                    },
                ]),
                context_input_tokens: None,
                total_input_tokens: None,
                total_cache_hit_tokens: None,
                total_cache_creation_tokens: None,
                max_tokens: None,
                output_tokens: None,
                last_active_at: None,
                skills: vec![],
                thread_path: None,
                cost: None,
            }],
            ..Default::default()
        });
        app.table_state.select(Some(0));
        // Pretend we previously scrolled way past the end.
        app.chat.info_scroll = usize::MAX;

        let width = 20;
        let height = 10;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_thread_info_pane(frame, frame.area(), &mut app))
            .expect("draw");

        // The pane is 10 rows tall, 1 row consumed by the border, so the
        // coarse clamp upper bound is `10 - 1 = 9`.
        assert!(
            app.chat.info_scroll < height as usize,
            "info_scroll must be clamped to inner.height - 1, got {}",
            app.chat.info_scroll
        );
    }

    fn ctx_with_full_data() -> ChatHeaderCtx<'static> {
        ChatHeaderCtx {
            mode: "plan",
            channel: Some("local_dev"),
            pattern: Some("jyc"),
            branch: None,
            model: Some("claude-opus-4-6"),
            pct: Some(10),
        }
    }

    fn test_header_style() -> Style {
        Style::default()
            .fg(Color::Rgb(249, 226, 175))
            .add_modifier(Modifier::BOLD)
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn header_line_box_drawing_uses_passed_line_style() {
        let ctx = ctx_with_full_data();
        // Inactive: line-drawing chars use #393552.
        let inactive = build_chat_header_line(80, &ctx, test_header_style(), LINE_DRAWING);
        assert_eq!(inactive.spans[0].content.as_ref(), "╭─");
        assert_eq!(
            inactive.spans[0].style.fg,
            Some(Color::Rgb(0x39, 0x35, 0x52))
        );
        // Active: caller passes DarkGray (matches the message separator).
        let active = build_chat_header_line(
            80,
            &ctx,
            test_header_style(),
            Style::default().fg(Color::DarkGray),
        );
        assert_eq!(active.spans[0].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn header_line_box_drawing_uses_line_color() {
        let ctx = ctx_with_full_data();
        let line = build_chat_header_line(80, &ctx, test_header_style(), LINE_DRAWING);
        let line_fg = Color::Rgb(0x39, 0x35, 0x52);
        // First span is the "╭─" prefix in the line-drawing color.
        assert_eq!(line.spans[0].content.as_ref(), "╭─");
        assert_eq!(line.spans[0].style.fg, Some(line_fg));
        // The dash padding run also uses the line-drawing color.
        let dash_span = line
            .spans
            .iter()
            .find(|s| s.content.chars().all(|c| c == '─'))
            .expect("dash padding span");
        assert_eq!(dash_span.style.fg, Some(line_fg));
    }

    #[test]
    fn header_line_includes_mode_channel_pattern_and_chip() {
        let ctx = ctx_with_full_data();
        let line = build_chat_header_line(80, &ctx, test_header_style(), LINE_DRAWING);
        let text = line_text(&line);
        // Left segment includes mode + channel + pattern.
        assert!(
            text.contains("╭─ plan · local_dev · jyc"),
            "missing left segment in: {text:?}"
        );
        // Right chip includes model + context-window percentage. The
        // version lives in the status bar, not the chat header.
        assert!(
            text.contains("[ claude-opus-4-6 · 10% ]"),
            "missing model/pct chip in: {text:?}"
        );
        assert!(
            !text.contains("jyc ai v"),
            "version belongs in the status bar, not the chat header: {text:?}"
        );
        // The line should fill the requested width via dash padding.
        assert_eq!(text.width(), 80);
    }

    #[test]
    fn header_line_omits_pattern_when_missing() {
        let mut ctx = ctx_with_full_data();
        ctx.pattern = None;
        let line = build_chat_header_line(80, &ctx, test_header_style(), LINE_DRAWING);
        let text = line_text(&line);
        assert!(
            text.starts_with("╭─ plan · local_dev"),
            "missing channel segment in: {text:?}"
        );
        assert!(!text.contains("· jyc"));
    }

    #[test]
    fn header_line_with_no_state_is_just_mode_and_padding() {
        let ctx = ChatHeaderCtx {
            mode: "build",
            channel: None,
            pattern: None,
            branch: None,
            model: None,
            pct: None,
        };
        let line = build_chat_header_line(80, &ctx, test_header_style(), LINE_DRAWING);
        let text = line_text(&line);
        // Defaults: mode = "build", channel/pattern/branch all absent.
        assert!(
            text.starts_with("╭─ build"),
            "missing default mode in: {text:?}"
        );
        // No fallback placeholders either — there is no chip anymore.
        assert!(
            !text.contains('[') && !text.contains('?'),
            "no question-mark placeholders expected: {text:?}"
        );
        // Padding still fills the row.
        assert_eq!(text.width(), 80);
    }

    #[test]
    fn header_line_truncates_left_when_too_narrow() {
        let mut ctx = ctx_with_full_data();
        ctx.channel = Some("a-very-long-channel-name");
        ctx.pattern = Some("a-very-long-pattern-name");
        // Width so tight that even truncating channel to 3 chars barely fits.
        let line = build_chat_header_line(20, &ctx, test_header_style(), LINE_DRAWING);
        let text = line_text(&line);
        // Channel must be truncated to fit; no chip ever rendered.
        assert!(
            !text.contains('['),
            "should not contain a chip, got: {text:?}"
        );
        assert!(text.starts_with("╭─ plan"));
        assert!(text.width() <= 20);
        // Never leave a dangling separator at the end.
        assert!(
            !text.ends_with("· "),
            "should not end with separator: {text:?}"
        );
    }

    #[test]
    fn header_line_appends_branch_when_present() {
        let mut ctx = ctx_with_full_data();
        ctx.branch = Some("feat/issue-512-show-branch");
        let line = build_chat_header_line(120, &ctx, test_header_style(), LINE_DRAWING);
        let text = line_text(&line);
        assert!(
            text.contains("· jyc · feat/issue-512-show-branch"),
            "branch segment should be appended after pattern, got: {text:?}"
        );
    }

    #[test]
    fn header_line_omits_branch_segment_when_none() {
        // Same ctx as `header_line_includes_mode_channel_pattern`
        // but with branch=None — the left segment must end at "· jyc"
        // without a dangling separator.
        let ctx = ctx_with_full_data();
        let line = build_chat_header_line(120, &ctx, test_header_style(), LINE_DRAWING);
        let text = line_text(&line);
        assert!(
            text.contains("· jyc "),
            "pattern should still render, got: {text:?}"
        );
        assert!(
            !text.contains("· · "),
            "no double-separator when branch absent, got: {text:?}"
        );
    }

    #[test]
    fn header_line_renders_partial_chip_with_model_only() {
        // pct missing (e.g., session hasn't recorded context yet) — the
        // chip should still render with just the model name.
        let mut ctx = ctx_with_full_data();
        ctx.pct = None;
        let line = build_chat_header_line(80, &ctx, test_header_style(), LINE_DRAWING);
        let text = line_text(&line);
        assert!(
            text.contains("[ claude-opus-4-6 ]"),
            "partial chip with model only: {text:?}"
        );
        assert!(
            !text.contains('%'),
            "no pct placeholder when pct is None: {text:?}"
        );
    }

    #[test]
    fn header_line_drops_chip_when_narrow() {
        // Width that fits the left segment but not the chip — chip
        // should be dropped, left segment preserved (with dash padding).
        let ctx = ctx_with_full_data();
        // Left "╭─ plan · local_dev · jyc" = 26 display cols.
        // Chip "[ claude-opus-4-6 · 10% ]" = 23 display cols.
        // total = 49 cols + 2 padding spaces. Width 48 forces dropping
        // the chip and falls back to dash padding only.
        let line = build_chat_header_line(48, &ctx, test_header_style(), LINE_DRAWING);
        let text = line_text(&line);
        assert!(
            !text.contains('['),
            "chip should be dropped when narrow, got: {text:?}"
        );
        assert!(
            text.contains("╭─ plan · local_dev · jyc"),
            "left segment should still render: {text:?}"
        );
        assert!(text.width() <= 48);
    }

    #[test]
    fn header_line_never_emits_dangling_separator() {
        // Width fits "╭─ plan · " (10 cols) but no room for channel content.
        let ctx = ChatHeaderCtx {
            mode: "plan",
            channel: Some("ch"),
            pattern: None,
            branch: None,
            model: None,
            pct: None,
        };
        let line = build_chat_header_line(10, &ctx, test_header_style(), LINE_DRAWING);
        let text = line_text(&line);
        assert!(
            !text.ends_with("· "),
            "should not end with separator: {text:?}"
        );
    }

    #[test]
    fn truncate_to_width_short_string_unchanged() {
        assert_eq!(truncate_to_width("hi", 5), "hi");
        assert_eq!(truncate_to_width("hi", 2), "hi");
    }

    #[test]
    fn truncate_to_width_long_string_gets_ellipsis() {
        assert_eq!(truncate_to_width("hello world", 6), "hello…");
        assert_eq!(truncate_to_width("abc", 1), "…");
        assert_eq!(truncate_to_width("abc", 0), "");
    }

    #[test]
    fn truncate_to_width_counts_cjk_as_two_columns() {
        // 4 CJK chars = 8 display columns; budget 5 keeps 2 chars + …
        let out = truncate_to_width("你好世界", 5);
        assert_eq!(out, "你好…");
        assert_eq!(out.width(), 5);
        // Wide char that doesn't fit the remaining column is dropped.
        let out = truncate_to_width("你好", 3);
        assert_eq!(out, "你…");
    }
}
