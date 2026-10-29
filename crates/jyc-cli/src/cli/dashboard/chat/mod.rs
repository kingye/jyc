//! Chat pane: state, key handling, and rendering for the dashboard's
//! WebSocket topic chat (all channel types via `/ws/<channel>/<topic>`).

use super::token_render::{
    CONTEXT_COMPOSITION_LABEL, LABEL_WIDTH, PaneLayout, bar_width, context_pct,
    context_region_label_span, label_span, layout_for, push_cache_creation_span,
    push_context_composition_bar_spans, push_cost_span, push_latency_span, push_output_span,
    push_reasoning_span, push_session_cache_span, push_status_span, push_today_all_span,
    push_tokens_span, push_turn_cache_span, value_width,
};
use super::*;
use jyc_core::duration::{DurationStyle, format_duration_ms, format_duration_secs};
use jyc_core::topic_manager::topic_tree::{Lineage, node_count, topic_forest};

pub(super) mod clipboard;
mod render;
mod table_wrap;

use render::{
    RenderFingerprint, entry_activity_text, entry_severity, render_chat_conversation,
    tail_to_width, truncate_to_width,
};

/// Inactive color for the input area (header line and bottom rule) when the
/// chat pane loses focus.
const LINE_DRAWING: Style = Style::new().fg(Color::Rgb(0x39, 0x35, 0x52));

/// Rows the status line takes off the bottom of the chat pane. Both sides
/// need this number: the renderer reserves it as its last chunk, and
/// `page_size` subtracts it from the scrollable message rows.
pub(super) const STATUS_LINE_ROWS: u16 = 1;

/// The overview's list as the tree's input: the same topics, in the shape
/// the tree walks.
///
/// The list already carries each topic's parent — the topic manager resolved
/// it — so this is a shape change, not a second resolution.
///
/// The price is a `Vec` and three cloned strings per topic, and the
/// explorer pays it once per frame while it is open. That is the same order
/// as the forest the frame builds anyway; if it ever shows up in a profile,
/// build the `Vec` once per state update and pass it down.
fn lineage_of(topics: &[jyc_types::TopicSummary]) -> Vec<Lineage> {
    topics.iter().map(Lineage::from).collect()
}

/// An `ask_user` question pushed by the daemon, awaiting the user's answer.
pub(super) struct PendingQuestion {
    /// Topic the question belongs to (only surfaced in that topic's pane).
    pub topic: String,
    /// What the question is about: the assistant's own text for the turn that
    /// asked (the daemon's `context`). Shown above the question — with only
    /// the question on screen the user has to guess which answer is meant.
    pub context: Option<String>,
    /// Question text.
    pub question: String,
    /// Selectable options.
    pub options: Vec<String>,
    /// Whether more than one option may be picked (the daemon's
    /// `allow_multiple`).
    pub multi: bool,
    /// The user will not answer this one. Reported as `(skipped)` in the answer,
    /// so a question that cannot be answered does not cost the whole batch — `d`
    /// toggles it, and the option list is replaced by the skipped state.
    pub skipped: bool,
    /// The user's own answer, typed on the list's `✎` row instead of picking an
    /// option. `Some` once one is settled, and it then answers for this question
    /// on its own line (`Q1: their words`) — a pick and a typed answer replace
    /// each other. An empty draft the user accepted is reported as `(skipped)`,
    /// the same as `d`. Nothing here leaves before the batch is settled.
    pub typed: Option<String>,
    /// Currently highlighted option - the cursor `Space` marks under.
    pub selected: usize,
    /// Marked option indices, in multi mode; single mode answers with the
    /// cursor directly.
    pub marked: Vec<usize>,
}

/// A single message in the chat conversation.
#[derive(Debug, Clone)]
pub(super) struct ChatMessage {
    pub(super) sender: String,
    pub(super) text: String,
    pub(super) timestamp: Option<String>,
    /// The topic's chat sequence number (`0` for a local echo the server has
    /// not confirmed yet, and for rows logged before the sequence existed).
    pub(super) id: u64,
    /// Identifies this client's optimistic echo of a message it sent, so the
    /// server's row replaces it instead of being appended as a second copy.
    pub(super) client_id: Option<String>,
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

/// The chat pane's WebSocket state.
///
/// Three states, not a bool: "not talking to the hub yet" and "the link is
/// broken" are the same `false` for a bool, and the pane reports them
/// differently — one is a startup state, the other is a fault worth showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WsState {
    /// No attempt has failed yet — the client is connecting (or has not
    /// started).
    Connecting,
    /// The socket is live.
    Connected,
    /// The socket is down: it was never established, or it was lost. Either
    /// way the client is retrying, and the pane says so.
    Disconnected,
}

/// Chat pane state: WebSocket topic chat for any channel type.
pub(super) struct ChatState {
    // Chat pane state
    pub(super) visible: bool,
    pub(super) topic: Option<String>,
    pub(super) channel: Option<String>,
    pub(super) messages: Vec<ChatMessage>,
    /// Multi-line text editor for the chat input (ratatui-textarea).
    pub(super) editor: TextArea<'static>,
    pub(super) focus: Focus,
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
    /// `focus == Focus::MessageArea`. Every entry into the pane re-arms the
    /// sentinel (`focus_message_area`), so the cursor starts from the last line
    /// each time the pane gains focus.
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
    /// keystroke / 50ms poll / 1Hz tick used to cost O(history)). The third
    /// element is the transcript line of every user turn's opening rule —
    /// the `[`/`]` section-jump boundaries.
    pub(super) render_cache: Option<(RenderFingerprint, Vec<Line<'static>>, Vec<usize>)>,
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
    /// Chat status block visibility — the version/stats (or transient
    /// message) lines at the top of the topic info pane. Default
    /// visible; toggled via the leader-key popup (`s`).
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
    /// The chat socket's state — see [`WsState`].
    pub(super) ws_state: WsState,
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
    /// Minimal progress mode: the live tail collapses to a single animated
    /// line (`⠹ 12.4s · thinking... | bash`) and the completed-turn thinking
    /// line is left out of the history. Toggled via the leader popup (`p`);
    /// default on.
    pub(super) minimal_progress: bool,
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
    pub(super) last_seen_activity_id: std::collections::BTreeMap<(String, String), u64>,
    /// Highest chat sequence number seen on the WS stream (and in a hydrate)
    /// for each (channel, topic) — used to drop duplicate WS events after
    /// reconnect / `resync`. The pane's own dedup does not use it: see
    /// `sync_live_chat`.
    pub(super) last_seen_chat_seq: std::collections::BTreeMap<(String, String), u64>,
    /// Counter behind this client's own message ids (see `ChatMessage::client_id`).
    pub(super) next_client_msg_seq: u64,
    /// Last (channel, topic) that was REST-hydrated by the poll loop.
    /// Used to avoid re-hydrating the same topic on every poll when the
    /// user is browsing the overview.
    pub(super) last_hydrated_key: Option<(String, String)>,
    /// Address stash so the explorer pane can switch topics later.
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
    /// One `ask_user` call can ask several at once, so they are answered as a
    /// form: `question_index` is the one on screen, each carries its own cursor
    /// and marks, and nothing is sent until the last one is confirmed — which
    /// is what lets `←/→` go back and adjust an earlier answer. Takes over the
    /// input area while non-empty; on the last Enter the whole set goes back as
    /// one ordinary chat message. The turn that asked them is already over, so
    /// Esc only closes the box.
    pub(super) questions: Vec<PendingQuestion>,
    /// Which of [`Self::questions`] is on screen.
    pub(super) question_index: usize,
    /// Whether the current question is taking a typed answer: the keys go to
    /// its draft instead of the list, so `j`/`k`, the digits and `d` cannot fire
    /// on letters the user means as text. Cleared by Enter, Esc, stepping to
    /// another question and closing the box.
    pub(super) question_typing: bool,
    /// Current position in history browsing (None = not browsing).
    pub(super) history_pos: Option<usize>,
    /// Authorization token to attach to WebSocket upgrade requests.
    pub(super) token: Option<String>,
    /// `(from, to)` set by `jyc connect` when the tunnel's local port differs
    /// from the remote hub's: the hub's publish links carry its own loopback
    /// URL, which does not resolve here. Applied to message text on the way
    /// in, so the pane shows (and copies) links that open.
    pub(super) link_rewrite: Option<(String, String)>,
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

/// Markdown render options for chat messages.
///
/// The defaults are all there is: tui-markdown is built without its
/// `highlight-code` feature, so `code_theme` does not exist and fenced code
/// renders as plain text in the default stylesheet (see jyc-cli's Cargo.toml).
pub(super) fn chat_markdown_options() -> tui_markdown::Options {
    tui_markdown::Options::default()
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

/// Replace leading spaces of each non-fence line with U+00A0 so the markdown
/// renderer (CommonMark rewrites indentation: up to 3 spaces are stripped, 4+
/// become a code block) preserves the user's visual indentation at any depth.
/// Fence contents keep real spaces so copied code is byte-clean.
pub(super) fn protect_leading_whitespace(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut in_fence = false;
    for line in md.split_inclusive('\n') {
        let stripped = line.trim_start();
        if stripped.starts_with("```") || stripped.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence || line.trim().is_empty() {
            out.push_str(line);
            continue;
        }
        let n = line.len() - line.trim_start_matches(' ').len();
        out.push_str(&"\u{00A0}".repeat(n));
        out.push_str(&line[n..]);
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
    app.chat.focus = Focus::ChatPane;
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

/// `[` / `]` — previous / next user turn — with the cursor showing.
fn section_cursor(app: &mut App, dir: i32, extend: bool) {
    let rows = app.chat.cursor_section_jump(dir, extend);
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
    let Some((_, lines, _)) = app.chat.render_cache.as_ref() else {
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
    // The explorer's rows are the lineage forest flattened, and the forest
    // holds every topic exactly once — so the row count is the topic count,
    // and the forest does not have to be built to know it.
    let len = app.state.as_ref().map_or(0, |s| s.topics.len());
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
        let forest = topic_forest(&lineage_of(&s.topics));
        let rows = explorer_rows(&forest);
        let node = rows.get(app.chat.explorer_selected)?.node;
        Some((node.name.clone(), node.channel.clone()))
    });
    let Some((name, channel)) = info else {
        return;
    };
    match app.chat.open_addr.clone() {
        Some(addr) => {
            let token = app.chat.token.clone();
            app.chat.open(&addr, Some(&channel), &name, token);
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
        LocalAction::OpenDashboard => {
            app.chat.close();
            // The overview's own panes are the ones on screen again; its
            // table takes the focus back (the chat screen's cycle never
            // reaches it, so nothing else would).
            app.chat.focus = Focus::TopicsTable;
        }
        // Dashboard-scoped; never offered on the chat screen.
        LocalAction::OpenChat => {}
        LocalAction::ReloadConfig => app.pending_reload_config = true,
        LocalAction::Quit => app.should_quit = true,
        LocalAction::ToggleExplorer => toggle_explorer_snapped(app),
        LocalAction::ToggleZen => app.chat.toggle_zen_mode(),
        LocalAction::ToggleActivity => app.chat.toggle_activity(),
        LocalAction::ToggleStatus => app.chat.toggle_status_bar(),
        LocalAction::ToggleInfo => app.chat.toggle_info_pane(),
        LocalAction::OpenExternalEditor => {
            if app.chat.focus == Focus::ChatPane
                && let Err(e) = edit_input_externally(app, terminal)
            {
                app.set_status(format!("Editor error: {e:#}"));
            }
        }
        LocalAction::FocusChat => app.chat.focus_message_area(),
        LocalAction::ScrollTop => app.chat.scroll_to_top(),
        LocalAction::ScrollBottom => app.chat.scroll_to_bottom(),
        LocalAction::ToggleMouseCapture => super::toggle_mouse_capture(app),
        LocalAction::ClearInput => {
            app.chat.editor = empty_chat_editor();
            // The popup has no field of its own — it filters off this one,
            // so clearing the field closes it (an empty field has nothing
            // to filter, same as deleting the `/` by hand).
            sync_command_popup(app);
        }
        LocalAction::ToggleThinking => {
            app.chat.thinking_expanded = !app.chat.thinking_expanded;
        }
        LocalAction::ToggleToolDetail => {
            app.chat.tool_detail_expanded = !app.chat.tool_detail_expanded;
        }
        LocalAction::ToggleMinimalProgress => {
            app.chat.minimal_progress = !app.chat.minimal_progress;
        }
        LocalAction::ToggleMode => toggle_topic_mode(app),
    }
}

/// Send `/plan` or `/build` for the topic open in the chat pane — the same
/// route `ctrl+c` takes with `/cancel`.
///
/// The mode is read through the lookup the header and the topic info pane
/// display ([`selected_topic_detail`]), so what the pane shows and what the
/// shortcut does cannot disagree. A topic whose mode the server has not
/// reported yet counts as build, matching the server's default.
fn toggle_topic_mode(app: &mut App) {
    let in_plan = selected_topic_detail(app)
        .and_then(|t| t.mode.as_deref())
        .is_some_and(|m| m.eq_ignore_ascii_case("plan"));
    app.chat
        .send_message_inner(if in_plan { "/build" } else { "/plan" }.to_string());
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
        let forest = topic_forest(&lineage_of(&s.topics));
        row_of(&explorer_rows(&forest), topic, channel)
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
    // per-topic CancellationToken. Restrict to an open chat — there is
    // no topic to cancel otherwise.
    let is_ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_c && app.chat.visible {
        // Close any open command popup so the cancel path runs cleanly.
        app.chat.command_popup = None;
        app.chat.leader = None;
        app.chat.close_questions();
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

    // Ctrl+P is the leader (works on the chat screen; `open dashboard`
    // inside it is the way back).
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
    // A pending question box owns the keyboard (see below), so the shortcut
    // stays out of its way: with a box up, `/` would go to the box's draft or
    // be swallowed, and a popup opened behind it would take the next Enter.
    let is_slash = key.code == KeyCode::Char('/')
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && !app.chat.active_question();
    if is_slash && app.chat.focus == Focus::ChatPane && app.chat.text().trim().is_empty() {
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

    // "[" on an empty input jumps to the previous user turn: `focus_message_area`
    // resets the cursor past the end of the transcript, so one jump up is the
    // newest turn. The bracket is not typed. Out of the way of a pending
    // question for the same reason as `/` above.
    let is_bracket = key.code == KeyCode::Char('[')
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && !app.chat.active_question();
    if is_bracket && app.chat.focus == Focus::ChatPane && app.chat.text().trim().is_empty() {
        app.chat.focus_message_area();
        section_cursor(app, -1, false);
        return;
    }

    // Pending questions own the keyboard while visible: the box is the answer
    // surface for a turn that already ended. Esc only closes it — the user can
    // still type an answer (or anything else) afterwards.
    if app.chat.active_question() {
        // A draft is being typed on the current question's `✎` row: the keys
        // belong to it, so the list's shortcuts (`j`/`k`, the digits, `d`) cannot
        // fire on letters the user means as text. Enter keeps the answer, Esc
        // goes back to the options.
        if app.chat.question_typing {
            match key.code {
                KeyCode::Enter => app.chat.confirm_question(),
                KeyCode::Esc => app.chat.cancel_typed_answer(),
                KeyCode::Backspace => app.chat.pop_typed_char(),
                KeyCode::Char(c) => app.chat.push_typed_char(c),
                _ => {}
            }
            return;
        }
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
            KeyCode::Char('d') | KeyCode::Backspace => app.chat.toggle_question_skip(),
            KeyCode::Esc => app.chat.close_questions(),
            _ => {}
        }
        return;
    }

    {
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
                if app.chat.focus == Focus::ExplorerPane {
                    explorer_move(app, -10);
                } else {
                    app.chat.page_up();
                }
                return;
            }
            KeyCode::PageDown => {
                if app.chat.focus == Focus::ExplorerPane {
                    explorer_move(app, 10);
                } else {
                    app.chat.page_down();
                }
                return;
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if app.chat.focus == Focus::ExplorerPane {
                    explorer_move(app, -10);
                } else {
                    app.chat.page_up();
                }
                return;
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if app.chat.focus == Focus::ExplorerPane {
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
        if app.chat.focus == Focus::ExplorerPane {
            match key.code {
                KeyCode::Esc => {
                    app.chat.focus = Focus::ChatPane;
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

        if app.chat.focus == Focus::InfoPane {
            // Vertical scroll only — file paths are short enough
            // that horizontal overflow isn't a concern. No Esc-back:
            // leaving the info pane is via Tab (focus cycle) or
            // the leader-key popup, same as ActivityPane. Any other
            // key refocuses the input (consumed, not forwarded), so
            // the user can scroll then just start typing.
            if key.code != KeyCode::Esc && !app.chat.scroll_pane_key(key, gg_jump) {
                refocus_input(app);
            }
            return;
        }

        if app.chat.focus == Focus::ActivityPane {
            // No Esc-back here: returning to the dashboard is done
            // via the leader-key popup (`open dashboard`, Ctrl+P).
            // Any other key refocuses the input, consumed (same
            // as MessageArea).
            if key.code != KeyCode::Esc && !app.chat.scroll_pane_key(key, gg_jump) {
                refocus_input(app);
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
        if app.chat.focus == Focus::MessageArea {
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
                // `[` / `]` step between user turns; the shifted forms grow a
                // selection (that is how a terminal reports Shift+[ / Shift+]).
                KeyCode::Char('[') => section_cursor(app, -1, false),
                KeyCode::Char(']') => section_cursor(app, 1, false),
                KeyCode::Char('{') => section_cursor(app, -1, true),
                KeyCode::Char('}') => section_cursor(app, 1, true),
                // With rows selected, one `y` copies them. Without a
                // selection `y` is the first half of a yank: arming it lets
                // a count sit between the halves, so `yy`, `y3y` and `3yy`
                // all mean the same thing.
                KeyCode::Char('y') if app.chat.selection_anchor.is_some() => yank_selection(app),
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
            KeyCode::Up if app.chat.text().trim().is_empty() || app.chat.history_pos.is_some() => {
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
    if !app.chat.visible {
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
    app.chat.focus_message_area();
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
    //   ├────────── bottom area: activity pane ─────────────────────────┤
    //   │  activity pane (bottom 20% / 80% / full when visible)         │
    //   └───────────────────────────────────────────────────────────────┘
    //
    // No bottom status bar on the chat screen: status info (version,
    // stats, transient messages) renders in a sub-pane pinned to the
    // bottom of the topic info pane instead (leader `s` toggles it). The
    // sub-pane never scrolls. Zen mode hides the info pane, which hides
    // the status info with it.

    let show_activity = app.chat.activity_split != 0;
    let show_explorer = app.chat.explorer_visible;
    if show_explorer {
        sync_explorer_selection(app);
    }

    // Main area: horizontal [explorer?, right column]. The right column
    // holds chat, info, and activity.
    let (explorer_area, right_area) = if show_explorer {
        let h = Layout::horizontal([Constraint::Percentage(20), Constraint::Percentage(80)])
            .split(area);
        (Some(h[0]), h[1])
    } else {
        (None, area)
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
}

/// Keep the explorer selection valid and, while the explorer is not
/// focused, following the topic currently open in the chat pane.
fn sync_explorer_selection(app: &mut App) {
    let Some(s) = app.state.as_ref() else {
        app.chat.explorer_selected = 0;
        return;
    };
    let forest = topic_forest(&lineage_of(&s.topics));
    let rows = explorer_rows(&forest);
    let len = rows.len();
    if len == 0 {
        app.chat.explorer_selected = 0;
        return;
    }
    if app.chat.explorer_selected >= len {
        app.chat.explorer_selected = len - 1;
    }
    if app.chat.focus != Focus::ExplorerPane
        && let (Some(topic), Some(channel)) = (&app.chat.topic, &app.chat.channel)
        && let Some(idx) = row_of(&rows, topic, channel)
    {
        app.chat.explorer_selected = idx;
    }
}

/// The status dot colour used by the explorer and the info pane's subagent
/// tree: green = processing, yellow = queued, red = error, grey = idle.
fn status_dot_style(status: &TopicStatus) -> Style {
    match status {
        TopicStatus::Processing => Style::default().fg(Color::Green),
        TopicStatus::Queued => Style::default().fg(Color::Yellow),
        TopicStatus::Idle => Style::default().fg(Color::DarkGray),
        TopicStatus::Error => Style::default().fg(Color::Red),
    }
}

/// A row of a tree view: the node it shows plus the box-drawing prefix
/// that puts it at its depth (`""` for a root).
struct TreeRow<'a> {
    node: &'a jyc_types::TopicNode,
    prefix: String,
}

/// The most columns of indent a level hands down to the next. A row is drawn
/// as that prefix plus its branch mark, so this is what caps how far a row
/// starts: the explorer pane is 20% wide — about fifteen columns — and a level
/// per three columns would leave a deep name nothing to print in. A level that
/// would add more freezes at `…  `, the same width as the one before it, so a
/// child never starts further left than its parent.
const MAX_PREFIX_COLS: usize = 3;

/// Append `nodes`' rows to `rows`, depth-first, each indented under its
/// parent — up to [`MAX_PREFIX_COLS`] columns handed down, past which the
/// indent freezes so the names stay readable.
///
/// The nesting is the core's: these rows draw a tree the lineage rules
/// already built, so the explorer's tree, the info pane's tree and a topic's
/// own `subagents` are the same shape.
fn push_tree_rows<'a>(
    rows: &mut Vec<TreeRow<'a>>,
    nodes: &'a [jyc_types::TopicNode],
    child_prefix: &str,
) {
    for (n, node) in nodes.iter().enumerate() {
        let last = n + 1 == nodes.len();
        let mut prefix = String::with_capacity(child_prefix.len() + 3);
        prefix.push_str(child_prefix);
        prefix.push_str(if last { "└─ " } else { "├─ " });
        rows.push(TreeRow { node, prefix });
        let mut next = String::with_capacity(child_prefix.len() + 3);
        next.push_str(child_prefix);
        next.push_str(if last { "   " } else { "│  " });
        let next = if next.chars().count() > MAX_PREFIX_COLS {
            "…  "
        } else {
            next.as_str()
        };
        push_tree_rows(rows, &node.children, next);
    }
}

/// The explorer's rows: every root, each followed by its subtree.
fn explorer_rows(forest: &[jyc_types::TopicNode]) -> Vec<TreeRow<'_>> {
    let mut rows = Vec::with_capacity(forest.len());
    for node in forest {
        rows.push(TreeRow {
            node,
            prefix: String::new(),
        });
        push_tree_rows(&mut rows, &node.children, "");
    }
    rows
}

/// The row showing `(name, channel)`, if the tree holds it.
fn row_of(rows: &[TreeRow], name: &str, channel: &str) -> Option<usize> {
    rows.iter()
        .position(|r| r.node.name == name && r.node.channel == channel)
}

/// Render the left-side topic explorer pane (20% wide when shown).
///
/// Lists all topics from the latest overview poll as a tree — a subagent is
/// indented under the topic that spawned it — with a status dot
/// (green = processing, yellow = queued, red = error, grey = idle),
/// highlighting the topic currently open in the chat pane. The cursor row is
/// marked whether or not this pane has focus, so switching focus back to the
/// input never hides which topic the arrow keys would move. The list is
/// rebuilt from `app.state` on every render, so it stays live.
pub(super) fn render_explorer(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.chat.focus == Focus::ExplorerPane;
    let border_style = pane_border_style(focused);
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

    // Tree rows: a subagent sits under the topic that spawned it.
    // Scroll window: keep the selected row visible.
    let forest = topic_forest(&lineage_of(&s.topics));
    let rows = explorer_rows(&forest);
    let height = inner.height as usize;
    let offset = window_offset(rows.len(), selected, height);

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, row)| {
            let t = row.node;
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
                Span::styled(row.prefix.as_str(), sel),
                Span::styled("● ", status_dot_style(&t.status)),
                Span::styled(t.name.as_str(), name_style.patch(sel)),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

/// The topic's detail, when it is the one on screen.
///
/// The detail comes from `GET /api/topics/{channel}/{topic}/info` — the
/// same source `/info` reads — so the pane and the command report the same
/// figures. The screen rule itself — the visible chat's own topic, else the
/// row under the dashboard cursor — lives in [`super::shown_topic_key`],
/// shared with the detail fetch so the two cannot drift apart. A detail for
/// another topic (the fetch for the current one has not landed yet) is not
/// the topic on screen, so it is not shown.
fn selected_topic_detail(app: &App) -> Option<&jyc_types::TopicInfo> {
    let (channel, name) = super::shown_topic_key(app)?;
    let detail = app.topic_detail.as_ref()?;
    (detail.channel == channel && detail.name == name).then_some(detail)
}

/// Whether the last detail fetch for the topic on screen failed.
///
/// A failed fetch clears the detail, so the pane cannot tell "not loaded
/// yet" from "the request failed" by looking at the detail alone — and a
/// dead tunnel looks exactly like a request that never returns. The failure
/// is keyed by topic, so a stale failure never shows up under a topic that
/// is loading fine.
fn detail_fetch_failed(app: &App) -> bool {
    let Some((channel, name)) = super::shown_topic_key(app) else {
        return false;
    };
    app.topic_detail_failed.as_ref() == Some(&(channel, name))
}

/// One row of the info pane: a `label: value` field, a value that belongs
/// to the row above it, or a line of a section that keeps its own shape
/// (the task list, the changed files).
enum InfoRow<'a> {
    /// A `label: value` row: the first span is the label, the rest is
    /// the value.
    Field(Vec<Span<'a>>),
    /// A value that belongs to the row above it — the figures of a bar
    /// row, which have a row of their own under the bar, in the column
    /// the bar starts in.
    Value(Span<'a>),
    /// A line that is not a `label: value` row and keeps its own shape.
    Raw(Line<'a>),
}

/// Lay the info pane's rows out. A field is laid out by the pane's
/// layout; a value that belongs to the row above it joins that row's
/// column; a section line keeps its own shape.
///
/// In the table layout the label and the value share one row: the label is
/// padded, so every value of the pane starts in the same column and the
/// pane reads as one table. A value too long for its column wraps inside
/// it, and the lines that continue it are indented to the column, so the
/// column keeps the values it holds.
///
/// In the stacked layout the label takes a row of its own and the value
/// follows flush left on the next one — a pane too narrow for a value
/// column would otherwise cut every value short.
///
/// The table layout assumes an inner width of at least the label's width:
/// the value column is what is left of the pane's width, and a pane
/// narrower than its label has no value column to lay the values out in.
/// The pane picks that layout only when the column can hold a label and a
/// value (see [`PaneLayout`]), so every call satisfies it.
fn layout_info_rows(rows: Vec<InfoRow>, layout: PaneLayout, inner_width: u16) -> Vec<Line> {
    use unicode_width::UnicodeWidthStr;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut spans = match row {
            InfoRow::Raw(line) => {
                out.push(line);
                continue;
            }
            InfoRow::Value(value) => {
                // A value that belongs to the row above it — the figures
                // of a bar row. They sit in the pane's value column, under
                // the bar they belong to, so the two line up.
                match layout {
                    PaneLayout::Table => {
                        let value_col = (inner_width as usize).saturating_sub(LABEL_WIDTH).max(1);
                        let indent = " ".repeat(LABEL_WIDTH);
                        for chunk in wrap_styled_lines(vec![Line::from(value)], value_col) {
                            let mut line = Vec::with_capacity(chunk.spans.len() + 1);
                            line.push(Span::raw(indent.clone()));
                            line.extend(chunk.spans);
                            out.push(Line::from(line));
                        }
                    }
                    PaneLayout::Stacked => {
                        out.push(Line::from(value));
                    }
                }
                continue;
            }
            InfoRow::Field(spans) => spans,
        };
        // A row without a label has no value to lay out beside it.
        if spans.is_empty() {
            continue;
        }
        let label = spans.remove(0);
        match layout {
            PaneLayout::Table => {
                // The value wraps inside the pane's value column, and every
                // line that continues it is indented to that column, so a
                // value too long for the column keeps the pane's shape.
                let label_width = label.content.width();
                let value_col = (inner_width as usize).saturating_sub(label_width).max(1);
                let mut chunks = wrap_styled_lines(vec![Line::from(spans)], value_col).into_iter();
                let first = chunks.next().unwrap_or_default();
                let mut line = Vec::with_capacity(first.spans.len() + 1);
                line.push(label);
                line.extend(first.spans);
                out.push(Line::from(line));
                for chunk in chunks {
                    let mut line = Vec::with_capacity(chunk.spans.len() + 1);
                    line.push(Span::raw(" ".repeat(label_width)));
                    line.extend(chunk.spans);
                    out.push(Line::from(line));
                }
            }
            PaneLayout::Stacked => {
                // The label's own padding goes: a label alone on its row
                // reads as "Model:", not as "Model:          ".
                out.push(Line::from(Span::styled(
                    label.content.trim_end().to_string(),
                    label.style,
                )));
                out.push(Line::from(spans));
            }
        }
    }
    out
}

/// Render the topic info pane (the right-hand column, always 20% wide when
/// shown).
///
/// Displays topic name, channel, pattern, model, mode, tokens, a
/// processing indicator, and the changed-files list (which can scroll
/// when it overflows the pane). The pane is one bordered column: the left
/// border runs the full height and the interior splits into a borderless,
/// scrollable topic sub-pane and a fixed status sub-pane at the bottom
/// (see [`chat_status_block`]). Both screens render it — the overview
/// column is the same pane, not a second lookalike.
/// Takes `&mut App` because the changed-files section owns
/// `app.chat.info_scroll`, which is clamped on every render.
pub(super) fn render_topic_info_pane(frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.chat.focus == Focus::InfoPane;
    // One border for both screens: a vertical border on the left edge (against
    // the topics table or the chat pane) and no title, plus one row of top
    // padding so the content does not hug the pane top. While this pane owns
    // the scroll keys its border lights up in the focus colour, like every
    // other pane (see `pane_border_style`).
    let block = Block::default()
        .borders(Borders::LEFT)
        .padding(Padding::top(1))
        .border_style(pane_border_style(focused));
    // The status block (version + gray stats, or a transient message) gets its
    // own sub-pane pinned to the bottom of the info column, separated by a
    // gray top border. It never scrolls — wheel and keys only move the topic
    // info above it. On the chat screen leader `s` hides the block, handing
    // the whole column back to the topic info; the overview has no such
    // command, so the block is always part of its info column. Both screens
    // render it from [`chat_status_block`]; there is no overview-only variant.
    //
    // The stats wrap to the pane's inner width, so the block is built once the
    // inner region is known. The sub-pane renders inside the border, pinned to
    // the bottom of the column: the outer block is drawn over the whole column
    // first so its left border runs the full height, and only the inner region
    // is split into the borderless topic sub-pane and the status sub-pane.
    let inner = block.inner(area);
    // `Ctrl+P` `s` is a chat-screen command, so it decides for the chat
    // screen only; the overview's info column always carries the block.
    let status = if !app.chat.visible || app.chat.status_visible {
        chat_status_block(app, inner.width)
    } else {
        Vec::new()
    };
    frame.render_widget(block, area);
    // The sub-pane is all-or-nothing: its top border, every wrapped row, and
    // the rows the topic info keeps for itself must all fit. A column that
    // cannot afford that loses the block entirely — rendering only the rows
    // that happen to fit would clip the stats again, which is what this block
    // exists to stop (a short terminal with the activity pane open is exactly
    // that column).
    const TOPIC_INFO_MIN_ROWS: u16 = 6;
    // +1: the sub-pane's top border costs a row.
    let needed = status.len() as u16 + 1;
    let (topic_inner, status_area) = if status.is_empty()
        || inner.height < needed + TOPIC_INFO_MIN_ROWS
    {
        (inner, None)
    } else {
        let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(needed)]).split(inner);
        (rows[0], Some(rows[1]))
    };
    if let Some(status_area) = status_area {
        let status_block = Block::default()
            .borders(Borders::TOP)
            .border_style(pane_border_style(false));
        let status_inner = status_block.inner(status_area);
        frame.render_widget(status_block, status_area);
        frame.render_widget(Paragraph::new(status), status_inner);
        // Join the sub-pane's top border with the pane's left border: ratatui
        // only draws corner glyphs when both adjacent borders are set, so the
        // cell left of the sub-pane's top border stays a plain vertical line
        // without this patch.
        if status_area.x > area.x {
            frame.buffer_mut()[(status_area.x - 1, status_area.y)].set_symbol("├");
        }
    }
    // The content rect rather than the topic sub-area: the pane's left
    // border separates it from the chat pane, and the wheel belongs to the
    // chat side of that line.
    app.chat.last_info_area = Some(topic_inner);

    // The whole pane lays its rows out one way, decided by its inner
    // width: the table layout while the pane can hold a label column and
    // a value column, the stacked one below that (see `PaneLayout`).
    let layout = layout_for(topic_inner.width);
    let lines: Vec<Line> = if let Some(t) = selected_topic_detail(app) {
        // The pane's `label: value` rows are kept as span groups — the
        // label first, the value after it — until the layout is known:
        // the table layout puts the two on one row, the stacked layout
        // gives the label a row of its own. The sections (the task list,
        // the files, the subagent tree) are not `label: value` rows and
        // keep the shape they have. Both kinds share one list, in the
        // order they are drawn, so a row added to either keeps its place.
        let mut rows: Vec<InfoRow> = Vec::new();
        rows.push(InfoRow::Field(vec![
            label_span("Topic: "),
            Span::raw(&t.name),
        ]));
        rows.push(InfoRow::Field(vec![
            label_span("Pattern: "),
            Span::raw(t.pattern.as_deref().unwrap_or("-")),
        ]));
        if let Some(ref model) = t.model {
            rows.push(InfoRow::Field(vec![
                label_span("Model: "),
                Span::raw(model),
            ]));
        }
        rows.push(InfoRow::Field(vec![
            label_span("Mode: "),
            Span::raw(t.mode.as_deref().unwrap_or("build")),
        ]));
        // Branch comes from the topic's detail — the same source `/info`
        // reads. Skipped when the topic's topic_path isn't a git repo
        // (most chat-channel topics: feishu/wecom).
        if let Some(branch) = t.branch.as_deref() {
            rows.push(InfoRow::Field(vec![
                label_span("Branch: "),
                Span::raw(branch),
            ]));
        }
        // The width the pane's values get: what the label leaves in the
        // table layout, the whole row in the stacked one. The status row
        // lays its backlog out in it.
        let value_cells = value_width(topic_inner.width, layout);
        // Status row — the topic's live status and the backlog its queue
        // holds, on one row: `Processing · queue ▊▊▊ 2 / 10`. The figures
        // move to the row under it when the pane is too narrow for them
        // beside the blocks.
        let mut status_spans = Vec::with_capacity(8);
        let status_figures = push_status_span(&mut status_spans, t, value_cells);
        rows.push(InfoRow::Field(status_spans));
        if let Some(figures) = status_figures {
            rows.push(InfoRow::Value(figures));
        }
        // The two percentage rows draw their bar as wide as this pane
        // allows — never wider, or the wrapping would cut it in half, and
        // never narrower than a bar can carry the share honestly: a pane
        // too narrow for that gets the figures alone.
        let bar_cells = bar_width(topic_inner.width, layout);
        // Tokens row — the label and the bar, with the figures on the row
        // under them: the bar has the row to itself, so it can reach for
        // the pane's edge, and the figures line up with it.
        let mut token_spans = Vec::with_capacity(3);
        let token_figures = push_tokens_span(&mut token_spans, t, bar_cells);
        if !token_spans.is_empty() {
            rows.push(InfoRow::Field(token_spans));
        }
        if let Some(figures) = token_figures {
            rows.push(InfoRow::Value(figures));
        }
        // Context composition — how the last call's wire messages split
        // across history, the current turn's checkpoint and its recent raw
        // suffix. The bar follows the section's label on the same row; the
        // legend rows under it line up with the bar's first cell and carry
        // the same estimated share `/info` prints.
        if !t.context_composition.is_empty() {
            let mut composition_spans = vec![Span::styled(
                CONTEXT_COMPOSITION_LABEL,
                Style::default().add_modifier(Modifier::BOLD),
            )];
            let bar_room =
                (topic_inner.width as usize).saturating_sub(CONTEXT_COMPOSITION_LABEL.len());
            push_context_composition_bar_spans(&mut composition_spans, t, bar_room);
            rows.push(InfoRow::Raw(Line::from(composition_spans)));
            let total = jyc_types::context_composition_total(&t.context_composition);
            let label_width = jyc_types::context_composition_label_width(&t.context_composition);
            for region in &t.context_composition {
                // A raw row, not a field: the legend's label column and its
                // indent are already part of the text, so the row keeps the
                // bar's column in the stacked layout too — a field would
                // move the label onto a row of its own and lose the indent.
                rows.push(InfoRow::Raw(Line::from(vec![
                    context_region_label_span(
                        region.region,
                        CONTEXT_COMPOSITION_LABEL.len(),
                        label_width,
                    ),
                    Span::raw(jyc_types::format_context_region_share(region, total)),
                ])));
            }
        }
        // Output row — same pattern.
        let mut output_spans = Vec::with_capacity(2);
        push_output_span(&mut output_spans, t);
        if !output_spans.is_empty() {
            rows.push(InfoRow::Field(output_spans));
        }
        // The cache group: the turn's rate, the session's rate and the
        // cache's write volume, under one header — the group names the
        // subject, its rows name their own scope. A raw line, like the
        // task list's and the subagents' headers. The turn's row is always
        // drawn, so the header always has a row under it.
        rows.push(InfoRow::Raw(Line::from(Span::styled(
            "Cache:",
            Style::default().add_modifier(Modifier::BOLD),
        ))));
        // The two cache rows — the latest turn's and the session's — each
        // carry their own hits, input and share, with the share drawn as a
        // bar and the figures on the row under it. The turn's row is
        // always drawn, at zero before the turn's first call; the
        // session's waits for the session's first input tokens.
        let mut turn_cache_spans = Vec::with_capacity(3);
        let turn_figures = push_turn_cache_span(&mut turn_cache_spans, t, bar_cells);
        if !turn_cache_spans.is_empty() {
            rows.push(InfoRow::Field(turn_cache_spans));
        }
        if let Some(figures) = turn_figures {
            rows.push(InfoRow::Value(figures));
        }
        let mut session_cache_spans = Vec::with_capacity(3);
        let session_figures = push_session_cache_span(&mut session_cache_spans, t, bar_cells);
        if !session_cache_spans.is_empty() {
            rows.push(InfoRow::Field(session_cache_spans));
        }
        if let Some(figures) = session_figures {
            rows.push(InfoRow::Value(figures));
        }
        // Create row — cache **write** tokens billed at the
        // creation rate (Anthropic ~1.25× input; GPT-5.6 reports
        // `cache_write_tokens` too). Rendered only when the running
        // total is non-zero, so other sessions see no extra row.
        let mut cache_creation_spans = Vec::with_capacity(2);
        push_cache_creation_span(&mut cache_creation_spans, t);
        if !cache_creation_spans.is_empty() {
            rows.push(InfoRow::Field(cache_creation_spans));
        }
        // Reasoning row — the hidden chain-of-thought share of the output.
        // Rendered only when the provider reports some.
        let mut reasoning_spans = Vec::with_capacity(2);
        push_reasoning_span(&mut reasoning_spans, t);
        if !reasoning_spans.is_empty() {
            rows.push(InfoRow::Field(reasoning_spans));
        }
        // Cost row — session-scoped spend plus today's durable total.
        // Omitted entirely when the model has no configured pricing.
        let mut cost_spans = Vec::with_capacity(2);
        push_cost_span(&mut cost_spans, t);
        if !cost_spans.is_empty() {
            rows.push(InfoRow::Field(cost_spans));
        }
        // The day's spend across every topic, per currency — the same
        // figure `/bill` reports.
        let mut today_all_spans = Vec::with_capacity(2);
        push_today_all_span(&mut today_all_spans, t);
        if !today_all_spans.is_empty() {
            rows.push(InfoRow::Field(today_all_spans));
        }
        // Latency row — the session's average wall-clock time of one API
        // call. Omitted until a call has been measured.
        let mut latency_spans = Vec::with_capacity(2);
        push_latency_span(&mut latency_spans, t);
        if !latency_spans.is_empty() {
            rows.push(InfoRow::Field(latency_spans));
        }
        // Separated section: the agent's task list (`.jyc/tasks.json`),
        // placed between the cost row and the files section. Same
        // `[ ] [~] [✓]` markers and ids the agent's own tools print, so what
        // the user sees and what `task_update` takes agree. Completed fades,
        // in-progress is the row you want to spot. Omitted entirely when the
        // topic has no list; the whole pane (list included) scrolls.
        if !t.tasks.is_empty() {
            rows.push(InfoRow::Raw(Line::default()));
            let (done, total) = t.tasks.progress();
            rows.push(InfoRow::Raw(Line::from(Span::styled(
                format!("Tasks ({done}/{total}):"),
                Style::default().add_modifier(Modifier::BOLD),
            ))));
            for item in &t.tasks.items {
                let style = match item.status {
                    jyc_types::task::TaskStatus::InProgress => Style::default().fg(Color::Yellow),
                    jyc_types::task::TaskStatus::Completed => Style::default().fg(Color::DarkGray),
                    jyc_types::task::TaskStatus::Pending => Style::default(),
                };
                rows.push(InfoRow::Raw(Line::from(Span::styled(
                    format!("  {} {}. {}", item.status.marker(), item.id, item.text),
                    style,
                ))));
            }
        }
        // Separated section: the subagents this topic spawned, as a tree —
        // their own subagents nest beneath them. Same status dot and colours
        // as the explorer, so a topic reads the same in both panes. Omitted
        // entirely when there are none. The tree comes from the topic's own
        // detail — the same shape `/info` reads — so the pane and the
        // command cannot drift apart.
        if let Some(subagents) = t.subagents.as_deref()
            && !subagents.is_empty()
        {
            rows.push(InfoRow::Raw(Line::default()));
            rows.push(InfoRow::Raw(Line::from(Span::styled(
                format!("Subagents ({}):", node_count(subagents)),
                Style::default().add_modifier(Modifier::BOLD),
            ))));
            let mut tree = Vec::new();
            push_tree_rows(&mut tree, subagents, "");
            for row in &tree {
                rows.push(InfoRow::Raw(Line::from(vec![
                    Span::raw(format!(" {}", row.prefix)),
                    Span::styled("● ", status_dot_style(&row.node.status)),
                    Span::raw(row.node.name.as_str()),
                ])));
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
            rows.push(InfoRow::Raw(Line::from(thinking_line)));
        }
        // Separated section at the end: files changed relative to `main`,
        // stamped from the per-topic detail (like the branch row above) onto
        // `TopicSummary.changed_files`, as `Vec<ChangedFileEntry>`. The whole
        // list is rendered (no
        // cap) — the parent pane scrolls when the list overflows. Each
        // path is plain when only committed on the branch, yellow when
        // currently dirty in the working tree, and followed by that same
        // diff's line counts in dim when git reported them. The section
        // is skipped entirely when the field is `None` (not a git repo,
        // both `git diff` invocations failed). Empty `Some(vec![])` is shown
        // as `Files: (none)` so the user knows the field resolved to
        // "no changes".
        if let Some(files) = t.changed_files.as_deref() {
            rows.push(InfoRow::Raw(Line::default()));
            if files.is_empty() {
                rows.push(InfoRow::Raw(Line::from(vec![
                    Span::styled("Files: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled("(none)", Style::default().fg(Color::DarkGray)),
                ])));
            } else {
                rows.push(InfoRow::Raw(Line::from(Span::styled(
                    format!("Files ({}):", files.len()),
                    Style::default().add_modifier(Modifier::BOLD),
                ))));
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
                    let mut spans = vec![Span::styled(format!("{}{}", prefix, entry.path), style)];
                    // Line counts of that same diff, dim so the path
                    // stays the loud part. Absent for a binary file.
                    if let Some(lines) = entry.lines {
                        spans.push(Span::styled(
                            format!(" +{} -{}", lines.insertions, lines.deletions),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    rows.push(InfoRow::Raw(Line::from(spans)));
                }
            }
        }
        // The pane's rows in the order they are drawn: the fields and the
        // sections share one list, so a row added to either keeps its place.
        layout_info_rows(rows, layout, topic_inner.width)
    } else if super::shown_topic_key(app).is_some() {
        // A topic is on screen, but its detail has not landed yet — the
        // fetch runs on a timer. Say so instead of claiming that no topic
        // is selected. A fetch that *failed* says something else: the hub
        // is unreachable (or refusing), and the retry will not fix it
        // until the connection comes back — so it is reported as a
        // failure, not as an eternal load.
        vec![Line::from(if detail_fetch_failed(app) {
            Span::styled(
                "Failed to load topic details — will retry",
                Style::default().fg(Color::Yellow),
            )
        } else {
            Span::raw("Loading topic details...")
        })]
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
    let wrapped = wrap_styled_lines(lines, topic_inner.width as usize);
    let max_skip = wrapped.len().saturating_sub(topic_inner.height as usize);
    let skip = app.chat.info_scroll.min(max_skip);
    frame.render_widget(
        Paragraph::new(wrapped.into_iter().skip(skip).collect::<Vec<_>>()),
        topic_inner,
    );
    app.chat.info_scroll = skip;
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
/// batch changes what `Enter` does, typing an answer changes it again, and the
/// layout has to measure the very text the renderer draws — see
/// [`question_chrome_rows`].
fn question_hint(total: usize, typing: bool) -> String {
    if typing {
        // Shorter than the list's line, and measured like it: the same call
        // decides the box's height and draws it.
        return "Enter keeps this answer - Esc back to the options".to_string();
    }
    let enter = if total > 1 {
        "Enter next, last one sends"
    } else {
        "Enter send"
    };
    format!("Up/Down/j/k move - 1-9/Space pick - d skip - {enter} - Esc closes")
}

/// Ceiling on the rows a question's context may spend above the question. The
/// box is a fixed-height surface, so an uncapped narration would push the
/// question and its options off the bottom — the one thing the user opened it
/// to read. On a terminal too short to afford this many rows the context gives
/// rows back first: [`question_context_budget`] hands out whatever the box can
/// actually spare.
const QUESTION_CONTEXT_ROWS: usize = 10;

/// Rows kept for the message pane while a question box is open. The box and
/// the pane split the body and `Layout` gives the pane `Min(0)`, so without a
/// ceiling a tall context would leave the conversation behind it no rows.
const QUESTION_BOX_MESSAGE_ROWS: usize = 3;

/// The tallest a question box may be, borders and rule included: whatever the
/// body has left once the message pane keeps its rows. Never below 6, the
/// floor the box shares with the editor.
fn question_box_max_rows(body_height: u16) -> usize {
    (body_height as usize)
        .saturating_sub(QUESTION_BOX_MESSAGE_ROWS)
        .max(6)
}

/// The context rows a question may actually show: [`QUESTION_CONTEXT_ROWS`]
/// unless the box cannot spare them once the question, the hint, the spacers,
/// the borders and every option row are paid for. The options win — the context
/// is what the user reads first, but the box exists to answer the question in
/// it.
fn question_context_budget(
    question: &str,
    options: usize,
    total: usize,
    width: u16,
    body_height: u16,
    typing: bool,
) -> usize {
    let fixed = count_wrapped_lines(question, width)
        + count_wrapped_lines(&question_hint(total, typing), width)
        + 4 // the three blank separators, plus the spare row that keeps the
            // ceiling from squeezing an option out of the window
        + options
        + 3; // the box's two borders and the bottom rule under it
    question_box_max_rows(body_height)
        .saturating_sub(fixed)
        .min(QUESTION_CONTEXT_ROWS)
}

/// The context rows to draw above a question: hard-wrapped to the box, cut
/// after `max_rows` rows, and marked with an ellipsis that is paid for by
/// dropping one column of the last row rather than adding a row. `max_rows`
/// comes from [`question_context_budget`] and is 0 when the box has nothing to
/// spare. One function, so the renderer and [`question_chrome_rows`] cannot
/// disagree about the height.
fn question_context_lines(context: &str, width: u16, max_rows: usize) -> Vec<String> {
    if max_rows == 0 {
        return Vec::new();
    }
    let mut lines = wrap_text_to_width(context, width.max(1) as usize);
    if lines.len() > max_rows {
        lines.truncate(max_rows);
        if let Some(last) = lines.last_mut() {
            last.pop();
            last.push('\u{2026}');
        }
    }
    lines
}

/// Rows a question box spends on everything but the options: the context, the
/// question and the hint (all wrap, so all are measured instead of assumed),
/// the blank spacers, and a row of slack for word-boundary wrapping. The box's
/// own borders are not in here — the renderer gets them excluded already from
/// `block.inner`, and the layout adds them. Both sides call this so neither can
/// size the box short of what the other draws: a row short used to cost the
/// hint, then the cursor.
fn question_chrome_rows(
    question: &str,
    context: Option<&str>,
    options: usize,
    total: usize,
    width: u16,
    body_height: u16,
    typing: bool,
) -> usize {
    let budget = question_context_budget(question, options, total, width, body_height, typing);
    // +1 for the blank row that separates the context from the question.
    let context_rows = context
        .filter(|text| !text.trim().is_empty())
        .map_or(0, |text| {
            question_context_lines(text, width, budget).len() + 1
        });
    context_rows
        + count_wrapped_lines(question, width)
        + count_wrapped_lines(&question_hint(total, typing), width)
        + 3
}

/// `body_height` is the rows of the whole body the box lives in, not the box's
/// own: the context is capped by how much of the body the box may take, and
/// the layout and the renderer have to derive that from the same number.
pub(super) fn render_question_box(frame: &mut Frame, area: Rect, app: &App, body_height: u16) {
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
    // among them: `select_question_next` reaches the `✎` row past the last
    // option, so without a window a list deeper than the box parks the `→` below
    // the clip — selectable, invisible. `question_chrome_rows` says which rows
    // those are.
    let rows = q.options.len() + 1;
    let budget = question_context_budget(
        &q.question,
        rows,
        total,
        inner.width,
        body_height,
        app.chat.question_typing,
    );
    let chrome = question_chrome_rows(
        &q.question,
        q.context.as_deref(),
        rows,
        total,
        inner.width,
        body_height,
        app.chat.question_typing,
    );
    let room = (inner.height as usize).saturating_sub(chrome).max(1);
    let off = window_offset(rows, q.selected, room);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(context) = q.context.as_deref().filter(|text| !text.trim().is_empty()) {
        // Above the question, dimmer than it: context to read first, question
        // to act on.
        for row in question_context_lines(context, inner.width, budget) {
            lines.push(Line::from(Span::styled(
                row,
                Style::default().fg(Color::Gray),
            )));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        q.question.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    if q.skipped {
        // Replaces the list rather than dimming it: a greyed-out option still
        // reads as pickable, and the point of this state is that none of them
        // are.
        lines.push(Line::from(Span::styled(
            "\u{2717} no answer \u{b7} d undoes this".to_string(),
            Style::default().fg(Color::Red),
        )));
    } else {
        for (i, opt) in q.options.iter().enumerate().skip(off).take(room) {
            // One row per option: a wrapped one would push the rows below it — and
            // the cursor with them — out of the box.
            // A marked option leads with a box, so it still reads as picked on the
            // dimmed rows that do not carry the cursor.
            let mark = if q.multi {
                if q.marked.contains(&i) {
                    "[✓] "
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
        // The list's last row is the user's own answer: a decision point
        // sometimes needs words none of the options hold. It is a row like the
        // options - one line, inside the same window - so the cursor reaches it
        // and the box's height counts it the same way.
        let typed_row = q.options.len();
        if typed_row >= off && typed_row - off < room {
            // `✎ ` and the `→ `/`  ` gutter take two columns each; one more pays
            // for the cursor block while the draft is open.
            let cursor = usize::from(app.chat.question_typing);
            // A blank draft the user accepted reads as "no answer" until the
            // batch goes out, so the row goes back to saying what it offers;
            // while the draft is open it stays empty, with the cursor on it.
            let blank = q
                .typed
                .as_deref()
                .is_some_and(|text| text.trim().is_empty());
            let label = match q.typed.as_deref() {
                Some(text) if !blank || app.chat.question_typing => format!(
                    "\u{270e} {}",
                    tail_to_width(text, (inner.width as usize).saturating_sub(4 + cursor))
                ),
                _ => truncate_to_width(
                    "\u{270e} type your own answer",
                    inner.width.saturating_sub(2) as usize,
                ),
            };
            let label = if app.chat.question_typing {
                format!("{label}\u{258c}")
            } else {
                label
            };
            if typed_row == q.selected {
                lines.push(Line::from(Span::styled(
                    format!("→ {label}"),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            } else {
                lines.push(Line::from(Span::raw(format!("  {label}"))));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        question_hint(total, app.chat.question_typing),
        Style::default().fg(Color::DarkGray),
    )));

    // `Wrap { trim: false }` is required so the option rows' two-column
    // gutter survives: the default `trim: true` strips leading whitespace
    // per line, which leaves the unselected rows' blank gutter gone and the
    // selected row's `→` as the only indented one.
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, inner);
}

/// Zero-alloc snapshot of the data the chat status line needs. All fields
/// borrow directly from the polled `InspectOverview`. Missing fields are
/// dropped rather than filled in: with no `state` yet, the line is just the
/// mode.
struct ChatHeaderCtx<'a> {
    mode: &'a str,
    topic: Option<&'a str>,
    branch: Option<&'a str>,
    model: Option<&'a str>,
    pct: Option<u32>,
}

fn resolve_header_ctx(app: &App) -> ChatHeaderCtx<'_> {
    let t = selected_topic_detail(app);
    ChatHeaderCtx {
        mode: t.and_then(|t| t.mode.as_deref()).unwrap_or("build"),
        topic: t.map(|t| t.name.as_str()),
        // Read straight off the topic's detail — the same source the info
        // pane renders.
        branch: t.and_then(|t| t.branch.as_deref()),
        model: t.and_then(|t| t.model.as_deref()),
        pct: t.and_then(context_pct),
    }
}

/// Build the chat status line — the pane's bottom row: "{mode} · {topic}[ ·
/// {branch}]" left-aligned, a right-aligned "[ {model} · {pct}% ]" chip
/// showing the current model and context-window usage. Both segments are
/// one dim gray: it is a footer, not a rule, so it carries no ─ padding
/// (the input field has its own rules above). Falls back gracefully when
/// any field is missing — the chip is dropped before the left segment
/// starts truncating.
fn build_status_line(width: usize, ctx: &ChatHeaderCtx<'_>) -> Line<'static> {
    let style = Style::default().fg(Color::Gray);

    // --- Left segment: "{mode} · {topic}[ · {branch}]" ---
    // The topic name is the identity that matters here; channel and
    // pattern are routing metadata and are intentionally not shown.
    let mut left = String::with_capacity(48);
    left.push_str(ctx.mode);
    if let Some(topic) = ctx.topic {
        left.push_str(" · ");
        left.push_str(topic);
    }
    if let Some(branch) = ctx.branch {
        left.push_str(" · ");
        left.push_str(branch);
    }
    let left_w = left.width();

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

    // Width budget: need left + chip + one separating space. Drop the chip
    // first, then shrink the left segment.
    if width < left_w + chip_w + 1 {
        // Chip dropped, left fits on its own.
        if width >= left_w {
            return Line::from(Span::styled(left, style));
        }
        // Left itself doesn't fit; best-effort segments over
        // [topic, branch], adding the separator only when there is
        // room for at least one column of content after it.
        let mut compact = ctx.mode.to_string();
        for seg in [ctx.topic, ctx.branch].into_iter().flatten() {
            let used = compact.width();
            // Need room for " · " (3 cols) plus at least 1 col of content.
            if width < used + 4 {
                break;
            }
            let avail = width - used - 3;
            compact.push_str(" · ");
            compact.push_str(&truncate_to_width(seg, avail));
        }
        return Line::from(Span::styled(compact, style));
    }

    let mut spans = Vec::with_capacity(3);
    spans.push(Span::styled(left, style));
    if let Some(c) = chip {
        // Spaces separate the two segments — at least one, so the chip
        // never sits flush against the left, the rest filling the row.
        // The guard above proved `width >= left_w + chip_w + 1`, so the
        // gap is never negative.
        spans.push(Span::styled(" ".repeat(width - left_w - chip_w), style));
        spans.push(Span::styled(c, style));
    }
    Line::from(spans)
}

/// Truncate `s` to at most `max_width` display columns (per
/// `unicode-width`); if the input is wider, replace the tail with `…`.
fn is_user_visible_activity(entry: &jyc_types::ActivityEntry) -> bool {
    // Heartbeat kinds are dropped by the tracker before they reach the log,
    // but a live buffer can still hold them — hide by kind. Legacy rows
    // (no raw event) keep the old prose sniff for pre-rotation log files.
    if let Some(kind) = entry.event.get("type").and_then(|t| t.as_str()) {
        return !matches!(kind, "processing_progress" | "loop_tick");
    }
    !entry.text.as_deref().unwrap_or("").ends_with(" chars)")
}

pub(super) fn render_activity_log(frame: &mut Frame, area: Rect, app: &mut App) {
    // Activity pane source-of-truth: WS-fed `live_activity` buffer for the
    // currently focused topic. Falls back to empty slice if no live data
    // has been seeded yet (transient state during hydrate).
    let activity_vec: Vec<jyc_types::ActivityEntry> = if app.chat.visible {
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

    let focused = app.chat.focus == Focus::ActivityPane;
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
    // One look for both screens: the chat pane above (or the topics table and
    // info pane, on the overview) is closed off by the top edge, while
    // bottom/left/right sit at the screen edge or against an adjacent pane
    // and are intentionally left open. No title.
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(pane_border_style(focused));

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
                    chrono::DateTime::parse_from_rfc3339(ts).ok().map(|dt| {
                        dt.with_timezone(&chrono::Local)
                            .format("%H:%M:%S")
                            .to_string()
                    })
                })
                .unwrap_or_else(|| "-".to_string());
            let text_style = match entry_severity(entry) {
                Severity::Error => Style::default().fg(Color::Red),
                Severity::Warning => Style::default().fg(Color::Yellow),
                Severity::Info => Style::default(),
            };
            Line::from(vec![
                Span::styled(
                    format!("  {time_str} "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(entry_activity_text(entry), text_style),
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
            topic: None,
            channel: None,
            messages: vec![],
            editor: empty_chat_editor(),
            // The overview is the screen at startup, so its table owns the
            // focus; opening a chat moves it to the input (`reset_chat_state`).
            focus: Focus::TopicsTable,
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
            ws_state: WsState::Connecting,
            live_activity: std::collections::BTreeMap::new(),
            live_chat: std::collections::BTreeMap::new(),
            live_thinking: std::collections::BTreeMap::new(),
            thinking_expanded: false,
            tool_detail_expanded: false,
            minimal_progress: true,
            live_processing: std::collections::BTreeMap::new(),
            live_tick_ms: std::collections::BTreeMap::new(),
            last_seen_activity_id: std::collections::BTreeMap::new(),
            last_seen_chat_seq: std::collections::BTreeMap::new(),
            next_client_msg_seq: 0,
            last_hydrated_key: None,
            open_addr: None,
            commands: vec![],
            command_popup: None,
            leader: None,
            input_history: vec![],
            questions: vec![],
            question_index: 0,
            question_typing: false,
            history_pos: None,
            token: None,
            link_rewrite: None,
        }
    }

    /// Append a message the pane shows, translating tunnel-side link prefixes
    /// first. Every push goes through here so the rewrite has one owner.
    fn push_message(&mut self, mut msg: ChatMessage) {
        if let Some((from, to)) = &self.link_rewrite
            && msg.text.contains(from.as_str())
        {
            msg.text = msg.text.replace(from.as_str(), to);
        }
        self.messages.push(msg);
    }

    pub(super) fn open(
        &mut self,
        addr: &str,
        channel: Option<&str>,
        topic: &str,
        token: Option<String>,
    ) {
        self.visible = true;
        self.channel = channel.map(|s| s.to_string());
        self.token = token;
        self.reset_chat_state(topic);
        // Stash addr so the explorer pane can switch topics later.
        self.open_addr = Some(addr.to_string());

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        self.ws_tx = Some(cmd_tx);
        // Replace the old receiver with the new one
        self.ws_rx = event_rx;

        // A channel-less binding is not merely degraded — it cannot talk:
        // the bare URL is served by the first websocket handler, with no
        // topic scope, and the frames this session sends carry no topic for
        // the server to route (it drops them). Its pane, status line and
        // command list still resolve — which is why the shared topic lookup
        // tolerates a missing channel.
        let url = match channel {
            Some(ch) => format!("ws://{}/ws/{}/{}", addr, ch, topic),
            None => format!("ws://{}/ws", addr),
        };
        tokio::spawn(ws_client_task(
            url,
            cmd_rx,
            event_tx,
            self.token.clone(),
            Heartbeat::default(),
        ));
    }

    /// Clear per-topic state for a switch to `topic`. Used by `open()` and
    /// directly by tests (open() also stashes addr/channel/token and spawns
    /// the WS task, which needs a tokio runtime).
    fn reset_chat_state(&mut self, topic: &str) {
        self.topic = Some(topic.to_string());
        self.messages.clear();
        self.editor = empty_chat_editor();
        self.focus = Focus::ChatPane;
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
        self.ws_state = WsState::Connecting;
        self.input_history.clear();
        self.history_pos = None;
        // Clear the poll-loop's last-hydrated key so it doesn't skip hydrate
        // when we switch back to overview later.
        self.last_hydrated_key = None;
        // The batch belongs to the topic being left behind and this pane can no
        // longer draw it, so drop it as Esc does.
        self.close_questions();
    }

    pub(super) fn close(&mut self) {
        self.visible = false;
        self.ws_state = WsState::Connecting;
        self.command_popup = None;
        self.last_hydrated_key = None;
        // Drop the batch: the pane that draws it is going away, and a half
        // picked set must not come back as the answer to a later question.
        self.close_questions();
        if let Some(tx) = self.ws_tx.take() {
            // Best-effort disconnect signal
            let _ = tx.send("{\"type\":\"disconnect\"}".to_string());
        }
    }

    /// Cycle focus: Input → MessageArea → InfoPane → ActivityPane →
    /// ExplorerPane → Input. Each pane is skipped when it is hidden so
    /// the cycle never lands on an invisible pane. The info pane's
    /// "hidden" state is `!self.info_visible`; the activity pane's is
    /// `self.activity_split == 0`.
    pub(super) fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::ChatPane => {
                // Entering the message pane resets the cursor (as
                // `focus_message_area` does) so the Tab cycle starts reading
                // at the last line, not where a previous visit left it.
                self.reset_cursor();
                Focus::MessageArea
            }
            Focus::MessageArea => {
                if self.info_visible {
                    Focus::InfoPane
                } else if self.activity_split != 0 {
                    Focus::ActivityPane
                } else if self.explorer_visible {
                    Focus::ExplorerPane
                } else {
                    Focus::ChatPane
                }
            }
            Focus::InfoPane => {
                if self.activity_split != 0 {
                    Focus::ActivityPane
                } else if self.explorer_visible {
                    Focus::ExplorerPane
                } else {
                    Focus::ChatPane
                }
            }
            Focus::ActivityPane => {
                if self.explorer_visible {
                    Focus::ExplorerPane
                } else {
                    Focus::ChatPane
                }
            }
            Focus::ExplorerPane => Focus::ChatPane,
            // The topics table belongs to the overview screen, which runs
            // its own cycle (`overview_cycle_focus`); it is never reached
            // from the chat screen, so it falls back to the input.
            Focus::TopicsTable => Focus::ChatPane,
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
            if self.focus == Focus::ActivityPane || self.focus == Focus::InfoPane {
                self.focus = Focus::ChatPane;
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
        if self.focus == Focus::ActivityPane
            || self.focus == Focus::InfoPane
            || self.focus == Focus::ExplorerPane
        {
            self.focus = Focus::ChatPane;
        }
    }

    /// Toggle the chat status block in the topic info pane (leader-key
    /// popup `s`).
    pub(super) fn toggle_status_bar(&mut self) {
        self.status_visible = !self.status_visible;
    }

    /// Toggle the topic info pane (leader-key popup `i`). Hiding it
    /// while focused moves focus back to the chat pane.
    pub(super) fn toggle_info_pane(&mut self) {
        self.info_visible = !self.info_visible;
        if !self.info_visible && self.focus == Focus::InfoPane {
            self.focus = Focus::ChatPane;
        }
    }

    /// Toggle the topic explorer pane (left side). Opening moves focus
    /// into it so j/k/Enter are immediately usable; closing returns
    /// focus to the chat input.
    pub(super) fn toggle_explorer(&mut self) {
        self.explorer_visible = !self.explorer_visible;
        if self.explorer_visible {
            self.focus = Focus::ExplorerPane;
        } else if self.focus == Focus::ExplorerPane {
            self.focus = Focus::ChatPane;
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

    /// Focus the scrollable message area, cursor reset to the newest content:
    /// every entry into the pane starts reading from the last line, the way
    /// `reset_cursor` leaves a freshly loaded transcript. A no-op when the
    /// pane already has focus — a wheel tick or a repeated key must not yank
    /// the cursor off the row the user is reading.
    fn focus_message_area(&mut self) {
        if self.focus == Focus::MessageArea {
            return;
        }
        self.focus = Focus::MessageArea;
        self.reset_cursor();
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

    /// `[` / `]`: cursor and view to the previous / next user turn — a section
    /// is the user's message plus the agent's replies up to the next user
    /// message. The boundaries are the opening-rule lines recorded by
    /// `render_history_lines` and carried in `render_cache`; history comes
    /// first in the transcript, so they are directly comparable to
    /// `cursor_line`.
    ///
    /// Lands on the nearest boundary strictly in `dir` — sitting exactly on
    /// one walks to the next, so a held key steps section by section (vim
    /// paragraph semantics) — and clamps to the transcript edge past the
    /// first/last turn. `count` (`3]`) repeats the jump; `extend` (a Shift
    /// key) grows a selection like `cursor_step`. Returns the number of
    /// selected rows, 0 when nothing is selected.
    pub(super) fn cursor_section_jump(&mut self, dir: i32, extend: bool) -> usize {
        let Some(last) = self.last_total_lines.checked_sub(1) else {
            return 0;
        };
        // Cloned: the search below must not hold the cache borrow while the
        // cursor and view are updated. One entry per user turn — small.
        let Some(turns) = self.render_cache.as_ref().map(|(_, _, t)| t.clone()) else {
            return 0;
        };
        let mut from = self.cursor_line.min(last);
        if extend {
            self.selection_anchor = Some(self.selection_anchor.unwrap_or(from));
        }
        let count = self.take_count();
        for _ in 0..count {
            let target = if dir < 0 {
                turns
                    .iter()
                    .rev()
                    .find(|&&b| b < from)
                    .copied()
                    .unwrap_or(0)
            } else {
                turns.iter().find(|&&b| b > from).copied().unwrap_or(last)
            };
            // A clamped jump lands on the edge; the next step would find no
            // boundary past it and stick there too — stop instead.
            if target == from {
                break;
            }
            from = target;
        }
        self.cursor_line = from;
        self.scroll_to_show(from);
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
            Focus::ChatPane | Focus::MessageArea => {
                let before = self.view_start_line();
                self.scroll = self.scroll.saturating_add(1).min(self.last_max_scroll);
                self.carry_cursor(before, self.view_start_line());
            }
            Focus::ActivityPane => self.activity_scroll = self.activity_scroll.saturating_add(1),
            // Info pane uses offset-from-top semantics (vs activity's
            // offset-from-bottom). `scroll_up` → earlier rows → smaller
            // offset.
            Focus::InfoPane => self.info_scroll = self.info_scroll.saturating_sub(1),
            Focus::ExplorerPane | Focus::TopicsTable => {}
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
            Focus::ChatPane | Focus::MessageArea => {
                let before = self.view_start_line();
                self.scroll = self.scroll.saturating_sub(1);
                self.carry_cursor(before, self.view_start_line());
            }
            Focus::ActivityPane => self.activity_scroll = self.activity_scroll.saturating_sub(1),
            // Info pane: scroll down → later rows → larger offset.
            Focus::InfoPane => self.info_scroll = self.info_scroll.saturating_add(1),
            Focus::ExplorerPane | Focus::TopicsTable => {}
        }
    }

    /// Jump to the oldest message (top) of the focused pane.
    ///
    /// The offset is clamped to the actual maximum during rendering, so
    /// setting it to `usize::MAX` is a safe "scroll all the way up".
    pub(super) fn scroll_to_top(&mut self) {
        match self.focus {
            Focus::ChatPane | Focus::MessageArea => {
                let before = self.view_start_line();
                self.scroll = usize::MAX;
                self.carry_cursor(before, self.view_start_line());
            }
            Focus::ActivityPane => self.activity_scroll = usize::MAX,
            // Info pane: offset-from-top, so "top" is offset = 0.
            Focus::InfoPane => self.info_scroll = 0,
            Focus::ExplorerPane | Focus::TopicsTable => {}
        }
    }

    /// Jump to the latest message (bottom) of the focused pane.
    pub(super) fn scroll_to_bottom(&mut self) {
        match self.focus {
            Focus::ChatPane | Focus::MessageArea => {
                let before = self.view_start_line();
                self.scroll = 0;
                self.carry_cursor(before, self.view_start_line());
            }
            Focus::ActivityPane => self.activity_scroll = 0,
            // Info pane: clamped to max in render.
            Focus::InfoPane => self.info_scroll = usize::MAX,
            Focus::ExplorerPane | Focus::TopicsTable => {}
        }
    }

    /// Apply a scroll key to the pane that owns the keys.
    ///
    /// The one key map for the scrollable panes — `j`/`k`, `↑`/`↓`, `G`, `gg`
    /// and the horizontal step on the activity pane — on both screens. The
    /// scroll direction rules live in `scroll_up`/`scroll_down` and friends,
    /// which read `self.focus`, so the caller only has to name the pane once
    /// (through the shared `Focus`). Returns `true` when `key` was a scroll
    /// key, letting a caller tell "handled" from "not one": the chat screen
    /// refocuses the input on the latter, the overview leaves it to its own
    /// keys. `Esc` is not a scroll key and never reaches here.
    ///
    /// `gg_jump` is the second half of the `gg` sequence (`gg_step`); a lone
    /// `g` is swallowed so the caller does not read it as "not one".
    pub(super) fn scroll_pane_key(&mut self, key: event::KeyEvent, gg_jump: bool) -> bool {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_up(),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_down(),
            KeyCode::Char('G') => self.scroll_to_bottom(),
            KeyCode::Char('g') => {
                if gg_jump {
                    self.scroll_to_top();
                }
            }
            // Horizontal step, activity pane only: the info pane's rows are
            // short enough that overflow is not a concern.
            KeyCode::Left if self.focus == Focus::ActivityPane => {
                self.activity_hscroll = self.activity_hscroll.saturating_sub(1)
            }
            KeyCode::Right if self.focus == Focus::ActivityPane => {
                self.activity_hscroll = self.activity_hscroll.saturating_add(1)
            }
            _ => return false,
        }
        true
    }

    /// Rows the input area occupies: 1 mode header row, the wrapped editor
    /// rows (or the question box), and the full-width bottom rule. The
    /// renderer (`render_chat_conversation`) and `page_size` must agree on
    /// this number — this helper is the single place that measures it.
    /// Widths mirror where content actually wraps: the editor spans the
    /// full body width, the question box two columns narrower for its own
    /// borders. `body_height` is the height of that whole body: a question
    /// box's ceiling comes from it, so both callers must hand over the same
    /// number or the box would be sized against a screen it is not drawn on.
    fn input_area_rows(&self, width: u16, body_height: u16) -> u16 {
        let body = if self.active_question() {
            let q = self.current_question().expect("active question");
            // `+ 1` twice: the list ends with the `✎` row, which the chrome
            // counts (it is one more row the context has to yield to) and the
            // body's own rows have to make room for as well.
            (question_chrome_rows(
                &q.question,
                q.context.as_deref(),
                q.options.len() + 1,
                self.questions.len(),
                width.saturating_sub(2),
                body_height,
                self.question_typing,
            ) + q.options.len()
                + 1
                + 3)
            .clamp(6, question_box_max_rows(body_height)) as u16
        } else {
            (count_wrapped_lines(&self.text(), width) + 1).clamp(2, 11) as u16
        };
        // +1: the bottom rule row below the editor.
        body + 1
    }

    pub(super) fn page_size(&self) -> usize {
        // What the renderer last gave the messages is the page: it already has
        // the input area, the popup slot and the status line taken out of it,
        // and it is the only source that also knows how tall the question box
        // came out. Before the first frame there is nothing to agree with, so
        // fall back to the arithmetic below.
        if matches!(self.focus, Focus::ChatPane | Focus::MessageArea)
            && let Some(area) = self.last_message_area
        {
            return (area.height as usize).max(1);
        }
        let base = crossterm::terminal::size()
            .map(|(_, h)| h.saturating_sub(7) as usize)
            .unwrap_or(10);
        match self.focus {
            Focus::ChatPane | Focus::MessageArea => {
                let term_width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
                // The status line is permanent chrome, exactly like the
                // input area above it — `STATUS_LINE_ROWS` is the same
                // count the renderer gives its last chunk.
                base.saturating_sub(
                    self.input_area_rows(term_width, base as u16) as usize
                        + STATUS_LINE_ROWS as usize,
                )
                .max(1)
            }
            Focus::ActivityPane | Focus::ExplorerPane | Focus::InfoPane | Focus::TopicsTable => {
                base.max(1)
            }
        }
    }

    pub(super) fn page_up(&mut self) {
        let page = self.page_size();
        match self.focus {
            Focus::ChatPane | Focus::MessageArea => {
                let before = self.view_start_line();
                self.scroll = self.scroll.saturating_add(page).min(self.last_max_scroll);
                self.carry_cursor(before, self.view_start_line());
            }
            Focus::ActivityPane => self.activity_scroll = self.activity_scroll.saturating_add(page),
            // Info pane: offset-from-top, so "page up" → smaller offset.
            Focus::InfoPane => self.info_scroll = self.info_scroll.saturating_sub(page),
            Focus::ExplorerPane | Focus::TopicsTable => {}
        }
    }

    pub(super) fn page_down(&mut self) {
        let page = self.page_size();
        match self.focus {
            Focus::ChatPane | Focus::MessageArea => {
                let before = self.view_start_line();
                self.scroll = self.scroll.saturating_sub(page);
                self.carry_cursor(before, self.view_start_line());
            }
            Focus::ActivityPane => self.activity_scroll = self.activity_scroll.saturating_sub(page),
            // Info pane: offset-from-top, so "page down" → larger offset.
            Focus::InfoPane => self.info_scroll = self.info_scroll.saturating_add(page),
            Focus::ExplorerPane | Focus::TopicsTable => {}
        }
    }

    /// Current chat input text (editor lines joined with newlines).
    pub(super) fn text(&self) -> String {
        self.editor.lines().join("\n")
    }

    pub(super) fn send_message(&mut self) {
        // `trim_end` only: leading whitespace is user content (indentation),
        // and `send_message_inner` no-ops on empty text.
        let text = self.text().trim_end().to_string();
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
        // The echo carries an id of its own so the row the server broadcasts
        // back replaces it in place rather than showing up beside it.
        let _ = self.topic.as_ref(); // topic must be set before send
        self.next_client_msg_seq += 1;
        let client_id = format!("{}-{}", std::process::id(), self.next_client_msg_seq);
        self.push_message(ChatMessage {
            sender: "user".to_string(),
            text: text.clone(),
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
            id: 0,
            client_id: Some(client_id.clone()),
        });
        // The /ws/<channel>/<topic> URL already carries the topic name.
        // Both ScopedWsHandler (websocket channel) and TopicProxyHandler
        // (any other channel) bind the topic from the URL, so the payload
        // doesn't need a `topic` field.
        let msg = serde_json::json!({
            "type": "message",
            "text": text,
            "metadata": { "jyc_client_msg_id": client_id },
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
        // Step to another question: the draft belongs to the question being
        // left, and each keeps its own. A backstop only - `←/→` are dispatched
        // from the list branch, so a draft never gets here.
        self.question_typing = false;
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

    /// Move the question selection down (clamped at the last row, which is the
    /// `✎` one — Enter there starts a typed answer).
    fn select_question_next(&mut self) {
        if let Some(q) = self.current_question_mut()
            && q.selected < q.options.len()
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
                // Marking is an answer too, and it replaces a typed one.
                q.typed = None;
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
        // The `✎` row is not an option, in either mode: Space opens the draft
        // there exactly as Enter does. Marking an index no option has would put
        // a phantom mark in the answer - invisible on screen, dropped at submit
        // - and would wipe a draft the user typed.
        if self.on_typed_row() {
            self.begin_typed_answer();
            return;
        }
        let multi = self.current_question().is_some_and(|q| q.multi);
        if !multi {
            self.confirm_question();
            return;
        }
        if let Some(q) = self.current_question_mut() {
            toggle_marked(&mut q.marked, q.selected);
            // Marking is an answer too, and it replaces a typed one.
            q.typed = None;
        }
    }

    /// Enter: settle the current question and step to the next one; on the
    /// last question, send every answer at once. Nothing leaves before that,
    /// which is what keeps `←/→` able to revisit an answer.
    ///
    /// Enter means whatever the row under the cursor means: while a draft is
    /// open it keeps what was typed, on the list's `✎` row it opens one, on an
    /// option it settles the question. The key handler routes every Enter here
    /// so this stays the one place that decides.
    fn confirm_question(&mut self) {
        if self.question_typing {
            self.accept_typed_answer();
            return;
        }
        if self.on_typed_row() {
            self.begin_typed_answer();
            return;
        }
        // An option settles this question, so a draft left from an earlier
        // attempt goes: a pick and a typed answer replace each other, and the
        // one that stands should be the one the user reached for last.
        self.discard_typed_answer();
        self.advance_or_submit();
    }

    /// Drop the current question's typed answer, leaving the list to answer it.
    fn discard_typed_answer(&mut self) {
        if let Some(q) = self.current_question_mut() {
            q.typed = None;
        }
    }

    /// Move on to the next question, or send the batch when this was the last
    /// one. The step an option takes on Enter, and a typed answer too.
    fn advance_or_submit(&mut self) {
        if self.question_index + 1 < self.questions.len() {
            self.question_index += 1;
            return;
        }
        self.submit_questions();
    }

    /// Whether the cursor is on the list's `✎` row: past the last option and
    /// not in the `d` state, which replaces the list. Enter there starts a
    /// typed answer instead of settling the question.
    fn on_typed_row(&self) -> bool {
        self.current_question()
            .is_some_and(|q| !q.skipped && q.selected == q.options.len())
    }

    /// Enter on the `✎` row: type this question's answer. Coming back to a
    /// question that has one already reopens it for editing rather than
    /// starting empty.
    fn begin_typed_answer(&mut self) {
        let Some(q) = self.current_question_mut() else {
            return;
        };
        if q.typed.is_none() {
            q.typed = Some(String::new());
        }
        self.question_typing = true;
    }

    /// Enter in the draft: keep it as this question's answer and move on - the
    /// same step/send an option gets. An empty draft stays as `Some("")`, which
    /// submits as `(skipped)`.
    fn accept_typed_answer(&mut self) {
        self.question_typing = false;
        self.advance_or_submit();
    }

    /// Esc in the draft: hand the keys back to the list. The cursor stays on the
    /// `✎` row (so Enter reopens what was typed) and the draft is kept - coming
    /// back to a question should not mean retyping it. Only an empty draft is
    /// dropped, which is what keeps Esc from leaving a blank answer behind that
    /// quietly overrides the options.
    fn cancel_typed_answer(&mut self) {
        self.question_typing = false;
        if let Some(q) = self.current_question_mut()
            && q.typed.as_deref() == Some("")
        {
            q.typed = None;
        }
    }

    /// A character typed into the draft. The row is one line, like every other
    /// row of the box, so the answer is one line too.
    fn push_typed_char(&mut self, c: char) {
        if let Some(q) = self.current_question_mut()
            && let Some(text) = q.typed.as_mut()
        {
            text.push(c);
        }
    }

    /// Backspace in the draft.
    fn pop_typed_char(&mut self) {
        if let Some(q) = self.current_question_mut()
            && let Some(text) = q.typed.as_mut()
        {
            text.pop();
        }
    }

    /// Send the whole batch as one ordinary chat message, oldest question
    /// first, and close the box. One line per question, keyed by the number the
    /// question was rendered with and carrying both the option number and its
    /// text, so the model can read the answer without any protocol of its own:
    /// `Q1: 2) 面条`. A typed answer says so by carrying no option number at all -
    /// `Q2: their own words` - which is how the model tells it from a pick. A
    /// skipped question (`d`, or a multi with nothing marked - the user backed
    /// out of that one) says so on its own line.
    fn submit_questions(&mut self) {
        let questions = std::mem::take(&mut self.questions);
        self.question_index = 0;
        self.question_typing = false;
        let mut lines = Vec::with_capacity(questions.len());
        for (number, q) in questions.iter().enumerate() {
            // `d` wins over a draft the user later backed out of: it is the
            // more recent and more explicit "no answer".
            if !q.skipped
                && let Some(text) = q.typed.as_deref().filter(|text| !text.trim().is_empty())
            {
                lines.push(format!("Q{}: {text}", number + 1));
                continue;
            }
            let picks: Vec<(usize, String)> = if q.skipped {
                Vec::new()
            } else if !q.multi {
                q.options
                    .get(q.selected)
                    .map(|text| vec![(q.selected, text.clone())])
                    .unwrap_or_default()
            } else {
                let mut marked = q.marked.clone();
                marked.sort_unstable();
                marked
                    .iter()
                    .filter_map(|&i| q.options.get(i).map(|text| (i, text.clone())))
                    .collect()
            };
            if picks.is_empty() {
                lines.push(format!("Q{}: (skipped)", number + 1));
                continue;
            }
            let picked: Vec<String> = picks
                .iter()
                .map(|(i, text)| format!("{}) {}", i + 1, text))
                .collect();
            lines.push(format!("Q{}: {}", number + 1, picked.join(", ")));
        }
        self.send_message_inner(lines.join("\n"));
    }

    /// Esc / leaving the pane: drop the pending batch. Nothing is sent and
    /// nothing needs settling - the turn that asked these questions ended when
    /// they were rendered, and the user may still answer in ordinary text.
    fn close_questions(&mut self) {
        self.questions.clear();
        self.question_index = 0;
        self.question_typing = false;
    }

    /// `d` / `Backspace`: skip this question without costing the rest of the set.
    ///
    /// A question the user cannot answer needs its own "no" — otherwise the only
    /// way out is Esc, which drops the picks already made for the rest of the
    /// batch. The flag rides to submit, where a skipped question says so.
    fn toggle_question_skip(&mut self) {
        if let Some(q) = self.current_question_mut() {
            q.skipped = !q.skipped;
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
        // The box owns the keyboard from the moment it appears: a command popup
        // or a leader session left over from before the frame landed would take
        // its Enter (the popup's fires the highlighted slash command instead of
        // keeping the answer) and its Esc.
        self.command_popup = None;
        self.leader = None;
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
        // questions at once, and the one already on screen still has to be
        // answered. The user steps through the batch with ←/→ and the whole set
        // goes out as one message on the last Enter.
        self.questions.push(PendingQuestion {
            topic: topic.to_string(),
            context: parsed
                .get("context")
                .and_then(|v| v.as_str())
                .map(String::from),
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
            skipped: false,
            typed: None,
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
    /// (initial fetch on topic selection). Sets the two seen-watermarks to the
    /// highest ids seen so duplicate WS events are dropped.
    pub(super) fn seed_live(
        &mut self,
        channel: &str,
        topic: &str,
        activity: Vec<jyc_types::ActivityEntry>,
        chat: Vec<jyc_types::ChatMessageEntry>,
    ) {
        let key = (channel.to_string(), topic.to_string());
        // Activities and chat rows are numbered by different owners and live
        // in different number spaces, so they need separate watermarks.
        let max_activity_id = activity.iter().map(|e| e.id).max().unwrap_or(0);
        let max_chat_seq = chat.iter().map(|e| e.id).max().unwrap_or(0);
        let activity_buf: std::collections::VecDeque<_> = activity.into_iter().collect();
        let chat_buf: std::collections::VecDeque<_> = chat.into_iter().collect();
        // Bound the buffers. The activity cap matches what `hydrate_live` asks
        // for; the chat cap matches its request too (50 rows).
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
        self.last_seen_activity_id
            .insert(key.clone(), max_activity_id);
        self.last_seen_chat_seq.insert(key.clone(), max_chat_seq);
    }

    /// Handle a parsed `{"type":"activity",...}` or similar WS payload.
    /// Filters out duplicate / older events using the per-stream watermark.
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

        let event_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            "activity" => {
                let id = payload.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let last = self.last_seen_activity_id.get(&key).copied().unwrap_or(0);
                if id != 0 && id <= last {
                    return; // duplicate or older
                }
                if id != 0 {
                    self.last_seen_activity_id.insert(key.clone(), id);
                }
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
                    // Chat rows are numbered by the chat log, not by the
                    // activity stream, so they use their own watermark.
                    let last = self.last_seen_chat_seq.get(&key).copied().unwrap_or(0);
                    if entry.id != 0 && entry.id <= last {
                        return; // duplicate or older
                    }
                    if entry.id != 0 {
                        self.last_seen_chat_seq.insert(key.clone(), entry.id);
                    }
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
                    // Fold while the pane is open rather than waiting for a
                    // timer: this is what makes a reply show up at WS speed.
                    self.sync_live_if_visible(&channel, &topic);
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
                        && self.is_visible_topic(&key.0, &key.1)
                    {
                        self.push_message(ChatMessage {
                            sender: "thinking".to_string(),
                            text: blocks.join("\n\n"),
                            timestamp: Some(chrono::Utc::now().to_rfc3339()),
                            id: 0,
                            client_id: None,
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
                // Server fell behind (Lagged): the buffers are a cache of what
                // the socket carried, so re-fetch them. `messages` describes
                // the *pane*, which the wipe does not touch: the hydrate's
                // rows match it by sequence number, so nothing already on
                // screen is drawn twice. Rows missed while lagged stay
                // missing until the next hydrate (a reconnect).
                self.live_activity.remove(&key);
                self.live_chat.remove(&key);
                self.live_thinking.remove(&key);
                self.live_processing.remove(&key);
                self.live_tick_ms.remove(&key);
                self.last_seen_activity_id.remove(&key);
                self.last_seen_chat_seq.remove(&key);
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
    /// The pane's copy comes from `sync_live_if_visible`, which folds this.
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

    /// Whether `(channel, topic)` is the topic the *open* chat pane is bound
    /// to. Events for anything else stay in their buffers. `close()` leaves
    /// the binding in place, so `visible` is part of the test — otherwise a
    /// hidden pane would keep folding rows nobody can see.
    fn is_visible_topic(&self, channel: &str, topic: &str) -> bool {
        self.visible
            && self.channel.as_deref() == Some(channel)
            && self.topic.as_deref() == Some(topic)
    }

    /// Fold `(channel, topic)`'s live buffer into the pane, if it is the topic
    /// on screen, and keep the pane pinned to the newest row. Returns whether
    /// anything was appended.
    ///
    /// This is the single entry point for "show what arrived": the buffer is
    /// written by the WS handler and by `hydrate_live`, and both call this
    /// instead of leaving the pane to a poll timer.
    pub(super) fn sync_live_if_visible(&mut self, channel: &str, topic: &str) -> bool {
        if !self.is_visible_topic(channel, topic) {
            return false;
        }
        if !self.sync_live_chat(channel, topic) {
            return false;
        }
        self.scroll = 0;
        true
    }

    /// Append the live buffer's rows the pane is not showing yet. Returns
    /// `true` if the pane changed.
    ///
    /// One rule: a row is appended unless the pane already holds a row with
    /// its chat sequence number. The number is allocated once, where the
    /// message entered the system, and the chat log stores the same value —
    /// so the live row and the historical row are the same row, and no
    /// comparison of message text is needed (or wanted: it silently dropped a
    /// repeated message).
    ///
    /// Membership, not a high-water mark. Numbers are handed out in arrival
    /// order but a row is written when it is delivered or injected, so a
    /// message that arrived mid-turn is logged *after* replies that took
    /// higher numbers: a hydrate hands its rows over out of id order, and
    /// "greater than the highest drawn" would skip the out-of-order row for
    /// the rest of the session.
    ///
    /// The one exception is the row this client is the author of: it already
    /// drew an optimistic echo (`id == 0`, tagged with a client id). The
    /// server's row carries that same client id back, and replaces the echo
    /// in place instead of being appended beside it.
    ///
    /// Rows with `id == 0` and no matching echo were logged before the
    /// sequence existed; they are not drawn.
    fn sync_live_chat(&mut self, channel: &str, topic: &str) -> bool {
        // Collect into a Vec first to release the immutable borrow on
        // self.live_chat before mutating self.messages.
        let live_msgs: Vec<jyc_types::ChatMessageEntry> =
            self.live_chat_for(channel, topic).cloned().collect();
        let mut changed = false;
        for msg in &live_msgs {
            if msg.id == 0 {
                continue;
            }
            if let Some(client_id) = msg.client_msg_id.as_deref()
                && let Some(echo) = self
                    .messages
                    .iter_mut()
                    .find(|m| m.id == 0 && m.client_id.as_deref() == Some(client_id))
            {
                // Confirm the optimistic echo: adopt the server's sequence
                // number and timestamp, keep its place in the list.
                echo.id = msg.id;
                echo.timestamp = msg.timestamp.clone();
                changed = true;
            } else if !self.messages.iter().any(|m| m.id == msg.id) {
                self.push_message(ChatMessage {
                    sender: msg.sender.clone(),
                    text: msg.text.clone(),
                    timestamp: msg.timestamp.clone(),
                    id: msg.id,
                    client_id: msg.client_msg_id.clone(),
                });
                changed = true;
            }
        }
        changed
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
