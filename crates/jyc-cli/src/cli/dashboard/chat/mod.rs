//! Chat pane: state, key handling, and rendering for the dashboard's
//! WebSocket thread chat (all channel types via `/ws/<channel>/<thread>`).

use super::token_render::{
    input_token_pct, push_cache_creation_span, push_cache_hit_span, push_cost_span,
    push_output_span, push_tokens_span, push_total_input_span,
};
use super::*;

mod render;

use render::{RenderFingerprint, render_chat_conversation, truncate_to_width};
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
    /// The chat input field.
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
    /// Multi-line text editor for the chat input (ratatui-textarea).
    pub(super) editor: TextArea<'static>,
    pub(super) focus: ChatFocus,
    pub(super) scroll: usize,
    pub(super) info_scroll: usize,
    pub(super) activity_scroll: usize,
    /// Last rendered rectangle of the scrollable message area (top chunk
    /// inside the chat pane). Stored during render and used by mouse-wheel
    /// hit-testing so scrolling only happens when the cursor is over the
    /// message area, not the editor / activity / explorer / info panes.
    pub(super) last_message_area: Option<Rect>,
    /// Last rendered maximum message-area scroll offset (`max_skip`).
    /// Stored during render and used to clamp `scroll_up` / `page_up` at
    /// the source — without this the offset overshoots the top and the
    /// overshoot must be scrolled back off before the view visibly moves.
    pub(super) last_max_scroll: usize,
    /// Rendered transcript lines cache — rebuilt only when the message
    /// history or pane width changes (see `history_fingerprint`). Avoids
    /// re-parsing the full transcript markdown on every frame (each
    /// keystroke / 50ms poll / 1Hz tick used to cost O(history)).
    pub(super) render_cache: Option<(RenderFingerprint, Vec<Line<'static>>)>,
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
    /// Aux-pane snapshot taken when entering zen mode (leader `z`),
    /// restored exactly on exit. `Some` = currently in zen mode.
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

/// Creates a chat input editor containing `text` (possibly multi-line),
/// cursor at the end. Line numbers and the cursor-line highlight are off;
/// long lines soft-wrap at word boundaries.
pub(super) fn chat_editor(text: &str) -> TextArea<'static> {
    let mut editor = TextArea::new(text.split('\n').map(str::to_string).collect());
    editor.remove_line_number();
    editor.set_cursor_line_style(Style::default());
    editor.set_wrap_mode(WrapMode::WordOrGlyph);
    editor.move_cursor(CursorMove::Bottom);
    editor.move_cursor(CursorMove::End);
    editor
}

/// Creates a fresh, empty chat input editor.
pub(super) fn empty_chat_editor() -> TextArea<'static> {
    chat_editor("")
}

impl ChatState {
    /// Replace the editor contents with `cmd`, cursor at end. Used by the
    /// command popup when delivering a selected command.
    pub(super) fn populate_editor(&mut self, cmd: &str) {
        self.editor = chat_editor(cmd);
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

/// Markdown render options for chat messages: Base16MochaDark code theme.
/// The highlighter emits foreground colors only — the terminal background
/// is kept, so the theme cannot clash with the TUI.
pub(super) fn chat_markdown_options() -> tui_markdown::Options {
    tui_markdown::Options::default().code_theme(tui_markdown::BuiltinCodeTheme::Base16MochaDark)
}

/// Rewrites markdown soft breaks (`\n` inside a paragraph) into hard breaks
/// (`"  \n"`) outside fenced code blocks, so line breaks the user typed into
/// the chat input survive rendering.
///
/// tui-markdown parses with hardcoded `ParseOptions` (no `ENABLE_HARDBREAKS`)
/// and renders `SoftBreak` as a space, collapsing multi-line messages into
/// one visual line. Fenced code blocks are left untouched — trailing spaces
/// inside them would alter the code.
// ponytail: local workaround; drop if tui-markdown ever exposes parser options.
pub(super) fn softbreaks_to_hardbreaks(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut in_fence = false;
    for line in md.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        match (!in_fence).then(|| line.strip_suffix('\n')).flatten() {
            Some(body) => {
                out.push_str(body);
                out.push_str("  \n");
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Word-wrap styled `lines` to `max_width` display columns, preserving span
/// styles and the line-level style (tui-markdown puts heading and
/// blockquote styling there, not on the spans), and return owned lines —
/// one entry per visual row.
///
/// The message area renders with `Paragraph` *without* `.wrap()`, so the
/// wrapping must happen here: scroll math counts `all_lines` entries and
/// must match the visual rows on screen. Breaks prefer the last space on
/// the row (the space itself is dropped); a word longer than `max_width`
/// is split at the column boundary. Wide characters (CJK, emoji) count for
/// the columns they occupy; zero-width characters attach to the current row
/// without advancing the width counter. `max_width` is clamped to at least
/// 1 to guarantee progress on extremely narrow panes.
pub(super) fn wrap_styled_lines(lines: Vec<Line<'_>>, max_width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthChar;

    /// Rebuild a `Line` from (char, style) cells, merging adjacent cells
    /// that share a style into one span.
    fn cells_to_line(cells: &[(char, Style)]) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for &(ch, style) in cells {
            match spans.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push(ch),
                _ => spans.push(Span::styled(ch.to_string(), style)),
            }
        }
        Line::from(spans)
    }

    /// Display width of a (char, style) row.
    fn row_width(row: &[(char, Style)]) -> usize {
        row.iter()
            .map(|&(ch, _)| UnicodeWidthChar::width(ch).unwrap_or(0))
            .sum()
    }

    let max_width = max_width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();

    for line in lines {
        // Line-level style (headings, blockquotes) applies to every row the
        // line wraps into.
        let line_style = line.style;
        // Flatten spans to (char, style) cells so wrapping can split spans.
        let cells: Vec<(char, Style)> = line
            .spans
            .iter()
            .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
            .collect();
        if cells.is_empty() {
            // Preserve blank lines from the source markdown.
            out.push(Line::default().style(line_style));
            continue;
        }

        let mut row: Vec<(char, Style)> = Vec::new();
        let mut width: usize = 0;
        // Index into `row` of the last space — the preferred break point.
        let mut last_space: Option<usize> = None;

        for cell @ (ch, _) in cells {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);

            // Overflow: break at the last space when it is not the row's
            // first cell (dropping the space), otherwise hard-split at the
            // boundary. Loops because the carried-over tail plus `cell` can
            // still overflow after a word-break; each iteration makes
            // progress (a word-break emits ≥1 cell, a hard-split empties
            // the row and exits). An over-wide char on an empty row always
            // lands — a character cannot be split.
            while width + ch_width > max_width && !row.is_empty() {
                if let Some(sp) = last_space.filter(|&sp| sp > 0) {
                    out.push(cells_to_line(&row[..sp]).style(line_style));
                    row.drain(..=sp);
                    width = row_width(&row);
                    last_space = row.iter().rposition(|&(c, _)| c == ' ');
                } else {
                    out.push(cells_to_line(&row).style(line_style));
                    row.clear();
                    width = 0;
                    last_space = None;
                }
            }

            if ch == ' ' {
                last_space = Some(row.len());
            }
            row.push(cell);
            width += ch_width;
        }

        out.push(cells_to_line(&row).style(line_style));
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
) -> Result<()>
where
    // ratatui 0.30 no longer bounds `Backend::Error` by Send + Sync, but
    // anyhow conversion requires them.
    B::Error: Send + Sync + 'static,
{
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
            app.chat.editor = chat_editor(edited);
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

/// Refocus the chat input, consuming the key — the user can scroll any
/// pane, then press any key to return to the input and start typing.
fn refocus_input(app: &mut App) {
    app.chat.focus = ChatFocus::ChatPane;
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
) where
    B::Error: Send + Sync + 'static,
{
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
        LocalAction::FocusChat => app.chat.focus = ChatFocus::MessageArea,
        LocalAction::ScrollTop => app.chat.scroll_to_top(),
        LocalAction::ScrollBottom => app.chat.scroll_to_bottom(),
        LocalAction::ToggleMouseCapture => super::toggle_mouse_capture(app),
        // Leader equivalent of typing `/` in an empty input, minus the
        // empty-input requirement (the leader is explicit intent).
        // Chatting-only: the popup is meaningless in PatternSelect.
        LocalAction::OpenCommandPopup => {
            if app.chat.phase == ChatPhase::Chatting {
                app.chat.focus = ChatFocus::ChatPane;
                app.chat.command_popup = Some(CommandPopupState::new());
            }
        }
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
) where
    B::Error: Send + Sync + 'static,
{
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

    // Ctrl+P is the leader (works in any chat phase — it is the only way
    // back to the dashboard from PatternSelect).
    let is_ctrl_p = key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_p {
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

    // "/" opens the command popup as the first char of an empty input
    // (intercepted before it reaches the editor).
    let is_slash = key.code == KeyCode::Char('/') && !key.modifiers.contains(KeyModifiers::CONTROL);
    if is_slash
        && app.chat.phase == ChatPhase::Chatting
        && app.chat.focus == ChatFocus::ChatPane
        && app.chat.text().trim().is_empty()
    {
        app.chat.leader = None;
        app.chat.command_popup = Some(CommandPopupState::new());
        return;
    }

    match app.chat.phase {
        ChatPhase::PatternSelect => match key.code {
            // No Esc-back here: returning to the dashboard is done via the
            // leader-key popup (`open dashboard`, Ctrl+P).
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

            // App-level keys take precedence over the editor.
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
            // Any other key refocuses the input (consumed, not forwarded),
            // so the user can browse then just start typing.
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
                    _ => refocus_input(app),
                }
                return;
            }

            if app.chat.focus == ChatFocus::InfoPane {
                // Vertical scroll only — file paths are short enough
                // that horizontal overflow isn't a concern. No Esc-back:
                // leaving the info pane is via Tab (focus cycle) or
                // the leader-key popup, same as ActivityPane. Any other
                // key refocuses the input (consumed, not forwarded), so
                // the user can scroll then just start typing.
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up | KeyCode::Char('k') => app.chat.scroll_up(),
                    KeyCode::Down | KeyCode::Char('j') => app.chat.scroll_down(),
                    KeyCode::Char('G') => app.chat.scroll_to_bottom(),
                    KeyCode::Char('g') if gg_jump => app.chat.scroll_to_top(),
                    KeyCode::Char('g') => {}
                    // PageUp/PageDown never reach here: the app-level
                    // match above intercepts them for every pane.
                    _ => refocus_input(app),
                }
                return;
            }

            if app.chat.focus == ChatFocus::ActivityPane {
                match key.code {
                    // No Esc-back here: returning to the dashboard is done
                    // via the leader-key popup (`open dashboard`, Ctrl+P).
                    // Any other key refocuses the input, consumed (same
                    // as MessageArea).
                    KeyCode::Esc => {}
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
                    _ => refocus_input(app),
                }
                return;
            }

            // Message area: scroll the conversation with arrows / vim keys.
            // Esc returns focus to the input field (does not exit the chat).
            // Any other key refocuses the input (consumed, not forwarded),
            // so the user can scroll then just start typing.
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
                    _ => refocus_input(app),
                }
                return;
            }

            // Chat input field. Everything not matched here is delegated
            // to the textarea (character input, editing keys, undo/redo).
            match key.code {
                // Esc does not leave the thread: returning to the dashboard
                // is done via the leader-key popup (`open dashboard`, Ctrl+P).
                // Plain Enter sends the message. Pasted multi-line text
                // goes through insert_str (not key events), so no paste
                // debounce is needed.
                KeyCode::Enter
                    if !key.modifiers.contains(KeyModifiers::SHIFT)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    app.chat.send_message()
                }
                // Shift/Alt+Enter inserts a newline.
                KeyCode::Enter => {
                    app.chat.editor.insert_newline();
                }
                // Up/Down, when input is empty or browsing history, recall history.
                KeyCode::Up
                    if app.chat.text().trim().is_empty() || app.chat.history_pos.is_some() =>
                {
                    app.chat.recall_older()
                }
                KeyCode::Down
                    if app.chat.text().trim().is_empty() || app.chat.history_pos.is_some() =>
                {
                    app.chat.recall_newer()
                }
                _ => {
                    app.chat.editor.input(key);
                }
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
            focus: ChatFocus::ChatPane,
            scroll: 0,
            info_scroll: 0,
            activity_scroll: 0,
            last_message_area: None,
            last_max_scroll: 0,
            render_cache: None,
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
        self.last_max_scroll = 0;
        self.render_cache = None;
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
        self.last_max_scroll = 0;
        self.render_cache = None;
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
                self.scroll = self.scroll.saturating_add(1).min(self.last_max_scroll)
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
                self.scroll = self.scroll.saturating_add(page).min(self.last_max_scroll)
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
        self.editor.lines().join("\n")
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
        self.editor = chat_editor(&self.input_history[new_pos]);
        self.history_pos = Some(new_pos);
    }

    /// Recall a newer entry from input history into the editor.
    /// At the newest, clears the editor and exits history mode.
    pub(super) fn recall_newer(&mut self) {
        match self.history_pos {
            Some(pos) if pos + 1 < self.input_history.len() => {
                let new_pos = pos + 1;
                self.editor = chat_editor(&self.input_history[new_pos]);
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
mod tests;
