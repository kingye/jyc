//! Chat pane: state, key handling, and rendering for the dashboard's
//! WebSocket topic chat (all channel types via `/ws/<channel>/<topic>`).

use super::token_render::{
    input_token_pct, push_cache_creation_span, push_cache_hit_span, push_cache_utilization_span,
    push_cost_span, push_output_span, push_tokens_span, push_total_input_span,
};
use super::*;
use jyc_core::duration::{DurationStyle, format_duration_ms, format_duration_secs};

pub(super) mod clipboard;
mod render;
mod table_wrap;

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
    /// User is actively chatting in a topic.
    Chatting,
}

/// An `ask_user` question pushed by the daemon, awaiting the user's answer.
pub(super) struct PendingQuestion {
    /// Question id — the response frame references it.
    pub id: String,
    /// Topic the question belongs to (only surfaced in that topic's pane).
    pub topic: String,
    /// Question text.
    pub question: String,
    /// Selectable options.
    pub options: Vec<String>,
    /// Whether more than one option may be picked (the daemon's
    /// `allow_multiple`).
    pub multi: bool,
    /// Currently highlighted option - the cursor `Space` marks under.
    pub selected: usize,
    /// Marked option indices, in multi mode; single mode answers with the
    /// cursor directly.
    pub marked: Vec<usize>,
}

/// Which pane has focus in chat mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChatFocus {
    /// The chat input field.
    ChatPane,
    /// The scrollable message area above the input field.
    MessageArea,
    /// The right-hand topic info pane (topic metadata + changed files).
    InfoPane,
    /// The activity log pane.
    ActivityPane,
    /// The left-side topic explorer pane.
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

/// Chat pane state: WebSocket topic chat for any channel type.
pub(super) struct ChatState {
    // Chat pane state
    pub(super) visible: bool,
    pub(super) phase: ChatPhase,
    pub(super) patterns: Vec<String>,
    pub(super) pattern_selected: usize,
    pub(super) topic: Option<String>,
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
    /// hit-testing so the wheel only acts when the cursor is over a pane it
    /// serves — the message area or the info pane — while the editor, the
    /// activity log and the explorer keep absorbing it.
    pub(super) last_message_area: Option<Rect>,
    /// Last rendered content rectangle of the topic info pane. Like
    /// [`Self::last_message_area`], lets the wheel scroll what it hovers.
    /// Only consulted while `info_visible`, and refreshed by every render that
    /// shows the pane, so no reset path needs to clear it.
    pub(super) last_info_area: Option<Rect>,
    /// Last rendered maximum message-area scroll offset (`max_skip`).
    /// Stored during render and used to clamp `scroll_up` / `page_up` at
    /// the source — without this the offset overshoots the top and the
    /// overshoot must be scrolled back off before the view visibly moves.
    pub(super) last_max_scroll: usize,
    /// Text a yank key (`yy`, `y3y`, or `y` on a selection) put on the clipboard,
    /// waiting for the event loop to send it as OSC 52 — queued because the
    /// handlers run inside `terminal.draw` and cannot write to stdout, see
    /// [`clipboard`].
    pub(super) pending_clipboard: Option<String>,
    /// A `y` was pressed and is waiting for the second half of `yy` / `y3y`.
    pub(super) pending_y: bool,
    /// Line of the rendered transcript the message-pane cursor sits on, absolute
    /// (independent of scrolling) so the view can move under it. `usize::MAX`
    /// until the transcript is measured — the renderer resolves it to the end of
    /// the newest message, and leaves it alone while there are no messages yet;
    /// it also keeps it inside the line count. Visible only while
    /// `focus == ChatFocus::MessageArea`.
    pub(super) cursor_line: usize,
    /// How many lines the transcript rendered last frame — the cursor's
    /// movement range. Written by the renderer, read by the keys.
    pub(super) last_total_lines: usize,
    /// Digit prefix for a movement or yank command: `3j`, `y3y`, `20k`.
    pub(super) pending_count: usize,
    /// The far end of a selection started with a Shift movement key, as an
    /// absolute transcript row (`None` = nothing selected). The cursor is the
    /// other end, so movement keeps growing (or shrinking) the range until `y`
    /// copies it — ending on the first row of the range, as linewise vim does —
    /// or `Esc` drops it and leaves the cursor where it moved to; scrolling
    /// leaves both ends alone, so a selection never changes just because the view
    /// moved.
    ///
    /// ponytail: both ends are absolute *row* indices, so a transcript that
    /// reflows underneath (the streaming reply turning into history) can leave
    /// the range on other text — clamped, never past the end. Anchoring to the
    /// text itself is the next step if that ever bites.
    pub(super) selection_anchor: Option<usize>,
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
    /// the topic is processing or has completed. Bridges the gap between
    /// sending a message and the inspect server reporting Processing status.
    pub(super) awaiting_response: bool,
    /// Activity pane visibility/size state.
    /// 0 = hidden, 1 = bottom 20%, 2 = bottom 80%, 3 = activity-only (full pane)
    pub(super) activity_split: u8,
    /// Topic info pane (right side, 20% width) visibility. Default
    /// visible; toggled via the leader-key popup (`i`).
    pub(super) info_visible: bool,
    /// Bottom status bar visibility. Default visible; toggled via the
    /// leader-key popup (`s`).
    pub(super) status_visible: bool,
    /// Topic explorer pane (left side, 20% width). Default hidden;
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
    /// `(channel, topic)`. The activity pane and chat progress read
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
    /// Live thinking blocks — appended by WS `thinking` events (never
    /// overwritten); cleared when a processing round starts, converted
    /// into a `sender: "thinking"` pseudo-message when it completes.
    pub(super) live_thinking: std::collections::BTreeMap<(String, String), Vec<String>>,
    /// Whether thinking display is expanded (live tail + completed-turn
    /// pseudo-messages). Toggled via the leader popup (`t`); default
    /// collapsed.
    pub(super) thinking_expanded: bool,
    /// Whether tool-call lines in the progress tail show the full,
    /// multi-line input detail instead of the one-line extracted
    /// summary. Toggled via the leader popup (`T`); default collapsed.
    pub(super) tool_detail_expanded: bool,
    /// Live processing status — updated by WS `processing` events.
    pub(super) live_processing: std::collections::BTreeMap<(String, String), (bool, bool)>,
    /// Live loop duration in milliseconds — updated by WS `loop_tick`
    /// events (1 Hz while a loop is running, with the first tick fired
    /// immediately at t=0). Drives the live-duration ticker in the
    /// dashboard's Details panel, the chat-mode info pane, and the chat
    /// progress line.
    pub(super) live_tick_ms: std::collections::BTreeMap<(String, String), u64>,
    /// Last-seen monotonic id per (channel, topic) — used to drop duplicate
    /// WS events after reconnect / `resync`.
    pub(super) last_seen_id: std::collections::BTreeMap<(String, String), u64>,
    /// Highest `ChatMessageEntry::id` already pushed from `live_chat` into
    /// `messages` for each (channel, topic). Used by the poll-driven sync
    /// to skip live entries that have already been pushed; without this,
    /// historical `id = 0` rows from `chat_log_store.rs` JSONL hydrate
    /// would re-push on every 500 ms poll cycle.
    pub(super) last_pushed_chat_id: std::collections::BTreeMap<(String, String), u64>,
    /// Last (channel, topic) that was REST-hydrated by the poll loop.
    /// Used to avoid re-hydrating the same topic on every poll when the
    /// user is browsing the overview.
    pub(super) last_hydrated_key: Option<(String, String)>,
    /// Address stash for `select_pattern` to call back into `open` when
    /// the user picks a pattern from the `c`-key pattern-select UI.
    pub(super) open_addr: Option<String>,
    // Command popup state. `/model` and every other command's nested levels
    // come from `CommandInfo::args` (the inspect payload), so the popup needs
    // nothing but this list.
    pub(super) commands: Vec<CommandInfo>,
    pub(super) command_popup: Option<CommandPopupState>,
    /// TUI-local leader-key popup (navigation, zen mode, activity pane, ...).
    /// Never sent to the backend.
    pub(super) leader: Option<leader::Leader>,
    /// History of sent messages for Up/Down recall (newest appended last).
    pub(super) input_history: Vec<String>,
    /// The `ask_user` questions awaiting an answer here, oldest first.
    ///
    /// One `ask_user` call can ask several at once, so these are answered as
    /// a flow: `question_index` is the one on screen, each carries its own
    /// cursor and marks, and nothing is sent until the last one is confirmed —
    /// which is what lets `←/→` go back and adjust an earlier answer. Takes
    /// over the input area while non-empty; Esc hides the whole set (they stay
    /// pending server-side, and typed messages answer them oldest first).
    pub(super) questions: Vec<PendingQuestion>,
    /// Which of [`Self::questions`] is on screen.
    pub(super) question_index: usize,
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
/// Returns a string like "15s", "2m05s", or "1h30m"; "" if parsing fails.
/// Mirrors `format_elapsed_ms` at minute scale so both halves of the
/// dual-time progress line (`2m05s / 10m20s`) have second granularity.
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
    format_duration_secs(secs as u64, DurationStyle::Precise)
}

/// Format a wall-clock duration in milliseconds for the live loop ticker.
/// Delegates to `jyc_core::duration` (Ticking style): `"12.4s"` below a
/// minute, then `"1m05s"`, `"1h02m03s"`. Used by the dashboard Details
/// panel, chat-mode info pane, and chat progress line.
pub(super) fn format_elapsed_ms(ms: u64) -> String {
    format_duration_ms(ms, DurationStyle::Ticking)
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
    format_duration_secs(secs as u64, DurationStyle::Precise)
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
    // A half-typed command (`3`, or an armed `y`) and an open selection must not
    // survive the way back into the input, or the next keystroke would complete
    // them against a transcript the user is no longer pointing at.
    app.chat.pending_count = 0;
    app.chat.pending_y = false;
    app.chat.selection_anchor = None;
}

/// Move the cursor (see `ChatState::cursor_step`) and tell the status line what
/// a selection now covers, so the growing range is readable without hunting for
/// it on screen.
fn step_cursor(app: &mut App, dir: i32, extend: bool) {
    let rows = app.chat.cursor_step(dir, extend);
    report_selection(app, rows);
}

/// `gg` / `G` with the cursor showing.
fn jump_cursor(app: &mut App, to_top: bool) {
    let rows = app.chat.cursor_jump(to_top);
    report_selection(app, rows);
}

/// Say how much the cursor now has selected, if anything.
fn report_selection(app: &mut App, rows: usize) {
    if rows > 0 {
        let noun = if rows == 1 { "line" } else { "lines" };
        app.set_status(format!("{rows} {noun} selected"));
    }
}

/// `y` while rows are selected: copy the selection, then leave visual mode with
/// the cursor on the *first* row of what was copied — the row the selection grew
/// out of, the way linewise vim ends a yank — following the view if that row
/// ended up off screen. (`Esc` drops the selection without
/// moving the cursor; see the message-pane keys.) The count is for the
/// no-selection form (`y3y`), so a stray digit must not carry into the next
/// command.
fn yank_selection(app: &mut App) {
    let Some((start, end)) = app.chat.selection_range() else {
        return;
    };
    app.chat.selection_anchor = None;
    app.chat.pending_count = 0;
    app.chat.pending_y = false;
    let (text, rows) = copy_rows(app, start, end - start + 1);
    app.chat.cursor_line = start;
    app.chat.scroll_to_show(start);
    report_yank(app, text, rows);
}

/// `yy` / `y3y`: copy `count` transcript rows starting at the cursor.
fn yank_from_cursor(app: &mut App, count: usize) {
    let start = app.chat.cursor_line;
    let (text, rows) = copy_rows(app, start, count);
    report_yank(app, text, rows);
}

/// Text of `count` rendered transcript rows from `start`, and how many there
/// were.
///
/// The rows are the rendered ones — the same wrapping the user sees — so one
/// message can copy as several lines, exactly as it reads on screen. The live
/// tail (activity progress, the streaming reply) is rebuilt every frame and is
/// not part of the transcript, so a range reaching it is truncated to the
/// history and a cursor sitting in it copies nothing.
fn copy_rows(app: &App, start: usize, count: usize) -> (String, usize) {
    let Some((_, lines)) = app.chat.render_cache.as_ref() else {
        return (String::new(), 0);
    };
    let end = start.saturating_add(count).min(lines.len());
    let rows = &lines[start.min(lines.len())..end];
    (
        rows.iter().map(line_text).collect::<Vec<_>>().join("\n"),
        rows.len(),
    )
}

/// Hand the text to the event loop, which sends it to the terminal, and say how
/// much of the request actually made it.
fn report_yank(app: &mut App, text: String, rows: usize) {
    if rows == 0 {
        app.set_status("Nothing to copy".to_string());
        return;
    }
    app.chat.pending_clipboard = Some(text);
    let noun = if rows == 1 { "line" } else { "lines" };
    app.set_status(format!("Copied {rows} {noun}"));
}

/// The plain text of one rendered row, without the padding the renderer adds so
/// a message's background reaches the edge of the pane.
fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// Move the explorer selection by `delta` rows, clamped to the current
/// topic list.
fn explorer_move(app: &mut App, delta: i64) {
    let len = app.state.as_ref().map(|s| s.topics.len()).unwrap_or(0);
    if len == 0 {
        app.chat.explorer_selected = 0;
        return;
    }
    let cur = app.chat.explorer_selected as i64;
    app.chat.explorer_selected = cur.saturating_add(delta).clamp(0, len as i64 - 1) as usize;
}

/// Open the topic currently selected in the explorer pane. All channel
/// types use the unified `/ws/<channel>/<topic>` endpoint.
fn explorer_open_selected(app: &mut App) {
    let info = app.state.as_ref().and_then(|s| {
        s.topics
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
            // Focus on the new topic's input; hide the explorer so
            // the user lands in the chat, not the pane they used to
            // pick the topic.
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
        // Same popup as typing `/`, but the input field stays untouched —
        // so the popup filters off whatever the field already holds.
        // Chatting-only: the popup is meaningless in PatternSelect.
        LocalAction::OpenCommandPopup => {
            if app.chat.phase == ChatPhase::Chatting {
                app.chat.focus = ChatFocus::ChatPane;
                // Same just-in-time refresh as the `/` key: without it the
                // popup shows "Loading..." until the user types a slash.
                app.refresh_chat_commands();
                app.chat.command_popup = Some(CommandPopupState::new());
            }
        }
        LocalAction::ToggleThinking => {
            app.chat.thinking_expanded = !app.chat.thinking_expanded;
        }
        LocalAction::ToggleToolDetail => {
            app.chat.tool_detail_expanded = !app.chat.tool_detail_expanded;
        }
    }
}

/// Toggle the topic explorer. When opening, snap the selection to the
/// topic currently open in the chat pane — `sync_explorer_selection`
/// only follows the chat topic while the explorer is *unfocused*, so
/// without this the explorer would open on a stale row.
fn toggle_explorer_snapped(app: &mut App) {
    app.chat.toggle_explorer();
    if !app.chat.explorer_visible {
        return;
    }
    let idx = app.state.as_ref().and_then(|s| {
        let topic = app.chat.topic.as_deref()?;
        let channel = app.chat.channel.as_deref()?;
        s.topics
            .iter()
            .position(|t| t.name == topic && t.channel == channel)
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
    // `pending_rx` select! arm in topic_manager.rs intercepts the
    // leading "/" and runs CancelCommandHandler, which fires the
    // per-topic CancellationToken. Restrict to Chatting — there is
    // no topic to cancel in PatternSelect.
    let is_ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_c && app.chat.phase == ChatPhase::Chatting {
        // Close any open command popup so the cancel path runs cleanly.
        app.chat.command_popup = None;
        app.chat.leader = None;
        app.chat.dismiss_question();
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
    // The popup has no input box of its own: the chat input field IS its
    // filter. Only navigation keys belong to the popup — every other key
    // falls through to the editor below (and the filter follows on sync).
    sync_command_popup(app);
    if let Some(ref mut popup) = app.chat.command_popup {
        match handle_popup_key(key, popup, &app.chat.commands) {
            PopupAction::PassThrough => {}
            PopupAction::None => return,
            PopupAction::Complete(cmd) => {
                app.chat.populate_editor(&cmd);
                // Re-filter now, not on the next keypress: the list must
                // match what the field just got filled with — and this is
                // also where a completion with nothing below it (`/plan `)
                // closes the popup, since the sync drops the level.
                sync_command_popup(app);
                return;
            }
            PopupAction::Close => {
                app.chat.command_popup = None;
                return;
            }
            PopupAction::Send(cmd) => {
                app.chat.command_popup = None;
                // The field held the filter that selected `cmd`; clearing
                // it matches a normal send (`send_message`) and stops the
                // command from sitting there ready to be sent twice.
                app.chat.editor = empty_chat_editor();
                app.chat.send_message_inner(cmd);
                return;
            }
        }
    }

    // "/" opens the command popup as the first char of an empty input.
    // The slash also lands in the input field — the popup filters off it.
    let is_slash = key.code == KeyCode::Char('/') && !key.modifiers.contains(KeyModifiers::CONTROL);
    if is_slash
        && app.chat.phase == ChatPhase::Chatting
        && app.chat.focus == ChatFocus::ChatPane
        && app.chat.text().trim().is_empty()
    {
        // Compute commands for the chat topic just-in-time so the popup
        // reflects the topic the user is typing into (not whichever row
        // is highlighted in the table).
        app.refresh_chat_commands();
        app.chat.leader = None;
        app.chat.command_popup = Some(CommandPopupState::new());
        app.chat.editor.input(key);
        sync_command_popup(app);
        return;
    }

    // Pending questions own the keyboard while visible: the agent is blocked
    // mid-turn waiting for these answers. Esc hides them (focus back to the
    // editor, the questions still pending — each next typed message becomes
    // the free-form answer of the oldest one via try_answer interception).
    if app.chat.active_question() {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => app.chat.select_question_prev(),
            KeyCode::Down | KeyCode::Char('j') => app.chat.select_question_next(),
            KeyCode::Left => app.chat.step_question(-1),
            KeyCode::Right => app.chat.step_question(1),
            KeyCode::Enter => app.chat.confirm_question(),
            KeyCode::Char(' ') => app.chat.space_question(),
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let idx = c as usize - '1' as usize;
                if idx < app.chat.current_question().map_or(0, |q| q.options.len()) {
                    app.chat.pick_question_idx(idx);
                }
            }
            KeyCode::Esc => app.chat.dismiss_question(),
            _ => {}
        }
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

            // Explorer pane: navigate the topic list; Enter switches the
            // chat to the selected topic. Esc returns focus to the input.
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

            // Message area: the cursor is showing here, so these keys move the
            // cursor rather than scrolling (the wheel and PgUp/PgDn still
            // scroll, and the cursor rides along — see `carry_cursor`). Digits
            // before a key count lines: `5j`, `y3y`. A Shifted movement key
            // opens a selection that every later movement grows, and `y` copies
            // it. `Esc` drops the selection first and only then returns focus to
            // the input field (it never exits the chat); any other key refocuses
            // the input, so the user can move around and then just start typing.
            if app.chat.focus == ChatFocus::MessageArea {
                // Terminals differ in whether they report Shift+arrow at all
                // (many send the same bytes as the plain key), so Shift+J/K is
                // the reliable way to start a selection; Shift+arrow works
                // wherever the terminal can tell them apart.
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                match key.code {
                    KeyCode::Esc => {
                        // Dropping an open selection comes first, and the next
                        // `Esc` returns to the input — leaving both at once
                        // would throw a selection away by accident. Staying in
                        // the pane is how `Esc` leaves visual mode in vim.
                        if app.chat.selection_anchor.take().is_none() {
                            refocus_input(app)
                        }
                    }
                    KeyCode::Up => step_cursor(app, -1, shift),
                    KeyCode::Down => step_cursor(app, 1, shift),
                    // Uppercase means Shift was held (that is how a terminal
                    // reports it), so `J`/`K` are the selection-friendly forms.
                    KeyCode::Char('k') => step_cursor(app, -1, false),
                    KeyCode::Char('K') => step_cursor(app, -1, true),
                    KeyCode::Char('j') => step_cursor(app, 1, false),
                    KeyCode::Char('J') => step_cursor(app, 1, true),
                    KeyCode::Char('G') => jump_cursor(app, false),
                    KeyCode::Char('g') if gg_jump => jump_cursor(app, true),
                    KeyCode::Char('g') => {}
                    // With rows selected, one `y` copies them. Without a
                    // selection `y` is the first half of a yank: arming it lets
                    // a count sit between the halves, so `yy`, `y3y` and `3yy`
                    // all mean the same thing.
                    KeyCode::Char('y') if app.chat.selection_anchor.is_some() => {
                        yank_selection(app)
                    }
                    KeyCode::Char('y') if app.chat.pending_y => {
                        app.chat.pending_y = false;
                        let count = app.chat.take_count();
                        yank_from_cursor(app, count)
                    }
                    KeyCode::Char('y') => app.chat.pending_y = true,
                    KeyCode::Char(c) if c.is_ascii_digit() => app.chat.push_count_digit(c),
                    _ => refocus_input(app),
                }
                return;
            }

            // Chat input field. Everything not matched here is delegated
            // to the textarea (character input, editing keys, undo/redo).
            match key.code {
                // Esc does not leave the topic: returning to the dashboard
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
            // The command popup filters off this field — refresh or drop it.
            sync_command_popup(app);
        }
    }
}

/// The command popup has no input box of its own: its filter is the chat
/// input field. Mirror the text into the popup, and close it once the field
/// is empty again (the user deleted the `/`) or its text walks off the
/// command tree into free text (`/grant <path>`, `/plan on`).
fn sync_command_popup(app: &mut App) {
    if app.chat.command_popup.is_none() {
        return;
    }
    let text = app.chat.text();
    // Close on an empty field, and on a path that leads to free text
    // (`/grant <path>`, `/plan on`) — there is nothing to complete there, so
    // the field should behave like the popup was never open.
    if text.trim().is_empty() || resolve_level(&text, &app.chat.commands).is_none() {
        app.chat.command_popup = None;
        return;
    }
    if let Some(popup) = app.chat.command_popup.as_mut()
        && popup.filter != text
    {
        popup.filter = text;
        popup.selected = 0;
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
/// Hit-testing: the wheel scrolls the pane it hovers — the message area or,
/// when it is visible, the topic info pane. The activity log, the explorer
/// and the input area keep absorbing the event, so wheeling next to the editor
/// does nothing. Neither pane drags focus around: the info pane's offset is
/// advanced directly (it has no cursor), while the message area takes
/// `MessageArea` focus because its cursor and the focus-routed `scroll_up` /
/// `scroll_down` belong together.
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
    let pos = Position::new(mouse.column, mouse.row);
    if app.chat.info_visible && app.chat.last_info_area.is_some_and(|r| r.contains(pos)) {
        // Advance the offset directly rather than routing through
        // `scroll_up`/`scroll_down`: those go by focus, and pulling focus away
        // from the editor mid-typing is exactly what this handler must not do.
        // The info pane has no cursor, so nothing here needs it.
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                app.chat.info_scroll = app.chat.info_scroll.saturating_sub(1)
            }
            MouseEventKind::ScrollDown => app.chat.info_scroll += 1,
            _ => {}
        }
        return;
    }
    if !app.chat.last_message_area.is_some_and(|r| r.contains(pos)) {
        return;
    }
    // The message area does take focus: its cursor and the focus-routed scroll
    // belong together, and wheeling is how the user aims at a row.
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
    //   │  ┌── topic info pane (20% wide, only when info_visible) ──┐  │
    //   │  │   ...                                                   │  │
    //   │  └─────────────────────────────────────────────────────────┘  │
    //   ├────────── bottom area: status bar + activity pane ────────────┤
    //   │  status bar (1 line; only when info_visible)                  │
    //   │  activity pane (bottom 20% / 80% / full when visible)         │
    //   └───────────────────────────────────────────────────────────────┘
    //
    // Status bar and topic info pane have independent visibility flags
    // (leader `s` / `i`); zen mode hides both.

    if app.chat.phase == ChatPhase::PatternSelect {
        // Pattern select is the initial screen when no topic is chosen.
        // Info row and status row are independent; zen hides both.
        let mut constraints = Vec::with_capacity(3);
        if app.chat.info_visible {
            constraints.push(Constraint::Length(1)); // Topic info pane
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
            render_topic_info_pane(frame, chunks[i], app);
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
        render_topic_info_pane(frame, top_cols[1], app);
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
/// focused, following the topic currently open in the chat pane.
fn sync_explorer_selection(app: &mut App) {
    let Some(s) = app.state.as_ref() else {
        app.chat.explorer_selected = 0;
        return;
    };
    let len = s.topics.len();
    if len == 0 {
        app.chat.explorer_selected = 0;
        return;
    }
    if app.chat.explorer_selected >= len {
        app.chat.explorer_selected = len - 1;
    }
    if app.chat.focus != ChatFocus::ExplorerPane
        && let (Some(topic), Some(channel)) = (&app.chat.topic, &app.chat.channel)
        && let Some(idx) = s
            .topics
            .iter()
            .position(|t| &t.name == topic && &t.channel == channel)
    {
        app.chat.explorer_selected = idx;
    }
}

/// Render the left-side topic explorer pane (20% wide when shown).
///
/// Lists all topics from the latest overview poll with a status dot
/// (green = processing, yellow = queued, cyan = waiting, red = error),
/// highlighting the topic currently open in the chat pane. The cursor row is
/// marked whether or not this pane has focus, so switching focus back to the
/// input never hides which topic the arrow keys would move. The list is
/// rebuilt from `app.state` on every render, so it stays live.
pub(super) fn render_explorer(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.chat.focus == ChatFocus::ExplorerPane;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    // Only the right edge (against the chat pane) gets a border — no
    // title, no top edge — with one row of top padding so the topic
    // list breathes a little.
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(border_style)
        .padding(Padding::top(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(s) = app.state.as_ref() else {
        return;
    };

    let current = app.chat.topic.as_ref().zip(app.chat.channel.as_ref());
    let selected = app.chat.explorer_selected;

    // Scroll window: keep the selected row visible.
    let height = inner.height as usize;
    let offset = window_offset(s.topics.len(), selected, height);

    let lines: Vec<Line> = s
        .topics
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, t)| {
            let dot_style = match t.status {
                TopicStatus::Processing => Style::default().fg(Color::Green),
                TopicStatus::Queued => Style::default().fg(Color::Yellow),
                TopicStatus::Idle => Style::default().fg(Color::DarkGray),
                TopicStatus::Error => Style::default().fg(Color::Red),
            };
            let is_current = current == Some((&t.name, &t.channel));
            let is_selected = i == selected;
            let name_style = if is_current {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            // The selected row uses the same two-column `→` gutter + DIM as
            // the command and question popups. The status dot keeps its own
            // color on that row too, so it stays readable at a glance.
            let sel = if is_selected {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(if is_selected { "→ " } else { "  " }, sel),
                Span::styled("● ", dot_style),
                Span::styled(t.name.as_str(), name_style.patch(sel)),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Find the `TopicSummary` currently in scope. Used by both the
/// topic info pane and the chat header so the two views agree on
/// which topic is "selected".
///
/// Lookup order matches the legacy chat-pane behavior:
/// 1. The topic the chat pane is currently bound to (`app.chat.topic`).
/// 2. The currently-selected row of the topic table.
fn selected_topic_summary(app: &App) -> Option<&jyc_types::TopicSummary> {
    let state = app.state.as_ref()?;
    state
        .topics
        .iter()
        .find(|t| Some(&t.name) == app.chat.topic.as_ref())
        .or_else(|| app.table_state.selected().and_then(|i| state.topics.get(i)))
}

/// Render the right-hand topic info pane (always 20% wide when shown).
///
/// Displays topic name, channel, pattern, model, mode, tokens, a
/// processing indicator, and the changed-files list (which can scroll
/// when it overflows the pane). Wraps content in a bordered `Block`
/// so it is visually separable from the borderless chat pane. Takes
/// `&mut App` because the changed-files section owns
/// `app.chat.info_scroll`, which is clamped on every render.
pub(super) fn render_topic_info_pane(frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.chat.focus == ChatFocus::InfoPane;
    // The left edge (against the chat pane) gets a vertical border, and the
    // top edge carries the title inline with the top border so the title
    // row acts as a separator between the heading and the content below.
    // When focused, paint the border yellow so the user knows they own
    // the scroll keys (mirrors render_activity_log_inner).
    // Chat screen: the title and top border are removed, leaving only the
    // left border to separate the pane from the chat content.
    let mut block = if app.chat.phase == ChatPhase::Chatting {
        // One row of top padding so the content does not hug the pane top.
        Block::default()
            .borders(Borders::LEFT)
            .padding(Padding::top(1))
    } else {
        Block::default()
            .title("── Topic Info ")
            .borders(Borders::TOP | Borders::LEFT)
    };
    if focused {
        block = block.border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // The content rect rather than `area`: the pane's left border separates it
    // from the chat pane, and the wheel belongs to the chat side of that line.
    app.chat.last_info_area = Some(inner);

    let lines: Vec<Line> = if let Some(t) = selected_topic_summary(app) {
        let mut out: Vec<Line> = Vec::new();
        out.push(Line::from(vec![
            Span::styled("Topic: ", Style::default().add_modifier(Modifier::BOLD)),
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
        // Branch is resolved server-side and shipped on TopicSummary.branch.
        // Skipped when the selected topic's topic_path isn't a git repo
        // (most chat-channel topics: feishu/wecom).
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
        // Cache util row — cache hits as a share of total input.
        // Omitted when the provider reports no cache data.
        let mut cache_util_spans = Vec::with_capacity(2);
        push_cache_utilization_span(&mut cache_util_spans, t);
        if !cache_util_spans.is_empty() {
            out.push(Line::from(cache_util_spans));
        }
        // Cache create row — cache **write** tokens billed at the
        // creation rate (Anthropic ~1.25× input; GPT-5.6 reports
        // `cache_write_tokens` too). Rendered only when the running
        // total is non-zero, so other sessions see no extra row.
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
        // Separated section: the agent's task list (`.jyc/tasks.json`),
        // placed between the cost row and the files section. Same
        // `[ ] [~] [x]` markers and ids the agent's own tools print, so what
        // the user sees and what `task_update` takes agree. Completed fades,
        // in-progress is the row you want to spot. Omitted entirely when the
        // topic has no list; the whole pane (list included) scrolls.
        if !t.tasks.is_empty() {
            out.push(Line::default());
            let (done, total) = t.tasks.progress();
            out.push(Line::from(Span::styled(
                format!("Tasks ({done}/{total}):"),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for item in &t.tasks.items {
                let style = match item.status {
                    jyc_types::task::TaskStatus::InProgress => Style::default().fg(Color::Yellow),
                    jyc_types::task::TaskStatus::Completed => Style::default().fg(Color::DarkGray),
                    jyc_types::task::TaskStatus::Pending => Style::default(),
                };
                out.push(Line::from(Span::styled(
                    format!("  {} {}. {}", item.status.marker(), item.id, item.text),
                    style,
                )));
            }
        }
        if t.status == TopicStatus::Processing {
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
        // resolved server-side and shipped on `TopicSummary.changed_files`
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
        vec![Line::from("Select a topic")]
    };

    // Wrap here rather than with `Paragraph::wrap()` so that the row count the
    // scroll clamp uses is the count actually drawn. This pane is 20% of the
    // width, so a changed-file path or a long topic title occupies several
    // screen rows; clamping against logical lines understates the content and
    // stops the scroll short of the bottom — `End`/`G` could never reach the
    // last rows. The message pane obeys the same contract (see
    // `wrap_styled_lines`): scroll math counts visual rows.
    //
    // Slice-skip in Rust (matching the activity pane's pattern) also keeps
    // `usize::MAX` — what `End`/`G` store — out of `Paragraph::scroll`, whose
    // `offset_y + height` math would overflow and panic the TUI.
    // Offset-from-top: `info_scroll == 0` shows the first rows, the max the last.
    let wrapped = wrap_styled_lines(lines, inner.width as usize);
    let max_skip = wrapped.len().saturating_sub(inner.height as usize);
    let skip = app.chat.info_scroll.min(max_skip);
    frame.render_widget(
        Paragraph::new(wrapped.into_iter().skip(skip).collect::<Vec<_>>()),
        inner,
    );
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

    // One row per pattern, windowed so the cursor stays on screen: `Wrap` used
    // to sit here for long paths, but a wrapped row pushed the cursor's row out
    // of the window [`window_offset`] picked — and it needed `trim: false` to
    // keep the unselected rows' blank gutter, which is why the `→` was the only
    // indented line. Long paths clip at the pane edge now, like the popups.
    let height = inner.height as usize;
    let offset = window_offset(app.chat.patterns.len(), app.chat.pattern_selected, height);
    let lines: Vec<Line> = app
        .chat
        .patterns
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, pattern)| {
            let selected = i == app.chat.pattern_selected;
            let gutter = if selected { "→ " } else { "  " };
            if selected {
                Line::from(vec![Span::styled(
                    format!("{gutter}{pattern}"),
                    Style::default().add_modifier(Modifier::DIM),
                )])
            } else {
                Line::from(vec![Span::raw(gutter), Span::raw(pattern)])
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Toggle `idx` in a mark list. Order is irrelevant while marking - the submit
/// sorts, so the answer follows the option list's own order.
fn toggle_marked(marked: &mut Vec<usize>, idx: usize) {
    match marked.iter().position(|&i| i == idx) {
        Some(at) => {
            marked.remove(at);
        }
        None => marked.push(idx),
    }
}

/// The line under an `ask_user` question box. A function, not a constant: a
/// batch changes what `Enter` does, and the layout has to measure the very text
/// the renderer draws — see [`question_chrome_rows`].
fn question_hint(total: usize) -> String {
    let enter = if total > 1 {
        "Enter next, last one sends"
    } else {
        "Enter send"
    };
    format!(
        "Up/Down or j/k move - Space or 1-9 marks/picks - {enter} - Esc hide, then type your answer"
    )
}

/// Rows a question box spends on everything but the options: the question and
/// the hint (both wrap, so both are measured instead of assumed), the two blank
/// spacers, and a row of slack for word-boundary wrapping. The box's own borders
/// are not in here — the renderer gets them excluded already from `block.inner`,
/// and the layout adds them. Both sides call this so neither can size the box
/// short of what the other draws: a row short used to cost the hint, then the
/// cursor.
fn question_chrome_rows(question: &str, total: usize, width: u16) -> usize {
    count_wrapped_lines(question, width) + count_wrapped_lines(&question_hint(total), width) + 3
}

pub(super) fn render_question_box(frame: &mut Frame, area: Rect, app: &App) {
    let Some(q) = app.chat.current_question() else {
        return;
    };
    // How far into the batch the user is, and that the keys step between
    // questions. It goes in the border rather than inside the box because the
    // inner rows are measured (`question_chrome_rows`) - a row spent here
    // inside would cost an option row.
    let total = app.chat.questions.len();
    let title = if total > 1 {
        format!(
            " Question {}/{} \u{b7} \u{2190}/\u{2192} \u{b7} ",
            app.chat.question_index + 1,
            total
        )
    } else {
        " Question ".to_string()
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    // Clear first: the covered editor's draft text must not ghost through
    // the box's empty cells.
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(block, area);

    // The options get whatever rows the chrome leaves, windowed so the cursor is
    // among them: `select_question_next` clamps only to `options.len()`, so
    // without a window a list deeper than the box parks the `→` below the clip —
    // selectable, invisible. `question_chrome_rows` says which rows those are.
    let chrome = question_chrome_rows(&q.question, total, inner.width);
    let room = (inner.height as usize).saturating_sub(chrome).max(1);
    let off = window_offset(q.options.len(), q.selected, room);

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        q.question.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    lines.push(Line::from(""));
    for (i, opt) in q.options.iter().enumerate().skip(off).take(room) {
        // One row per option: a wrapped one would push the rows below it — and
        // the cursor with them — out of the box.
        // A marked option leads with a box, so it still reads as picked on the
        // dimmed rows that do not carry the cursor.
        let mark = if q.multi {
            if q.marked.contains(&i) {
                "[x] "
            } else {
                "[ ] "
            }
        } else {
            ""
        };
        let label = truncate_to_width(
            &format!("{mark}{}. {opt}", i + 1),
            inner.width.saturating_sub(2 + mark.chars().count() as u16) as usize,
        );
        if i == q.selected {
            lines.push(Line::from(Span::styled(
                format!("→ {label}"),
                Style::default().add_modifier(Modifier::DIM),
            )));
        } else {
            lines.push(Line::from(Span::raw(format!("  {label}"))));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        question_hint(total),
        Style::default().fg(Color::DarkGray),
    )));

    // `Wrap { trim: false }` is required so the option rows' two-column
    // gutter survives: the default `trim: true` strips leading whitespace
    // per line, which leaves the unselected rows' blank gutter gone and the
    // selected row's `→` as the only indented one.
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
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
    let t = selected_topic_summary(app);
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
    // Divergence from the Topic Info pane: when `pattern` is `None`
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
    // currently focused topic. Falls back to empty slice if no live data
    // has been seeded yet (transient state during hydrate).
    let activity_vec: Vec<jyc_types::ActivityEntry> =
        if app.chat.visible && app.chat.phase == ChatPhase::Chatting {
            let (chan, topic) = (app.chat.channel.clone(), app.chat.topic.clone());
            match (chan, topic) {
                (Some(c), Some(t)) => app.chat.live_activity_for(&c, &t).cloned().collect(),
                _ => Vec::new(),
            }
        } else if let Some(state) = &app.state {
            // Overview mode: show the activity for the table-selected topic
            // (also pulled from live buffers, hydrated when the row is selected).
            let selected_idx = app.table_state.selected();
            if let Some(idx) = selected_idx {
                if let Some(t) = state.topics.get(idx) {
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
        // Chat screen: only the top edge borders the chat pane above;
        // bottom/left/right sit at the screen edge or against an adjacent
        // pane and are intentionally left open. No title on this screen.
        Borders::TOP,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_activity_log_inner(
    frame: &mut Frame,
    area: Rect,
    activity: &[jyc_types::ActivityEntry],
    scroll_offset: usize,
    hscroll: usize,
    focused: bool,
    borders: Borders,
    titled: bool,
) {
    // The chat screen renders the activity pane without a title; the
    // dashboard/overview screen keeps the `Activity` title in its top
    // border, using the same plain ` Text ` format as the other dashboard
    // panes (Channels / Topics / Details). See chat/mod.rs vs
    // dashboard/mod.rs call sites.
    let mut block = if titled {
        Block::default().title(" Activity ").borders(borders)
    } else {
        Block::default().borders(borders)
    };
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
            topic: None,
            channel: None,
            messages: vec![],
            editor: empty_chat_editor(),
            focus: ChatFocus::ChatPane,
            scroll: 0,
            info_scroll: 0,
            activity_scroll: 0,
            last_message_area: None,
            last_info_area: None,
            last_max_scroll: 0,
            pending_clipboard: None,
            pending_y: false,
            cursor_line: usize::MAX,
            last_total_lines: 0,
            pending_count: 0,
            selection_anchor: None,
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
            thinking_expanded: false,
            tool_detail_expanded: false,
            live_processing: std::collections::BTreeMap::new(),
            live_tick_ms: std::collections::BTreeMap::new(),
            last_seen_id: std::collections::BTreeMap::new(),
            last_pushed_chat_id: std::collections::BTreeMap::new(),
            last_hydrated_key: None,
            open_addr: None,
            commands: vec![],
            command_popup: None,
            leader: None,
            input_history: vec![],
            questions: vec![],
            question_index: 0,
            history_pos: None,
            token: None,
        }
    }

    pub(super) fn open(
        &mut self,
        addr: &str,
        channel: Option<&str>,
        initial_topic: Option<&str>,
        token: Option<String>,
    ) {
        self.visible = true;
        self.phase = if initial_topic.is_some() {
            ChatPhase::Chatting
        } else {
            ChatPhase::PatternSelect
        };
        self.patterns.clear();
        self.pattern_selected = 0;
        self.channel = channel.map(|s| s.to_string());
        self.topic = initial_topic.map(|s| s.to_string());
        self.token = token;
        self.messages.clear();
        self.editor = empty_chat_editor();
        self.focus = ChatFocus::ChatPane;
        self.scroll = 0;
        self.activity_scroll = 0;
        self.info_scroll = 0;
        self.last_message_area = None;
        self.last_max_scroll = 0;
        self.reset_cursor();
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
        // Stash addr so the explorer pane can switch topics later.
        self.open_addr = Some(addr.to_string());

        // No WS yet — the chat starts in PatternSelect (if no initial topic)
        // and opens a scoped WS only after the user picks a pattern
        // (see `open_pattern_select` + `select_pattern`).
        if initial_topic.is_none() {
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

        let url = match (channel, initial_topic) {
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
        self.topic = None;
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
        self.reset_cursor();
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

    /// Clear state and set topic — used by `select_pattern` for the WS flow
    /// and directly by tests to verify state-clearing without a tokio runtime.
    fn select_pattern_inner(&mut self, pattern: String) {
        self.phase = ChatPhase::Chatting;
        self.topic = Some(pattern);
        self.editor = empty_chat_editor();
        self.scroll = 0;
        self.reset_cursor();
        self.messages.clear();
        self.input_history.clear();
        self.history_pos = None;
        self.last_hydrated_key = None;
        // A queued batch belongs to the topic being left behind. Keeping it
        // would hide the new topic's question behind it: `current_question`
        // reads `questions[question_index]`, and a first entry from the old
        // topic makes `active_question` false forever, so no box is drawn.
        // Dropping the queue is exactly what Esc does - the questions stay
        // pending server-side, where a typed message still answers them.
        self.questions.clear();
        self.question_index = 0;
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
    /// (activity, topic info, status bar, explorer) and hides all of
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

    /// Toggle the topic info pane (leader-key popup `i`). Hiding it
    /// while focused moves focus back to the chat pane.
    pub(super) fn toggle_info_pane(&mut self) {
        self.info_visible = !self.info_visible;
        if !self.info_visible && self.focus == ChatFocus::InfoPane {
            self.focus = ChatFocus::ChatPane;
        }
    }

    /// Toggle the topic explorer pane (left side). Opening moves focus
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

    /// Put the cursor away because the transcript is being replaced: a new
    /// topic starts reading at the end of its newest message (the renderer places
    /// it, once there are messages to place it on), with nothing selected and no
    /// half-finished command.
    fn reset_cursor(&mut self) {
        self.cursor_line = usize::MAX;
        self.selection_anchor = None;
        self.pending_count = 0;
        self.pending_y = false;
    }

    /// Transcript line the viewport starts at. `scroll` counts lines from the
    /// *bottom* of the scrollable range, so the two convert through
    /// `last_max_scroll`, which the renderer measures every frame.
    fn view_start_line(&self) -> usize {
        self.last_max_scroll - self.scroll.min(self.last_max_scroll)
    }

    /// Consume the digit prefix a command was typed with (`3j`, `y3y`); 1 when
    /// there was none.
    pub(super) fn take_count(&mut self) -> usize {
        let n = self.pending_count.max(1);
        self.pending_count = 0;
        n
    }

    /// Extend the digit prefix with `digit`, capped so a held key cannot run
    /// away from any reasonable transcript.
    fn push_count_digit(&mut self, digit: char) {
        debug_assert!(digit.is_ascii_digit(), "only called for digit keys");
        let value = digit as usize - '0' as usize;
        self.pending_count = (self.pending_count * 10 + value).min(999);
    }

    /// Scroll the view the minimum needed to bring `line` inside it.
    fn scroll_to_show(&mut self, line: usize) {
        let Some(height) = self
            .last_message_area
            .map(|a| a.height as usize)
            .filter(|h| *h > 0)
        else {
            return;
        };
        let start = self.view_start_line();
        if line >= start + height {
            self.scroll = self.last_max_scroll.saturating_sub(line - height + 1);
        } else if line < start {
            self.scroll = self.last_max_scroll.saturating_sub(line);
        }
    }

    /// The selected rows, lowest first, clamped to the transcript; `None` when
    /// nothing is selected. The ends are the anchor and the cursor in whichever
    /// order they came out, so moving back across the anchor shrinks the
    /// selection and crossing it flips which end is which.
    pub(super) fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        let last = self.last_total_lines.checked_sub(1)?;
        let from = anchor.min(self.cursor_line).min(last);
        let to = anchor.max(self.cursor_line).min(last);
        Some((from, to))
    }

    /// Move the cursor by the pending count of lines in direction `dir`
    /// (`j`/`k`, Up/Down). The view only starts scrolling once the cursor runs
    /// into its top or bottom edge, so a big count stops at the edge instead of
    /// dragging the whole screen along.
    ///
    /// `extend` (a Shift movement key) opens a selection anchored where the
    /// cursor was. From then on *every* movement grows it, Shift or not — vim's
    /// visual-line mode, where `Esc` is the way out. Returns the number of
    /// selected rows, 0 when nothing is selected.
    pub(super) fn cursor_step(&mut self, dir: i32, extend: bool) -> usize {
        let Some(last) = self.last_total_lines.checked_sub(1) else {
            return 0;
        };
        let from = self.cursor_line.min(last);
        if extend {
            self.selection_anchor = Some(self.selection_anchor.unwrap_or(from));
        }
        let to = (from as i32 + dir * self.take_count() as i32).clamp(0, last as i32) as usize;
        self.cursor_line = to;
        self.scroll_to_show(to);
        self.selected_rows()
    }

    /// `gg` / `G`: cursor and view to the first / last transcript line, growing
    /// an open selection along the way. Returns the number of selected rows.
    pub(super) fn cursor_jump(&mut self, to_top: bool) -> usize {
        self.pending_count = 0;
        let Some(last) = self.last_total_lines.checked_sub(1) else {
            return 0;
        };
        self.cursor_line = if to_top { 0 } else { last };
        self.scroll = if to_top { self.last_max_scroll } else { 0 };
        self.selected_rows()
    }

    /// How many rows the selection covers, for the status line.
    fn selected_rows(&self) -> usize {
        self.selection_range().map_or(0, |(from, to)| to - from + 1)
    }

    /// Carry the cursor along a view-only move (wheel, `PgUp`/`PgDn`,
    /// `Ctrl+B`/`Ctrl+F`, jumping): `before` and `after` are the viewport's
    /// start line around the move, and the cursor travels the same distance, so
    /// it keeps the screen row it was on while the text slides underneath. A
    /// move that clamps to nothing — the wheel at the bottom, a page at the top
    /// — leaves the cursor alone. Applies while the cursor is hidden too, so
    /// focusing the pane later finds it where the user was reading.
    fn carry_cursor(&mut self, before: usize, after: usize) {
        // While a selection is open the cursor is pinned to its text: scrolling
        // to look elsewhere must not grow or shrink what is selected. With
        // nothing selected the cursor rides the view instead, keeping the screen
        // row it was on (see the `PgUp`/`PgDn` rule).
        if self.selection_anchor.is_some() {
            return;
        }
        let Some(last) = self.last_total_lines.checked_sub(1) else {
            return;
        };
        let moved = after as i64 - before as i64;
        let cur = self.cursor_line.min(last) as i64 + moved;
        self.cursor_line = cur.clamp(0, last as i64) as usize;
    }

    pub(super) fn scroll_up(&mut self) {
        match self.focus {
            ChatFocus::ChatPane | ChatFocus::MessageArea => {
                let before = self.view_start_line();
                self.scroll = self.scroll.saturating_add(1).min(self.last_max_scroll);
                self.carry_cursor(before, self.view_start_line());
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
                let before = self.view_start_line();
                self.scroll = self.scroll.saturating_sub(1);
                self.carry_cursor(before, self.view_start_line());
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
            ChatFocus::ChatPane | ChatFocus::MessageArea => {
                let before = self.view_start_line();
                self.scroll = usize::MAX;
                self.carry_cursor(before, self.view_start_line());
            }
            ChatFocus::ActivityPane => self.activity_scroll = usize::MAX,
            // Info pane: offset-from-top, so "top" is offset = 0.
            ChatFocus::InfoPane => self.info_scroll = 0,
            ChatFocus::ExplorerPane => {}
        }
    }

    /// Jump to the latest message (bottom) of the focused pane.
    pub(super) fn scroll_to_bottom(&mut self) {
        match self.focus {
            ChatFocus::ChatPane | ChatFocus::MessageArea => {
                let before = self.view_start_line();
                self.scroll = 0;
                self.carry_cursor(before, self.view_start_line());
            }
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
                let before = self.view_start_line();
                self.scroll = self.scroll.saturating_add(page).min(self.last_max_scroll);
                self.carry_cursor(before, self.view_start_line());
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
                let before = self.view_start_line();
                self.scroll = self.scroll.saturating_sub(page);
                self.carry_cursor(before, self.view_start_line());
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
        // Reset history cursor so the next Up arrow recalls the message we
        // just sent, instead of decrementing a stale cursor into the now-larger
        // history (which would skip past the most recent entry — see PR #655).
        self.history_pos = None;

        // WebSocket-only flow: echo user message locally, send via WebSocket.
        let _ = self.topic.as_ref(); // topic must be set before send
        self.messages.push(ChatMessage {
            sender: "user".to_string(),
            text: text.clone(),
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
        });
        // The /ws/<channel>/<topic> URL already carries the topic name.
        // Both ScopedWsHandler (websocket channel) and TopicProxyHandler
        // (any other channel) bind the topic from the URL, so the payload
        // doesn't need a `topic` field.
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

    /// The question on screen — the batch entry the cursor points at.
    pub(super) fn current_question(&self) -> Option<&PendingQuestion> {
        self.questions.get(self.question_index)
    }

    /// Mutable view of the question on screen.
    fn current_question_mut(&mut self) -> Option<&mut PendingQuestion> {
        self.questions.get_mut(self.question_index)
    }

    /// Whether questions are pending for the topic open in this pane.
    pub(super) fn active_question(&self) -> bool {
        self.current_question()
            .is_some_and(|q| self.topic.as_deref() == Some(q.topic.as_str()))
    }

    /// `←/→`: step between the questions of a batch, wrapping. Each question
    /// keeps its own cursor and marks, so going back to adjust an answer is
    /// free - nothing has been sent yet.
    fn step_question(&mut self, delta: isize) {
        let total = self.questions.len() as isize;
        if total < 2 {
            return;
        }
        let current = self.question_index as isize;
        self.question_index = (current + delta).rem_euclid(total) as usize;
    }

    /// Move the question selection up (clamped at the first option).
    fn select_question_prev(&mut self) {
        if let Some(q) = self.current_question_mut()
            && q.selected > 0
        {
            q.selected -= 1;
        }
    }

    /// Move the question selection down (clamped at the last option).
    fn select_question_next(&mut self) {
        if let Some(q) = self.current_question_mut()
            && q.selected + 1 < q.options.len()
        {
            q.selected += 1;
        }
    }

    /// Number-key shortcut: mark this option in multi mode; in single mode
    /// settle the question on it, which is Enter's job - a question asked on
    /// its own therefore still sends at once, as it always did.
    fn pick_question_idx(&mut self, idx: usize) {
        {
            let Some(q) = self.current_question_mut() else {
                return;
            };
            if idx >= q.options.len() {
                return;
            }
            if q.multi {
                toggle_marked(&mut q.marked, idx);
                return;
            }
            q.selected = idx;
        }
        self.confirm_question();
    }

    /// `Space`: mark the option under the cursor when several picks are
    /// allowed; otherwise confirm the highlighted one outright.
    ///
    /// Single-select confirms rather than ignoring the key: an inert key in a
    /// panel that spells out what the keys do is a bug report waiting to
    /// happen, and confirming is what a user who just tapped Space means.
    fn space_question(&mut self) {
        let multi = self.current_question().is_some_and(|q| q.multi);
        if !multi {
            self.confirm_question();
            return;
        }
        if let Some(q) = self.current_question_mut() {
            toggle_marked(&mut q.marked, q.selected);
        }
    }

    /// Enter: settle the current question and step to the next one; on the
    /// last question, send every answer at once. Nothing leaves before that,
    /// which is what keeps `←/→` able to revisit an answer.
    fn confirm_question(&mut self) {
        if self.question_index + 1 < self.questions.len() {
            self.question_index += 1;
            return;
        }
        self.submit_questions();
    }

    /// Send every buffered answer as its own frame, oldest question first, and
    /// close the batch. A multi question left unmarked goes out as a cancel -
    /// the user backed out of that one rather than picking something.
    fn submit_questions(&mut self) {
        let questions = std::mem::take(&mut self.questions);
        self.question_index = 0;
        for q in questions {
            if !q.multi {
                if let Some(choice) = q.options.get(q.selected) {
                    self.send_question_response(&q.id, std::slice::from_ref(choice));
                }
                continue;
            }
            if q.marked.is_empty() {
                self.send_question_cancelled(&q.id);
                continue;
            }
            let mut marked = q.marked.clone();
            marked.sort_unstable();
            let choices: Vec<String> = marked
                .iter()
                .filter_map(|&i| q.options.get(i).cloned())
                .collect();
            self.send_question_response(&q.id, &choices);
        }
    }

    /// Hide the pending questions (Esc) and return focus to the editor.
    ///
    /// They stay alive server-side: the next typed message is routed to the
    /// oldest of them by the websocket inbound adapter's interception
    /// (`QuestionHub::try_answer`), making it that question's free-form answer,
    /// and the one after that answers the next. The daemon-side timeout still
    /// bounds anything left unanswered.
    fn dismiss_question(&mut self) {
        self.questions.clear();
        self.question_index = 0;
    }

    /// Send a `question_response` frame carrying every picked option. Even a
    /// single pick goes out as a list, so there is one frame shape.
    fn send_question_response(&self, id: &str, choices: &[String]) {
        let msg = serde_json::json!({
            "type": "question_response",
            "id": id,
            "choices": choices,
        })
        .to_string();
        if let Some(tx) = &self.ws_tx {
            let _ = tx.send(msg);
        }
    }

    /// Send a `question_response` frame marking the question as cancelled.
    fn send_question_cancelled(&self, id: &str) {
        let msg = serde_json::json!({
            "type": "question_response",
            "id": id,
            "cancelled": true,
        })
        .to_string();
        if let Some(tx) = &self.ws_tx {
            let _ = tx.send(msg);
        }
    }

    /// A `question` payload arrived on the websocket. Surface it in this
    /// pane only when it belongs to the topic open here; a question for
    /// another topic is left for a pane on that topic (the server times
    /// out unanswered questions).
    fn handle_question_event(&mut self, parsed: &serde_json::Value) {
        let topic = parsed
            .get("topic")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if self.topic.as_deref() != Some(topic) {
            return;
        }
        let Some(id) = parsed.get("id").and_then(|v| v.as_str()) else {
            return;
        };
        let options: Vec<String> = parsed
            .get("options")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| o.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if options.is_empty() {
            return;
        }
        // Queued rather than replacing: one `ask_user` call can ask several
        // questions at once, and cancelling the one already on screen would
        // tell the tool the user backed out of it. The user steps through the
        // batch with ←/→ and the whole set goes out on the last Enter.
        self.questions.push(PendingQuestion {
            id: id.to_string(),
            topic: topic.to_string(),
            question: parsed
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            options,
            multi: parsed
                .get("allow_multiple")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            selected: 0,
            marked: Vec::new(),
        });
    }

    pub(super) fn handle_ws_message(&mut self, text: &str) {
        let parsed: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };

        // The new TopicProxyHandler / ScopedWsHandler publish these events
        // on the inspect-broadcast bus. Forward them to the live buffers
        // (activity pane, chat progress, chat message stream).
        let event_type = parsed.get("type").and_then(|v| v.as_str());
        match event_type {
            Some("activity") | Some("chat_message") | Some("thinking") | Some("processing")
            | Some("resync") | Some("loop_tick") => {
                self.handle_live_event(&parsed);
            }
            Some("question") => {
                self.handle_question_event(&parsed);
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
        // no longer needed (REST `get_topic_chat`).
    }

    /// Recall an older entry from input history into the editor.
    /// Bounded by the oldest (first) entry.
    pub(super) fn recall_older(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let pos = self.history_pos.unwrap_or(self.input_history.len());
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
    /// (initial fetch on topic selection). Sets `last_seen_id` to the
    /// highest id seen so duplicate WS events are dropped.
    #[allow(dead_code)]
    pub(super) fn seed_live(
        &mut self,
        channel: &str,
        topic: &str,
        activity: Vec<jyc_types::ActivityEntry>,
        chat: Vec<jyc_types::ChatMessageEntry>,
    ) {
        let key = (channel.to_string(), topic.to_string());
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
        self.last_seen_id.insert(key.clone(), max_id);
        // Reset the egress tracker for the freshly-seeded topic. `messages`
        // was cleared by `open()` / `open_pattern_select()` /
        // `select_pattern_inner()` and the freshly-hydrated `live_chat` must
        // be re-pushed in full — leaving the previous visit's max id here
        // would skip every hydrated historical row whose id ≤ old max.
        self.last_pushed_chat_id.insert(key, 0);
    }

    /// Handle a `{"type":"resync", "channel":..., "topic":...}` event by
    /// clearing the live buffers for that topic. The caller should re-run
    /// the REST hydrate (`get_topic_activity` + `get_topic_chat`) and
    /// re-seed via `seed_live`.
    #[allow(dead_code)]
    pub(super) fn clear_live(&mut self, channel: &str, topic: &str) {
        let key = (channel.to_string(), topic.to_string());
        self.live_activity.remove(&key);
        self.live_chat.remove(&key);
        self.live_thinking.remove(&key);
        self.live_processing.remove(&key);
        self.last_seen_id.remove(&key);
        self.last_pushed_chat_id.remove(&key);
    }

    /// Handle a parsed `{"type":"activity",...}` or similar WS payload.
    /// Filters out duplicate / older events using `last_seen_id`.
    #[allow(dead_code)]
    pub(super) fn handle_live_event(&mut self, payload: &serde_json::Value) {
        let channel = match payload.get("channel").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => return,
        };
        let topic = match payload.get("topic").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => return,
        };
        let key = (channel.clone(), topic.clone());
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
                    // The agent publishes the cumulative reasoning content of the
                    // current LLM request, so an event extending the last block is
                    // a snapshot update, not a new block.
                    let blocks = self.live_thinking.entry(key).or_default();
                    match blocks.last_mut() {
                        Some(last) if text.starts_with(last.as_str()) => *last = text.to_string(),
                        _ => blocks.push(text.to_string()),
                    }
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
                    // Processing completed — fold the round's accumulated
                    // thinking blocks into a collapsed pseudo-message above
                    // the AI reply (in-memory only; gone on TUI restart),
                    // then clear the live buffer. Only when this topic is
                    // the one open in the chat pane.
                    if let Some(blocks) = self.live_thinking.remove(&key)
                        && !blocks.is_empty()
                        && self.channel.as_deref() == Some(key.0.as_str())
                        && self.topic.as_deref() == Some(key.1.as_str())
                    {
                        self.messages.push(ChatMessage {
                            sender: "thinking".to_string(),
                            text: blocks.join("\n\n"),
                            timestamp: Some(chrono::Utc::now().to_rfc3339()),
                        });
                    }
                    // Processing completed - the fold above already removed
                    // the thinking entry; clear the other per-round transient
                    // artifacts but keep live_activity as the audit trail
                    // across rounds. Buffer is bounded at 180 entries.
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
                self.last_pushed_chat_id.remove(&key);
            }
            _ => {}
        }
    }

    /// Clear the transient per-topic live state (`live_thinking` and
    /// `live_processing`) without touching the activity/chat buffers.
    ///
    /// Called on REST hydrate when switching topics: `live_processing` is
    /// only updated by WS `processing` events received while the topic is
    /// watched, so entries go stale while unwatched (a missed completion
    /// leaves `true` → phantom progress; a missed start leaves `false` →
    /// no progress). Clearing makes the renderer fall back to the polled
    /// overview status until fresh WS events arrive.
    pub(super) fn clear_live_transient(&mut self, channel: &str, topic: &str) {
        let key = (channel.to_string(), topic.to_string());
        self.live_thinking.remove(&key);
        self.live_processing.remove(&key);
        self.live_tick_ms.remove(&key);
    }

    /// Get a snapshot of the live activity buffer for the given (channel, topic).
    /// Returns an empty slice if no live data has been seeded yet.
    #[allow(dead_code)]
    pub(super) fn live_activity_for(
        &self,
        channel: &str,
        topic: &str,
    ) -> std::collections::vec_deque::Iter<'_, jyc_types::ActivityEntry> {
        self.live_activity
            .get(&(channel.to_string(), topic.to_string()))
            .map(|v| v.iter())
            .unwrap_or_else(|| EMPTY_VEC_DEQUE.iter())
    }

    /// Get the current turn's thinking blocks for the given
    /// (channel, topic), if any (arrival order, full text).
    pub(super) fn live_thinking_for(&self, channel: &str, topic: &str) -> Option<&[String]> {
        self.live_thinking
            .get(&(channel.to_string(), topic.to_string()))
            .map(|v| v.as_slice())
    }

    /// Get the current processing status for the given (channel, topic).
    /// Returns `None` if no status has been received yet (fall back to polled state).
    pub(super) fn live_processing_for(&self, channel: &str, topic: &str) -> Option<(bool, bool)> {
        self.live_processing
            .get(&(channel.to_string(), topic.to_string()))
            .copied()
    }
    /// Get the live wall-clock elapsed time (milliseconds) for an active
    /// agent loop on the given (channel, topic). Returns `None` when no
    /// tick has arrived yet (loop just started) or the loop has ended.
    /// Used by all three render sites: the dashboard Details panel, the
    /// chat-mode info pane, and the chat progress line.
    pub(super) fn live_tick_ms_for(&self, channel: &str, topic: &str) -> Option<u64> {
        self.live_tick_ms
            .get(&(channel.to_string(), topic.to_string()))
            .copied()
    }
    /// Iterate over the live chat messages for the given (channel, topic).
    /// Used by the dashboard's poll loop to append new messages to the
    /// `chat.messages` vec shown in the chat pane.
    #[allow(dead_code)]
    pub(super) fn live_chat_for(
        &self,
        channel: &str,
        topic: &str,
    ) -> std::collections::vec_deque::Iter<'_, jyc_types::ChatMessageEntry> {
        self.live_chat
            .get(&(channel.to_string(), topic.to_string()))
            .map(|v| v.iter())
            .unwrap_or_else(|| EMPTY_CHAT_DEQUE.iter())
    }

    /// Append new chat messages from the live buffer to the rendered
    /// message list. Returns `true` if at least one row was pushed.
    ///
    /// Three dedup rules apply:
    /// - **Live entries** (`id != 0`): skipped if already pushed in an
    ///   earlier poll (`id <= last_pushed_chat_id`). This is what makes
    ///   repeated `/`-commands (`/context`, `/exchange`, `/help`,
    ///   `/model <x>`) emit byte-identical AI text every run but still
    ///   show up — each event has a fresh monotonic per-topic id.
    /// - **User echoes** (`sender == "user"`): dedup by `(sender, text)`
    ///   because the local echo pushed by `send_message_inner` shares
    ///   `(sender, text)` with the server's IncomingMessage echo (which
    ///   has `id > 0`).
    /// - **Historical rows** (`id == 0` from `chat_log_store.rs` JSONL
    ///   hydrate): dedup by `(sender, text)`. All historical rows share
    ///   `id = 0` so the id-tracker cannot distinguish them; without
    ///   this fallback the 500 ms poll loop would re-push the same row
    ///   every cycle and flood `self.messages`.
    pub(super) fn poll_sync_live_chat(&mut self, channel: &str, topic: &str) -> bool {
        // Collect into a Vec first to release the immutable borrow on
        // self.live_chat before mutating self.messages and last_pushed_chat_id.
        let live_msgs: Vec<jyc_types::ChatMessageEntry> =
            self.live_chat_for(channel, topic).cloned().collect();
        let push_key = (channel.to_string(), topic.to_string());
        let last_pushed = self
            .last_pushed_chat_id
            .get(&push_key)
            .copied()
            .unwrap_or(0);
        let mut new_msg = false;
        let mut max_pushed = last_pushed;
        for msg in &live_msgs {
            if msg.id != 0 && msg.id <= last_pushed {
                continue;
            }
            if msg.sender == "user"
                && self
                    .messages
                    .iter()
                    .any(|m| m.sender == "user" && m.text == msg.text)
            {
                continue;
            }
            if msg.id == 0
                && self
                    .messages
                    .iter()
                    .any(|m| m.sender == msg.sender && m.text == msg.text)
            {
                continue;
            }
            self.messages.push(ChatMessage {
                sender: msg.sender.clone(),
                text: msg.text.clone(),
                timestamp: msg.timestamp.clone(),
            });
            new_msg = true;
            if msg.id > max_pushed {
                max_pushed = msg.id;
            }
        }
        if max_pushed > last_pushed {
            self.last_pushed_chat_id.insert(push_key, max_pushed);
        }
        new_msg
    }
}

/// Static empty deque used as a fallback when no live data is seeded for a
/// (channel, topic) — lets us return a concrete `Iter` from the accessors.
static EMPTY_VEC_DEQUE: std::sync::LazyLock<std::collections::VecDeque<jyc_types::ActivityEntry>> =
    std::sync::LazyLock::new(std::collections::VecDeque::new);
static EMPTY_CHAT_DEQUE: std::sync::LazyLock<
    std::collections::VecDeque<jyc_types::ChatMessageEntry>,
> = std::sync::LazyLock::new(std::collections::VecDeque::new);

#[cfg(test)]
mod tests;
