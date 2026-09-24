//! Chat rendering helpers.

use super::table_wrap::wrap_tables;
use super::*;
use super::{App, ChatMessage, LINE_DRAWING};

/// Max lines a tool detail expansion can render before trailing it with
/// a `… (N more lines)` marker. Shared by `render_file_tool_diff` (edit /
/// write diffs) and `format_tool_input_full` (generic tool field listings).
const TOOL_DETAIL_MAX_LINES: usize = 20;

/// Prefix `render_file_tool_diff` emits for removed lines. Two leading
/// spaces indent the diff one column past the file header line. Kept as
/// constants so `render_file_tool_diff` (producer) and `style_diff_line`
/// (consumer) cannot drift apart — a drift here would silently re-break
/// the gray/green styling.
const DIFF_REMOVED_PREFIX: &str = "  -";
/// Prefix `render_file_tool_diff` emits for added lines.
const DIFF_ADDED_PREFIX: &str = "  +";

pub(super) fn truncate_to_width(s: &str, max_width: usize) -> String {
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

/// Fingerprint of the message history + pane width used to invalidate
/// `ChatState::render_cache`. Covers every message mutation in the
/// codebase (push, last-message streaming append, clear) — messages are
/// never edited in place mid-history. The cache is additionally reset on
/// `open()` so a cross-topic collision (same count/lengths/timestamp)
/// can never serve the previous topic's lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderFingerprint {
    count: usize,
    text_len_sum: usize,
    last_timestamp: Option<String>,
    width: usize,
    /// How many messages render as the human side (the background block).
    /// The sender is otherwise invisible to the fingerprint, so a message
    /// that flips sides with the same length and timestamp would serve stale
    /// styled lines from the cache.
    human_side: usize,
    /// Thinking expand/collapse affects the rendered lines of
    /// `sender == "thinking"` pseudo-messages, so it must bust the cache.
    thinking_expanded: bool,
    /// Minimal progress mode drops that same thinking line, so it affects the
    /// cached history too — see [`thinking_expanded`].
    minimal_progress: bool,
}

pub(super) fn history_fingerprint(
    messages: &[ChatMessage],
    width: usize,
    thinking_expanded: bool,
    minimal_progress: bool,
) -> RenderFingerprint {
    RenderFingerprint {
        count: messages.len(),
        text_len_sum: messages.iter().map(|m| m.text.len()).sum(),
        last_timestamp: messages.last().and_then(|m| m.timestamp.clone()),
        width,
        human_side: messages
            .iter()
            .filter(|m| is_user_message(&m.sender))
            .count(),
        thinking_expanded,
        minimal_progress,
    }
}

/// Background of a human turn's block (see [`render_history_lines`]).
///
/// `#343541` — the shade pi's dark theme paints its own user turns with
/// (`userMsgBg` in `theme/dark.json`), so the block matches what the eye is
/// already used to. It is a hardcoded RGB on purpose: ANSI palette slot 8 was
/// the previous answer, and it turned out to be the theme's *bright* black,
/// lighter than this block wants.
///
/// The block sets **no foreground** — the text keeps the terminal's own color,
/// which is what makes a human turn read at exactly the same brightness as the
/// agent's reply.
pub(super) const USER_BG: Color = Color::Rgb(52, 53, 65);

/// Background for the message-pane highlight: the same bar for the cursor row and
/// for every selected row (`Shift+J` to start a selection, any movement key to
/// extend it), so a selection reads as one thing instead of two.
///
/// A dim navy, far enough from [`USER_BG`]'s neutral gray to read as a highlight
/// rather than as part of a message. Unlike [`USER_BG`] it cannot be an ANSI name
/// — no theme color is both distinct from the message gray and light-theme safe —
/// so a *selected* row additionally forces a light foreground (on a light-theme
/// terminal the default foreground is black, which would vanish on this navy),
/// while the cursor row gives up its background alone — it is the highlight that
/// sits on screen while you are merely reading, and the text's own colours (dim
/// metadata, markdown colours) are worth more there than they are mid-selection.
/// Inside a selection the cursor row is painted with the range, so it takes the
/// light foreground like its neighbours.
pub(super) const SELECT_BG: Color = Color::Rgb(35, 58, 84);

/// Paint one transcript row with the highlight bar (the cursor row, or a selected
/// row — both on [`SELECT_BG`]).
///
/// `bar` says what the row gives up: a selected row overrides the foreground as
/// well (it has to stay legible on the navy — see [`SELECT_BG`]), the cursor row
/// only the background, so the text under it keeps its own colour. Either way the
/// row is padded out with
/// spaces because `Paragraph` paints a style only where it has glyphs — without
/// the padding the bar stops at the last character instead of crossing the pane.
fn paint_row(line: &mut Line<'static>, width: u16, bar: Style) {
    for span in &mut line.spans {
        span.style = span.style.patch(bar);
    }
    let pad = width.saturating_sub(line.width() as u16);
    if pad > 0 {
        line.spans.push(Span::styled(" ".repeat(pad as usize), bar));
    }
}

/// Whether a chat message belongs to the human side of the conversation.
///
/// The agent's replies carry `sender == "ai"`; every other sender — the chat
/// pane's `"user"`, or a remote user's display name from a piped channel
/// (e.g. feishu via `pipe`) — is the human side.
fn is_user_message(sender: &str) -> bool {
    sender != "ai"
}

/// Render the full message history to wrapped, styled lines: per-round
/// top/bottom rules (time / duration) of exactly `width`, plus each human or
/// agent message's markdown body, wrapped one column narrower than the pane and
/// inset by that column — no text starts flush against the pane edge. There is
/// no speaker label — the human side is identified by a background block (see
/// [`USER_BG`]), the agent's replies sit on the pane background. Pure in
/// `(messages, width)` so the result is cached per frame — the dynamic progress
/// tail (thinking / activity / live ticker) is appended by the caller after
/// these lines and stays per-frame.
pub(super) fn render_history_lines(
    messages: &[ChatMessage],
    width: usize,
    thinking_expanded: bool,
    minimal_progress: bool,
) -> Vec<Line<'static>> {
    let mut all_lines: Vec<Line<'static>> = Vec::new();

    let dim_style = Style::default().fg(Color::DarkGray);
    // The pane is `width` wide; message bodies give up one column to the inset
    // they are drawn with (see the insert below), so a row never overflows.
    let body_width = width.saturating_sub(1);
    // The human side of the conversation: a background block in place of a
    // label. No foreground is set on purpose — see [`USER_BG`]: the text keeps
    // the terminal's own color, so a human turn reads at exactly the brightness
    // of the agent's reply.
    let user_style = Style::default().bg(USER_BG);
    let thinking_style = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::ITALIC);
    let mut group_start_ts: Option<String> = None;
    // Last *conversation* sender (user/AI) — thinking pseudo-messages are
    // skipped so the human→AI blank row and the AI→human round close still
    // fire across an interleaved thinking block. Tracked incrementally.
    let mut prev_conv_sender: Option<&str> = None;

    for (idx, msg) in messages.iter().enumerate() {
        // Completed-turn thinking pseudo-message: collapsed one-liner by
        // default, full wrapped text when expanded. Takes no part in
        // round-rule grouping — the rules key off user/AI turns only.
        if msg.sender == "thinking" {
            if minimal_progress {
                // Minimal progress mode: the thinking line is a progress
                // artifact, not conversation — leave it out, blank row and all,
                // so no orphan gap is left where it used to sit.
                continue;
            }
            if thinking_expanded && !msg.text.is_empty() {
                for line in wrap_text_to_width(&msg.text, width) {
                    all_lines.push(Line::from(Span::styled(line, thinking_style)));
                }
            } else {
                let chars = msg.text.chars().count();
                all_lines.push(Line::from(Span::styled(
                    format!("💭 thinking — {chars} chars (ctrl+p t to expand)"),
                    thinking_style,
                )));
            }
            all_lines.push(Line::from(""));
            continue;
        }

        let is_user = is_user_message(&msg.sender);

        let prev_sender = prev_conv_sender;

        // Close previous round when transitioning AI → user. Bottom rule
        // has the duration right-aligned with breathing space:
        // "──────── 1m ──"
        if is_user && prev_sender == Some("ai") {
            let last_ts = messages.get(idx - 1).and_then(|m| m.timestamp.clone());
            let elapsed = format_group_elapsed(&group_start_ts, &last_ts);
            if elapsed.is_empty() {
                let dashes = "─".repeat(width);
                all_lines.push(Line::from(Span::styled(dashes, dim_style)));
            } else {
                // "<dashes> <elapsed> ──" of exactly `width`: the fill is what
                // the label leaves over, so the two cannot disagree.
                let label = format!(" {elapsed} ──");
                let fill = width.saturating_sub(label.chars().count());
                all_lines.push(Line::styled(
                    format!("{}{label}", "─".repeat(fill)),
                    dim_style,
                ));
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
            if time_str.is_empty() {
                all_lines.push(Line::from(Span::styled("─".repeat(width), dim_style)));
            } else {
                // "── <time> <dashes>", same as the round-close rule above.
                let label = format!("── {time_str} ");
                let fill = width.saturating_sub(label.chars().count());
                all_lines.push(Line::styled(
                    format!("{label}{}", "─".repeat(fill)),
                    dim_style,
                ));
            }
        }

        // Between a human turn and the agent's reply: one blank row, no rule.
        // The block's own padding already separates them; the blank keeps the
        // reply from hugging the block. Keyed off the same human-side
        // predicate, so a piped channel's display name counts too.
        if !is_user && prev_sender.is_some_and(is_user_message) {
            all_lines.push(Line::from(""));
        }

        // Render message (no speaker label — the human side is identified by
        // its background below).
        let md_text = softbreaks_to_hardbreaks(&format!("{}\n", msg.text));
        let rendered =
            tui_markdown::from_str_with_options(&md_text, &chat_markdown_options()).lines;
        let mut msg_lines = wrap_styled_lines(wrap_tables(rendered, body_width), body_width);
        // The inset cell, one per row, both sides. A bare span is enough:
        // `Line::styled_graphemes` patches the row's own style onto every
        // grapheme it renders, so the block's background — or a code block's
        // shade — covers this cell too. Goes in before the pad is measured.
        for line in &mut msg_lines {
            line.spans.insert(0, Span::raw(" "));
        }
        if is_user {
            for line in &mut msg_lines {
                // A row that carries its own background (a fenced code block
                // when syntax highlighting is off) pads in that shade too, so
                // the row reads as one piece to the edge.
                let bg = line.style.bg.unwrap_or(USER_BG);
                // Patched *under* the line style so markdown's own colors
                // (a blockquote's green, a heading) survive on the block.
                line.style = user_style.patch(line.style);
                // `Paragraph` paints a line's style only where it has glyphs,
                // so the row is padded to read as one solid block.
                let pad = width.saturating_sub(line.width());
                if pad > 0 {
                    line.spans
                        .push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
                }
            }
            // One painted blank row above and below: the block needs breathing
            // room inside itself. The row is padded with spaces because
            // `Paragraph` only paints where there are glyphs.
            let pad_row =
                || Line::from(Span::styled(" ".repeat(width), user_style)).style(user_style);
            msg_lines.insert(0, pad_row());
            msg_lines.push(pad_row());
        }
        all_lines.extend(msg_lines);
        // Breath below a reply as well, mirroring the padding the human block
        // paints for itself: no message ends flush against the next row.
        if !is_user {
            all_lines.push(Line::from(""));
        }
        prev_conv_sender = Some(msg.sender.as_str());
    }

    // Close any open round at the end (same bottom-rule format as above).
    if group_start_ts.is_some() {
        let last_ts = messages.last().and_then(|m| m.timestamp.clone());
        let elapsed = format_group_elapsed(&group_start_ts, &last_ts);
        if elapsed.is_empty() {
            let dashes = "─".repeat(width);
            all_lines.push(Line::from(Span::styled(dashes, dim_style)));
        } else {
            let label = format!(" {elapsed} ──");
            let fill = width.saturating_sub(label.chars().count());
            all_lines.push(Line::styled(
                format!("{}{label}", "─".repeat(fill)),
                dim_style,
            ));
        }
        all_lines.push(Line::from(""));
    }

    all_lines
}

// ── Progress animation ──────────────────────────────────────────────────
//
// The live tail's spinner and the minimal progress line animate off the wall
// clock, so they need no timer of their own: the chat loop repaints at least
// every 50 ms, and the frame index is derived from `now_ms` — which also keeps
// these helpers pure and testable.

/// Braille spinner, one frame per [`SPINNER_STEP_MS`] — a full turn per second.
const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Spinner frame duration (10 fps).
const SPINNER_STEP_MS: i64 = 100;
/// Pulse ramp for the state word at the end of the line: dark gray → bright
/// yellow → back, one round trip per second. Named ANSI colors on purpose — an
/// RGB interpolation would look wrong on a terminal without truecolor.
const PULSE_PHASES: [Color; 4] = [
    Color::DarkGray,
    Color::Gray,
    Color::LightYellow,
    Color::Gray,
];
/// Pulse phase duration (4 × 250 ms = 1 s).
const PULSE_STEP_MS: i64 = 250;
/// What a tool row's rendered first line starts with (see
/// [`render_activity_entry`]).
const TOOL_PREFIX: &str = "Tool: ";

/// Wall-clock milliseconds, clamped at the epoch — a clock set before 1970
/// would otherwise index a frame list with a negative number.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis().max(0)
}

/// Index into a frame list from an absolute timestamp. Clamped at the epoch:
/// a clock set before 1970 would otherwise index the list with a negative
/// number and panic on the very first frame.
fn step_index(now_ms: i64, step_ms: i64, len: usize) -> usize {
    ((now_ms.max(0) / step_ms) % len as i64) as usize
}

/// Current spinner glyph.
fn spinner_frame(now_ms: i64) -> char {
    SPINNER_FRAMES[step_index(now_ms, SPINNER_STEP_MS, SPINNER_FRAMES.len())]
}

/// Current pulse style for the state word (keeps the italics the rest of the
/// tail is drawn with).
fn pulse_style(now_ms: i64) -> Style {
    Style::default()
        .fg(PULSE_PHASES[step_index(now_ms, PULSE_STEP_MS, PULSE_PHASES.len())])
        .add_modifier(Modifier::ITALIC)
}

/// The spinner column. Two trailing spaces, because the `⏳ ` / `💭 ` / `⚠ `
/// prefixes it replaces measure three columns and wrapped rows pad by their
/// width — one space would shift the text a column left.
fn spinner_prefix(now_ms: i64) -> String {
    format!("{}  ", spinner_frame(now_ms))
}

/// Where the tool name ends inside a line that already lost its [`TOOL_PREFIX`]:
/// before the ` — <input>` separator, or before a ` (done, 0s)` status marker if
/// one comes first.
fn tool_name_end(rest: &str) -> usize {
    let sep = rest.find(" — ").unwrap_or(rest.len());
    rest[..sep].find(" (").unwrap_or(sep)
}

/// The state word of an activity line: `Tool: bash (done, 0s) — {…}` → `bash`.
/// Anything that is not a tool row — the live thinking entries read
/// `Thinking... (iteration 14)` — reports as `thinking`.
fn activity_state(text: &str) -> &str {
    let name = match text.strip_prefix(TOOL_PREFIX) {
        Some(rest) => &rest[..tool_name_end(rest)],
        None => "",
    };
    if name.is_empty() { "thinking" } else { name }
}

/// An activity line broken into spans, pulsing only the state word:
/// `Tool: ` + **`bash`** + ` — {…}`. A line without a tool name (a thinking
/// row) pulses as a whole.
fn pulse_spans(line: &str, base: Style, now_ms: i64) -> Vec<Span<'static>> {
    let pulse = pulse_style(now_ms);
    let Some(rest) = line.strip_prefix(TOOL_PREFIX) else {
        return vec![Span::styled(line.to_string(), pulse)];
    };
    let end = TOOL_PREFIX.len() + tool_name_end(rest);
    let mut spans = vec![
        Span::styled(line[..TOOL_PREFIX.len()].to_string(), base),
        Span::styled(line[TOOL_PREFIX.len()..end].to_string(), pulse),
    ];
    if end < line.len() {
        spans.push(Span::styled(line[end..].to_string(), base));
    }
    spans
}

/// The entire progress tail in minimal mode: `⠹  12.4s · bash`.
///
/// The time is the live loop ticker (1 Hz), falling back to the current
/// entry's own age before the first tick arrives; with neither, the line is
/// just the spinner and the state word.
fn minimal_progress_line(
    last: Option<&jyc_types::ActivityEntry>,
    live_tick_ms: Option<u64>,
    now_ms: i64,
) -> Line<'static> {
    let timestamp = last.and_then(|entry| entry.timestamp.clone());
    let elapsed = match live_tick_ms {
        Some(ms) => format_elapsed_ms(ms),
        None => format_elapsed(&timestamp),
    };
    let dim = Style::default().fg(Color::Gray);
    let mut spans = vec![Span::raw("  "), Span::styled(spinner_prefix(now_ms), dim)];
    if !elapsed.is_empty() {
        spans.push(Span::styled(format!("{elapsed} · "), dim));
    }
    let state = last
        .map(|entry| activity_state(&entry.text))
        .unwrap_or("thinking");
    spans.push(Span::styled(state.to_string(), pulse_style(now_ms)));
    Line::from(spans)
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
    let input_line_count = if app.chat.active_question() {
        // Question box: `question_chrome_rows` (the question, the hint, the two
        // spacers, slack) + one row per option + the box's borders + the input's
        // mode header row — the box is drawn in the *body* under that header, so
        // a row missing here is a row the box loses, and the window would eat an
        // option for it. Both sides measure with the same helper.
        let q = app.chat.current_question().expect("active_question");
        // `questions.len()` because the hint says something different for a
        // batch, and a hint that wraps to one more row is a row the options
        // lose — both sides of this measurement have to read the same text.
        (question_chrome_rows(
            &q.question,
            app.chat.questions.len(),
            area.width.saturating_sub(PROMPT_GUTTER_WIDTH),
        ) + q.options.len()
            + 3)
        .clamp(6, 15) as u16
    } else {
        (count_wrapped_lines(
            &app.chat.text(),
            area.width.saturating_sub(PROMPT_GUTTER_WIDTH),
        ) + 1)
            .clamp(2, 11) as u16
    };
    // Rows reserved for an open popup, directly BELOW the input field: the
    // popup is a layout participant, not an overlay, so it can never be
    // clipped by the pane edge or cover the field the user is typing in.
    let popup_rows = match (app.chat.command_popup.as_ref(), app.chat.leader.as_ref()) {
        (Some(state), _) => crate::cli::command_popup::popup_height(state, &app.chat.commands),
        (None, Some(leader)) => leader.popup_height(area.width as usize),
        (None, None) => 0,
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(input_line_count),
            Constraint::Length(popup_rows),
        ])
        .split(area);
    // Cache the message-area rect so mouse-wheel events can hit-test
    // against it from the input loop.
    app.chat.last_message_area = Some(chunks[0]);
    // Register the message area as the OSC 8 link-scanning region: the
    // backend clips URL detection to pane columns, so neighbouring pane
    // text can never merge into a link target.
    app.link_regions.borrow_mut().push(chunks[0]);

    // --- Messages area (markdown-rendered) ---
    // tui-markdown renders without wrapping, so messages are word-wrapped
    // to the pane width here — the scroll math below counts one entry per
    // visual row.
    let messages_width = chunks[0].width as usize;

    // Message-history lines are cached: they only change when a message
    // arrives / streams / clears or the pane width changes. Without the
    // cache every frame (each keystroke, 50ms poll, 1Hz tick) re-parsed
    // the whole transcript's markdown — the per-keystroke input lag.
    let fingerprint = history_fingerprint(
        &app.chat.messages,
        messages_width,
        app.chat.thinking_expanded,
        app.chat.minimal_progress,
    );
    let cache_hit = matches!(&app.chat.render_cache, Some((fp, _)) if *fp == fingerprint);
    if !cache_hit {
        let lines = render_history_lines(
            &app.chat.messages,
            messages_width,
            app.chat.thinking_expanded,
            app.chat.minimal_progress,
        );
        app.chat.render_cache = Some((fingerprint, lines));
    }

    // Dynamic progress tail (thinking / activity / live ticker) — small,
    // rebuilt every frame, appended after the cached history lines.
    let mut tail_lines: Vec<Line> = Vec::new();

    // Show progress indicator
    // Determine if the topic is processing: prefer the live processing
    // status (updated via WS `processing` events), fall back to the polled
    // overview state, fall back to local `awaiting_response`.
    let live_processing = app
        .chat
        .channel
        .as_deref()
        .zip(app.chat.topic.as_deref())
        .and_then(|(c, t)| app.chat.live_processing_for(c, t));
    let server_processing = match live_processing {
        Some((p, _)) => p,
        None => app
            .state
            .as_ref()
            .and_then(|s| {
                let chat_name = app.chat.topic.as_deref()?;
                s.topics.iter().find(|t| t.name == chat_name)
            })
            .is_some_and(|ct| ct.status == TopicStatus::Processing),
    };

    // Show progress if the server reports processing OR we've sent a message
    // locally and are still waiting for the server state to catch up.
    let show_progress = server_processing || app.chat.awaiting_response;
    let has_error = live_processing
        .as_ref()
        .map(|(_, has_error)| *has_error)
        .unwrap_or(false);

    if show_progress {
        // Read live activity + thinking from the WS-fed buffers, falling
        // back to the polled overview only as a last resort. The rendering
        // logic below is byte-for-byte identical to before — only the data
        // source changed.
        let live_chan = app.chat.channel.clone();
        let live_topic = app.chat.topic.clone();
        // Minimal progress mode collapses the whole tail into one animated
        // line. The animation phase is read from the wall clock once per frame
        // (the loop repaints at least every 50 ms, so no timer is needed) —
        // one read keeps every animated row of the frame in step.
        let minimal = app.chat.minimal_progress;
        let now = now_ms();
        let activity_entries: Vec<jyc_types::ActivityEntry> = live_chan
            .as_deref()
            .zip(live_topic.as_deref())
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

        let thinking_blocks: Option<&[String]> = if minimal {
            None
        } else {
            live_chan
                .as_deref()
                .zip(live_topic.as_deref())
                .and_then(|(c, t)| app.chat.live_thinking_for(c, t))
        };

        // Live wall-clock ticker (1 Hz, with the first tick at t=0). When
        // present, it is the authoritative elapsed-time display for the
        // last in-progress line — the polled `last_active_at` it
        // normally shows goes stale during silent LLM/tool work. Falls
        // back to the polled value when no tick has arrived yet.
        let live_tick_ms: Option<u64> = live_chan
            .as_deref()
            .zip(live_topic.as_deref())
            .and_then(|(c, t)| app.chat.live_tick_ms_for(c, t));

        // Render thinking first (same wrap + indent as before). Blocks are
        // accumulated per turn (never overwritten); collapsed to a one-line
        // summary by default, expanded via the leader popup (`ctrl+p t`).
        // Thinking events are NOT stored in the activity buffer or
        // activity.jsonl.
        let has_thinking = matches!(thinking_blocks, Some(b) if !b.is_empty());
        if let Some(blocks) = thinking_blocks
            && !blocks.is_empty()
        {
            let gray_style = Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::ITALIC);
            // Hard-wrap long lines so nothing is clipped at the right edge.
            // The 2 accounts for the "  " indent prefix below; each wrapped
            // segment becomes its own `Line` so the scroll calculation sees
            // the correct visual row count.
            let avail = chunks[0].width.saturating_sub(2) as usize;
            if app.chat.thinking_expanded {
                let joined = blocks.join("\n\n");
                for line in wrap_text_to_width(&joined, avail) {
                    tail_lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(line, gray_style),
                    ]));
                }
            } else {
                let chars: usize = blocks.iter().map(|b| b.chars().count()).sum();
                push_tail_rows(
                    &mut tail_lines,
                    chunks[0].width,
                    "💭 ",
                    format!(
                        "thinking — {} blocks · {chars} chars (ctrl+p t)",
                        blocks.len()
                    ),
                    gray_style,
                );
            }
        }

        if minimal {
            tail_lines.push(minimal_progress_line(
                activity_entries.last(),
                live_tick_ms,
                now,
            ));
        } else if activity_entries.is_empty() && !has_thinking {
            // Pre-activity placeholder: shown only between ProcessingStarted
            // and the first ToolStarted/LLMRequestStarted event. There's
            // no "since-event" numerator to pair with, so we display the
            // live ticker alone in `(12.4s)` form. This intentionally
            // diverges from the dual-time format below — there's literally
            // no `a.timestamp` to compute the left half from.
            //
            // No tool name to pulse here, so the whole state sentence takes
            // the pulse, and the spinner replaces the static `⏳`.
            let body = match live_tick_ms {
                Some(ms) => format!("AI is thinking... ({})", format_elapsed_ms(ms)),
                None => "AI is thinking...".to_string(),
            };
            tail_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(spinner_prefix(now), Style::default().fg(Color::Gray)),
                Span::styled(body, pulse_style(now)),
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

                // Split multi-line entries (e.g. edit diff) into separate lines
                // — edit/write JSON events carry their own data; the helper
                // unifies the `ctrl+p T` toggle across every tool.
                //
                // Diff-specific styling (`-` gray, `+` green) is decided here
                // on the *unpadded* `line`, before the caller pads it with
                // the marker / `"   "` — checking the padded label would
                // always miss because the marker sits two columns in.
                let spinner = spinner_prefix(now);
                let rendered_lines: Vec<(String, Vec<Span<'static>>)> =
                    render_activity_entry(&a.text, app.chat.tool_detail_expanded)
                        .into_iter()
                        .enumerate()
                        .map(|(line_idx, line)| {
                            // The row prefix stays out of the text so a wrapped
                            // row can carry the same pad and line up under the
                            // first one.
                            let current = line_idx == 0 && is_last;
                            let prefix = if current {
                                spinner.clone()
                            } else {
                                // Pad with 3 spaces to visually align with the
                                // spinner / "⏳ " column.
                                "   ".to_string()
                            };
                            let text = if current && !elapsed.is_empty() {
                                format!("{line} {elapsed}")
                            } else {
                                line
                            };
                            let label_style = style_diff_line(&text, style);
                            // The current row animates: the spinner turns and
                            // the state word pulses. Diff-colored rows keep
                            // their own color — pulsing a `+`/`-` line would
                            // throw away the only color that means anything.
                            let spans = if current && label_style == style {
                                pulse_spans(&text, label_style, now)
                            } else {
                                vec![Span::styled(text.clone(), label_style)]
                            };
                            (prefix, spans)
                        })
                        .collect();

                for (prefix, spans) in rendered_lines {
                    push_tail_rows_spans(&mut tail_lines, chunks[0].width, &prefix, spans);
                }
            }
        }
    } else if has_error {
        // Processing completed with an error. Show a persistent red warning
        // until the next round starts (has_error is cleared by a new
        // ProcessingStarted event).
        let live_chan = app.chat.channel.clone();
        let live_topic = app.chat.topic.clone();
        let error_text = live_chan
            .as_deref()
            .zip(live_topic.as_deref())
            .and_then(|(c, t)| {
                app.chat
                    .live_activity_for(c, t)
                    .rev()
                    .find(|e| {
                        matches!(e.severity, jyc_types::Severity::Error)
                            && e.text.starts_with("ERROR:")
                    })
                    .map(|e| e.text.clone())
            })
            .unwrap_or_else(|| "Processing failed".to_string());
        let message = error_text
            .strip_prefix("ERROR: ")
            .unwrap_or(&error_text)
            .chars()
            .take(160)
            .collect::<String>();
        push_tail_rows(
            &mut tail_lines,
            chunks[0].width,
            "⚠ ",
            message,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        );
    }

    let inner_height = chunks[0].height as usize;
    // Borrow the cached history lines without cloning; scroll math below
    // only touches disjoint fields (`last_max_scroll`, `scroll`).
    let history: &[Line] = &app
        .chat
        .render_cache
        .as_ref()
        .expect("render cache stored above")
        .1;
    let history_len = history.len();
    let total_lines = history_len + tail_lines.len();
    let max_skip = total_lines.saturating_sub(inner_height);
    app.chat.last_max_scroll = max_skip;
    // The cursor's range and the viewport height are only known here, so the
    // geometry the keys move against is always last frame's.
    app.chat.last_total_lines = total_lines;
    app.chat.scroll = app.chat.scroll.min(max_skip);
    if app.chat.cursor_line == usize::MAX {
        // Never placed: resolving the sentinel against the empty transcript of a
        // topic switch would park it on row 0 for the session, so it waits for
        // content and lands on the last row with text — not the blank spacer that
        // closes a reply, nor the pending rows after the history.
        app.chat.cursor_line = history
            .iter()
            .rposition(|line| line.spans.iter().any(|s| !s.content.trim().is_empty()))
            .unwrap_or(usize::MAX);
    } else {
        // Clamp against *this* frame too: switching topics or a resize that
        // shortened the transcript must not leave the cursor past the end.
        app.chat.cursor_line = app.chat.cursor_line.min(total_lines.saturating_sub(1));
    }
    // The selection belongs to the message pane alone. Focus can leave it by
    // routes that do not go through `refocus_input` (`Tab`, a click, the
    // explorer), and a selection left behind is invisible yet still live: it
    // would pin the cursor against `carry_cursor` and could be yanked later.
    if app.chat.focus != ChatFocus::MessageArea {
        app.chat.selection_anchor = None;
    }
    let skip = max_skip.saturating_sub(app.chat.scroll);
    // Clone only the visible window (≤ inner_height lines) — the
    // Paragraph clips anything beyond the area anyway.
    let hist_slice: &[Line] = if skip < history_len {
        &history[skip..]
    } else {
        &[]
    };
    let tail_start = skip.saturating_sub(history_len);
    let mut visible_lines: Vec<Line> = hist_slice
        .iter()
        .chain(&tail_lines[tail_start..])
        .take(inner_height)
        .cloned()
        .collect();
    // The highlight belongs to the message pane alone: while any other pane has
    // focus the transcript is plain text. One transcript line is exactly one
    // screen row here (the markdown renderer wraps to the pane width and the
    // `Paragraph` does not wrap again), hence the direct line -> row mapping.
    // The cursor row and the selected rows share the one background — how many
    // rows are selected is in the status line, so a second colour would be noise.
    let cursor = app.chat.cursor_line;
    let selection = app.chat.selection_range();
    if app.chat.focus == ChatFocus::MessageArea {
        let bar = Style::default().bg(SELECT_BG);
        let selected_bar = bar.fg(Color::White);
        for (row, line) in visible_lines.iter_mut().enumerate() {
            let line_no = skip + row;
            if selection.is_some_and(|(from, to)| line_no >= from && line_no <= to) {
                paint_row(line, chunks[0].width, selected_bar);
            } else if line_no == cursor {
                paint_row(line, chunks[0].width, bar);
            }
        }
    }

    let messages_para = Paragraph::new(visible_lines);
    frame.render_widget(messages_para, chunks[0]);

    // --- Input area (text editor, at bottom) ---
    // The editor renders its own wrapping and scroll-follow. A two-line
    // prompt gutter sits left of the editor: the header row shows
    // "╭─ {mode} · {channel} · {pattern}[ · {branch}]", and "╰─❯" on the
    // first editor row; both dim when the input field loses focus.
    // The cursor is a blinking underline when the input has focus and
    // invisible when another pane does (a default-styled cursor cell is
    // indistinguishable from the text under it). While a question is
    // pending the editor is covered by the question box, so the cursor
    // stays hidden.
    app.chat
        .editor
        .set_cursor_style(if app.chat.active_question() {
            Style::default()
        } else {
            match app.chat.focus {
                ChatFocus::ChatPane => Style::default()
                    .add_modifier(Modifier::UNDERLINED)
                    .add_modifier(Modifier::SLOW_BLINK),
                ChatFocus::MessageArea
                | ChatFocus::ActivityPane
                | ChatFocus::ExplorerPane
                | ChatFocus::InfoPane => Style::default(),
            }
        });
    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(chunks[1]);
    let [prompt_area, editor_area] =
        Layout::horizontal([Constraint::Length(PROMPT_GUTTER_WIDTH), Constraint::Min(0)])
            .areas(body_area);
    let focused = app.chat.focus == ChatFocus::ChatPane;
    // Resolve mode/channel/pattern/model/tokens for the header line, all
    // from the polled overview (same source as the Topic Info pane).
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
    // Prompt arrow: "╰─❯ ". The box-drawing prefix uses the
    // focus-dependent line style; the arrow is yellow when focused and
    // dims to #393552 when not.
    let arrow_style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        LINE_DRAWING
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("╰─", line_style),
            Span::styled("❯ ", arrow_style),
        ])),
        prompt_area,
    );
    frame.render_widget(&app.chat.editor, editor_area);
    if app.chat.active_question() {
        super::render_question_box(frame, editor_area, app);
    }

    // ── Popups: the slot right below the input field (chunks[2]) ──
    if let Some(ref popup) = app.chat.command_popup {
        render_command_popup(frame, chunks[2], popup, &app.chat.commands);
    }

    // ── Leader-key popup (TUI-local commands) ──
    if let Some(ref leader) = app.chat.leader {
        leader.render_anchored(frame, chunks[2]);
    }
}

/// Reformat an activity line of the form `Tool: <name> — <json input>`
/// (optionally `Tool: <name> (done, Xs) — <json>`) into a readable summary
/// using the shared extractor, e.g. `Tool: bash — ls -la`. Extraction
/// happens here — at render time — so the WS payload and `activity.jsonl`
/// keep the raw JSON. Lines that don't match, or whose input can't be
/// summarized, pass through unchanged.
/// Render the raw text of one activity entry into a list of display
/// lines. The caller applies the per-line padding — the spinner on the
/// entry being worked on, three blank columns elsewhere — based on
/// `is_last` + `elapsed`.
///
/// Edit/write events arrive as bare JSON (`{"type":"edit",...}`) carrying
/// `old_string` / `new_string` / `content` for a diff view. That diff is
/// shown only when `tool_detail_expanded` (the `ctrl+p T` toggle) is on;
/// otherwise the entry collapses to the same one-line
/// `Tool: <name> — <basename>` form every other tool uses. Putting edit
/// and write under the same toggle unifies the "tool details" pane — no
/// more independent always-on rendering for file-write tools.
fn render_activity_entry(text: &str, tool_detail_expanded: bool) -> Vec<String> {
    let parsed = serde_json::from_str::<serde_json::Value>(text).ok();
    // Take ownership of the parsed value so `typ` can borrow from it
    // without conflicting with later moves.
    let json = parsed.unwrap_or_default();
    let typ = json.get("type").and_then(|t| t.as_str());

    if matches!(typ, Some("edit") | Some("write")) {
        let tool_name = typ.unwrap_or("?");
        if tool_detail_expanded {
            render_file_tool_diff(tool_name, &json)
        } else {
            let file_path = json
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let basename = file_path.rsplit('/').next().unwrap_or(file_path);
            let line_no = json.get("line_no").and_then(|v| v.as_u64());
            let label = match line_no {
                Some(n) => format!("{basename}:{n}"),
                None => basename.to_string(),
            };
            vec![format!("Tool: {tool_name} — {label}")]
        }
    } else {
        // Standard `Tool: <name> — <json>` path.
        let mut lines: Vec<String> = Vec::new();
        for (i, line) in text.split('\n').enumerate() {
            if i == 0 {
                // Expanded renders this same line with the summary cap lifted,
                // so the field listing below only adds the secondary arguments.
                lines.push(reformat_tool_line(line, tool_detail_expanded));
                if tool_detail_expanded {
                    lines.extend(format_tool_input_full(line));
                }
            } else {
                lines.push(line.to_string());
            }
        }
        lines
    }
}

/// Rich diff view for an `edit` or `write` JSON event. Lines are bare
/// content (no marker prefix) — the caller applies per-line padding.
fn render_file_tool_diff(tool_name: &str, json: &serde_json::Value) -> Vec<String> {
    let file_path = json
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let line_no = json.get("line_no").and_then(|v| v.as_u64());
    let location = match line_no {
        Some(n) => format!("{file_path}:{n}"),
        None => file_path.to_string(),
    };
    let mut out = vec![location];
    match tool_name {
        "edit" => {
            let old_str = json
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new_str = json
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            for line in old_str.split('\n') {
                out.push(format!("{DIFF_REMOVED_PREFIX}{line}"));
            }
            for line in new_str.split('\n') {
                out.push(format!("{DIFF_ADDED_PREFIX}{line}"));
            }
        }
        "write" => {
            let content = json.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let content_lines: Vec<&str> = content.split('\n').collect();
            for line in content_lines.iter().take(TOOL_DETAIL_MAX_LINES) {
                out.push(format!("{DIFF_ADDED_PREFIX}{line}"));
            }
            if content_lines.len() > TOOL_DETAIL_MAX_LINES {
                out.push(format!(
                    "  … ({} more lines)",
                    content_lines.len() - TOOL_DETAIL_MAX_LINES
                ));
            }
        }
        _ => {}
    }
    out
}

/// Style one unpadded diff line. The caller pads with the marker
/// (`⠹  `) / `"   "` later, so the prefix check runs on the **unpadded**
/// input — checking the padded label would always miss because the
/// prefix sits two characters in (`"⠹  -..."` or `"   -..."`), not at
/// column 0.
///
/// Returns `default` for the header (`<file>:<line>`), truncation
/// marker (`… (N more lines)`), and the empty case, so the rest of the
/// progress tail keeps its yellow-italic styling.
/// Push one progress-tail entry as transcript rows, wrapped to the message
/// pane's width.
///
/// The transcript `Paragraph` deliberately has no `.wrap()` — one line is
/// exactly one screen row, which is what the scroll, cursor and selection maths
/// count — so a row wider than the pane has to be broken up here, or everything
/// past its right edge is unreachable. `prefix` marks the entry (the
/// spinner, "💭 ", "⚠ "); every wrapped row is padded by the prefix's own
/// display width, so a tall block still reads as one entry rather than a ragged
/// column. The two-column base indent is added here as well.
fn push_tail_rows(
    out: &mut Vec<Line<'static>>,
    pane_width: u16,
    prefix: &str,
    text: String,
    style: Style,
) {
    push_tail_rows_spans(out, pane_width, prefix, vec![Span::styled(text, style)]);
}

/// [`push_tail_rows`] with the row already split into styled spans — lets the
/// current activity row pulse just its state word, and lets
/// [`wrap_styled_lines`] handle the wrapping (it rebuilds spans from the
/// per-character styles, so a multi-span row wraps without losing either color).
fn push_tail_rows_spans(
    out: &mut Vec<Line<'static>>,
    pane_width: u16,
    prefix: &str,
    spans: Vec<Span<'static>>,
) {
    use unicode_width::UnicodeWidthStr;
    let pad = prefix.width();
    let mut rows = wrap_styled_lines(
        vec![Line::from(spans)],
        (pane_width as usize).saturating_sub(2 + pad),
    );
    if rows.is_empty() {
        // An empty entry still owns a row — `wrap_styled_lines` returns
        // nothing for it, and dropping the row would shift the tail.
        rows.push(Line::default());
    }
    let blank = " ".repeat(pad);
    for (i, row) in rows.into_iter().enumerate() {
        let marker = if i == 0 {
            prefix.to_string()
        } else {
            blank.clone()
        };
        let mut spans = vec![Span::raw("  "), Span::raw(marker)];
        spans.extend(row.spans);
        out.push(Line::from(spans));
    }
}

fn style_diff_line(unpadded: &str, default: Style) -> Style {
    if unpadded.starts_with(DIFF_REMOVED_PREFIX) {
        Style::default().fg(Color::Gray)
    } else if unpadded.starts_with(DIFF_ADDED_PREFIX) {
        Style::default().fg(Color::Green)
    } else {
        default
    }
}

/// Width the collapsed tool row caps the value to (CJK counts two, `…` appended
/// by [`truncate_to_width`]); `ctrl+p T` is uncapped, #811 wraps it.
const TOOL_SUMMARY_MAX_WIDTH: usize = 160;

/// A tool row's text: the extracted activity in place of the raw JSON input.
/// `expanded` is the `ctrl+p T` detail, which drops this row's cap so a long
/// `bash` command is readable in place instead of repeating its truncated
/// prefix in the field listing underneath.
fn reformat_tool_line(text: &str, expanded: bool) -> String {
    let Some(rest) = text.strip_prefix("Tool: ") else {
        return text.to_string();
    };
    let Some((prefix, input)) = rest.split_once(" — ") else {
        return text.to_string();
    };
    // `prefix` is either the bare tool name ("bash") or the completion
    // variant ("bash (done, 3s)") — the extractor needs the name only.
    let name = prefix.split(" (").next().unwrap_or(prefix);
    let Some(value) = jyc_types::inspect::tool_activity(name, Some(input)) else {
        return text.to_string();
    };
    // Capping is this row's decision, not the extractor's (#812).
    let value = if expanded {
        value
    } else {
        truncate_to_width(&value, TOOL_SUMMARY_MAX_WIDTH)
    };
    format!("Tool: {prefix} — {value}")
}

/// Render the raw JSON input of a `Tool: <name> — <json>` activity line as
/// an indented multi-line field listing (`  key: value`), for the expanded
/// tool-detail view (`ctrl+p T`). Multi-line string values keep their
/// newlines with indented continuation lines; nested values stay on one
/// line in compact form. Capped at 20 lines with a trailing
/// `… (N more lines)` marker — same convention as the edit-diff renderer.
/// Returns empty for non-tool lines or unparseable input.
/// Per-tool list of field names whose values already appear in the tool row
/// above (rendered by [`reformat_tool_line`]) — skipping them in
/// `format_tool_input_full` prevents a redundant line appearing below a row
/// that already inlines the value.
///
/// When the extractor reads either of two alternate keys for a tool (e.g.
/// `read_image` accepts `file_path` *or* `path`), list both — the skip is a
/// set, not a single key.
fn primary_field_keys(tool_name: &str) -> &'static [&'static str] {
    match tool_name {
        "bash" => &["command"],
        "read" | "edit" | "write" => &["file_path"],
        "read_image" => &["path", "file_path"],
        "grep" | "glob" => &["pattern"],
        "webfetch" => &["url"],
        _ => &[],
    }
}

fn format_tool_input_full(text: &str) -> Vec<String> {
    let Some((prefix, input)) = text
        .strip_prefix("Tool: ")
        .and_then(|rest| rest.split_once(" — "))
    else {
        return Vec::new();
    };
    // `prefix` is either the bare tool name or the completion variant
    // ("bash (done, 3s)") — primary keys are keyed on the bare name.
    let name = prefix.split(" (").next().unwrap_or(prefix);
    let skip = primary_field_keys(name);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        return Vec::new();
    };
    let Some(obj) = value.as_object() else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for (key, val) in obj {
        // The summary line above inlines the primary field, so listing it again
        // here would only repeat it; expanding lifts that line's cap instead
        // (see `reformat_tool_line`).
        if skip.contains(&key.as_str()) {
            continue;
        }
        match val {
            serde_json::Value::String(s) => {
                let mut parts = s.split('\n');
                out.push(format!("  {key}: {}", parts.next().unwrap_or("")));
                out.extend(parts.map(|l| format!("    {l}")));
            }
            other => out.push(format!("  {key}: {other}")),
        }
    }
    if out.len() > TOOL_DETAIL_MAX_LINES {
        let more = out.len() - TOOL_DETAIL_MAX_LINES;
        out.truncate(TOOL_DETAIL_MAX_LINES);
        out.push(format!("  … ({more} more lines)"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_pane_user_is_human_side() {
        assert!(is_user_message("user"));
    }

    #[test]
    fn piped_channel_sender_is_human_side() {
        // A feishu-piped message carries the remote user's display name (or
        // open_id), not "user" — it must still be the human side, not "AI:".
        assert!(is_user_message("金晔"));
        assert!(is_user_message("ou_c36ae8bf58a1d727fffd2289467fefce"));
    }

    #[test]
    fn agent_reply_is_ai_side() {
        assert!(!is_user_message("ai"));
    }

    #[test]
    fn reformat_tool_line_extracts_started_and_completed() {
        assert_eq!(
            reformat_tool_line(r#"Tool: bash — {"command": "ls -la"}"#, false),
            "Tool: bash — ls -la"
        );
        assert_eq!(
            reformat_tool_line(r#"Tool: bash (done, 3s) — {"command": "ls -la"}"#, false),
            "Tool: bash (done, 3s) — ls -la"
        );
        assert_eq!(
            reformat_tool_line(
                r#"Tool: edit — {"file_path": "/home/jiny/projects/jyc/src/tools.rs"}"#,
                false,
            ),
            "Tool: edit — tools.rs"
        );
    }

    #[test]
    fn reformat_tool_line_passthrough() {
        // Unknown tool / unparseable input: show the line unchanged — the
        // raw JSON still carries information in the activity view.
        let unknown = r#"Tool: context_browse — {"offset": 0}"#;
        assert_eq!(reformat_tool_line(unknown, false), unknown);
        let bad_json = "Tool: bash — not json";
        assert_eq!(reformat_tool_line(bad_json, false), bad_json);
        // Non-tool lines are untouched.
        let other = "Thinking... (iteration 2)";
        assert_eq!(reformat_tool_line(other, false), other);
    }

    #[test]
    fn format_tool_input_full_lists_fields_multiline() {
        // `batch` has no primary field — every JSON key expands. Switching
        // the example tool away from `edit` keeps the test focused on the
        // multi-line field rendering, not on primary-key skipping (which
        // has its own test below).
        let lines = format_tool_input_full(
            r#"Tool: batch — {"file_path": "src/tools.rs", "old_string": "fn a() {\n}", "new_string": "fn b() {}"}"#,
        );
        // serde_json without `preserve_order` stores objects as BTreeMap,
        // so fields render in alphabetical order.
        assert_eq!(
            lines,
            vec![
                "  file_path: src/tools.rs",
                "  new_string: fn b() {}",
                "  old_string: fn a() {",
                "    }",
            ]
        );
    }

    #[test]
    fn format_tool_input_full_skips_primary_field_for_bash() {
        let lines = format_tool_input_full(r#"Tool: bash — {"command": "ls -la"}"#);
        assert!(lines.is_empty(), "got {lines:?}");
    }

    #[test]
    fn expanded_tool_line_lifts_the_summary_cap_without_repeating_it() {
        // The reported shape — a chained grep past the cap. Expanding has to
        // make the command readable *in the line itself*; what it must not do
        // is show the truncated prefix and then repeat it in the listing.
        let cmd = r#"grep -rn "push_tail_rows" crates/jyc-cli/src/ --include=*.rs --exclude-dir=tests --exclude-dir=target --exclude-dir=.git --exclude=*.md --exclude=*.json --exclude=*.lock -A 2 | head -5"#;
        assert!(
            cmd.chars().count() > TOOL_SUMMARY_MAX_WIDTH,
            "fixture must exceed the row cap"
        );
        let line = format!(
            "Tool: bash (done, 0s) — {}",
            serde_json::json!({ "command": cmd })
        );

        let collapsed = reformat_tool_line(&line, false);
        assert!(
            collapsed.ends_with('…'),
            "collapsed keeps the cap: {collapsed}"
        );

        let expanded = reformat_tool_line(&line, true);
        assert!(
            expanded.contains(cmd),
            "expanded shows the whole command: {expanded}"
        );
        assert!(!expanded.contains('…'), "nothing cut off");
        let listing = format_tool_input_full(&line);
        assert!(
            listing.is_empty(),
            "the listing must not repeat it: {listing:?}"
        );

        // A value that fits is identical either way.
        let short = r#"Tool: bash (done, 3s) — {"command": "ls -la"}"#;
        assert_eq!(
            reformat_tool_line(short, false),
            reformat_tool_line(short, true)
        );
    }

    #[test]
    fn format_tool_input_full_keeps_secondary_fields_for_read() {
        // `read`'s primary field is `file_path` — only `offset` + `limit`
        // should survive the skip.
        let lines = format_tool_input_full(
            r#"Tool: read — {"file_path": "x.rs", "offset": 10, "limit": 5}"#,
        );
        assert_eq!(lines, vec!["  limit: 5", "  offset: 10"]);
    }

    #[test]
    fn format_tool_input_full_skips_primary_field_for_completion_variant() {
        // "bash (done, 3s)" — `primary_field_keys` only sees the bare name.
        let lines = format_tool_input_full(r#"Tool: bash (done, 3s) — {"command": "ls"}"#);
        assert!(lines.is_empty(), "got {lines:?}");
    }

    #[test]
    fn format_tool_input_full_skips_primary_field_for_read_image() {
        // `read_image` names its argument `path`, not `file_path` (see
        // `inspect.rs` `tool_activity_extracts_basename_for_file_tools`).
        // Either key must be skipped when listed in `primary_field_keys`.
        let lines_path = format_tool_input_full(r#"Tool: read_image — {"path": "/tmp/x.png"}"#);
        assert!(lines_path.is_empty(), "got {lines_path:?}");
        let lines_file_path =
            format_tool_input_full(r#"Tool: read_image — {"file_path": "/tmp/x.png"}"#);
        assert!(lines_file_path.is_empty(), "got {lines_file_path:?}");
    }

    #[test]
    fn format_tool_input_full_keeps_nested_values_compact() {
        let lines = format_tool_input_full(r#"Tool: batch — {"items": [1, 2], "n": 3}"#);
        assert_eq!(lines, vec!["  items: [1,2]", "  n: 3"]);
    }

    #[test]
    fn format_tool_input_full_empty_for_non_tool_or_bad_input() {
        assert!(format_tool_input_full("user typed something").is_empty());
        assert!(format_tool_input_full("Tool: bash — {bad json").is_empty());
        assert!(format_tool_input_full(r#"Tool: bash — "just a string""#).is_empty());
    }

    #[test]
    fn format_tool_input_full_caps_at_max_lines() {
        let long = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\\n");
        let input = format!(r#"Tool: write — {{"content": "{long}"}}"#);
        let lines = format_tool_input_full(&input);
        assert_eq!(lines.len(), 21);
        assert_eq!(lines[20], "  … (10 more lines)");
    }

    // `render_activity_entry` — `ctrl+p T` toggle behavior for edit/write.

    fn edit_event(file_path: &str, line_no: Option<u64>, old: &str, new: &str) -> String {
        serde_json::json!({
            "type": "edit",
            "file_path": file_path,
            "line_no": line_no,
            "old_string": old,
            "new_string": new,
        })
        .to_string()
    }

    fn write_event(file_path: &str, content: &str) -> String {
        serde_json::json!({
            "type": "write",
            "file_path": file_path,
            "content": content,
        })
        .to_string()
    }

    #[test]
    fn render_activity_entry_edit_on_detail_shows_diff() {
        let text = edit_event("src/foo.rs", Some(42), "old\n", "new\n");
        let lines = render_activity_entry(&text, true);
        assert_eq!(
            lines,
            vec!["src/foo.rs:42", "  -old", "  -", "  +new", "  +"]
        );
    }

    #[test]
    fn render_activity_entry_edit_off_detail_collapses_to_oneline() {
        let text = edit_event("src/foo.rs", Some(42), "old", "new");
        let lines = render_activity_entry(&text, false);
        assert_eq!(lines, vec!["Tool: edit — foo.rs:42"]);
    }

    #[test]
    fn render_activity_entry_edit_off_detail_omits_line_no_when_absent() {
        let text = edit_event("src/foo.rs", None, "old", "new");
        let lines = render_activity_entry(&text, false);
        assert_eq!(lines, vec!["Tool: edit — foo.rs"]);
    }

    #[test]
    fn render_activity_entry_write_on_detail_truncates_content() {
        let long: Vec<String> = (0..30).map(|i| format!("line{i}")).collect();
        let text = write_event("src/foo.rs", &long.join("\n"));
        let lines = render_activity_entry(&text, true);
        assert_eq!(lines.len(), 1 + 20 + 1); // header + 20 content lines + ellipsis
        assert_eq!(lines[0], "src/foo.rs");
        assert!(lines.last().unwrap().starts_with("  …"));
    }

    #[test]
    fn render_activity_entry_write_off_detail_collapses_to_oneline() {
        let text = write_event("src/foo.rs", "anything");
        let lines = render_activity_entry(&text, false);
        assert_eq!(lines, vec!["Tool: write — foo.rs"]);
    }

    #[test]
    fn render_activity_entry_bash_on_detail_still_uses_standard_path() {
        // Non-edit/write JSON: the standard `Tool: <name> — <summary>`
        // path runs, with `format_tool_input_full` skipping `command`.
        let text = r#"Tool: bash — {"command": "ls -la", "timeout": 5000}"#;
        let lines = render_activity_entry(text, true);
        // Summary on first line; only `timeout` survives the skip.
        assert_eq!(lines, vec!["Tool: bash — ls -la", "  timeout: 5000"]);
    }

    /// The minus line produced by `render_file_tool_diff` must round-trip
    /// through `style_diff_line` and come back gray — this is the exact
    /// regression that was missed when the prefix check moved after the
    /// caller padded the line with `"⏳ "` / `"   "`. Goes through the full
    /// `render_activity_entry` path so the wire format and the helper stay
    /// wired to the same JSON shape used by the rest of the test module.
    #[test]
    fn style_diff_line_minus_emitted_by_render_file_tool_diff_is_gray() {
        let text = edit_event("src/foo.rs", None, "removed line", "added line");
        let lines = render_activity_entry(&text, true);
        // First line is the file header (default style); second is the
        // `-` removed line (must be gray); third is the `+` added line.
        assert_eq!(lines[0], "src/foo.rs");
        assert!(lines[1].starts_with(DIFF_REMOVED_PREFIX));
        assert!(lines[2].starts_with(DIFF_ADDED_PREFIX));

        let default = Style::default().fg(Color::Yellow);
        assert_eq!(
            style_diff_line(&lines[1], default).fg,
            Some(Color::Gray),
            "- line must be gray, not default"
        );
        assert_eq!(
            style_diff_line(&lines[2], default).fg,
            Some(Color::Green),
            "+ line must be green, not default"
        );
    }

    #[test]
    fn style_diff_line_plus_uses_green() {
        let default = Style::default().fg(Color::Yellow);
        let s = style_diff_line("  +added line", default);
        assert_eq!(s.fg, Some(Color::Green));
    }

    #[test]
    fn style_diff_line_minus_uses_gray() {
        let default = Style::default().fg(Color::Yellow);
        let s = style_diff_line("  -removed line", default);
        assert_eq!(s.fg, Some(Color::Gray));
    }

    #[test]
    fn style_diff_line_header_uses_default() {
        // File header like `src/foo.rs:42` has no diff prefix and must
        // fall through to the caller's default style.
        let default = Style::default().fg(Color::Yellow);
        let s = style_diff_line("src/foo.rs:42", default);
        assert_eq!(s, default);
    }

    #[test]
    fn style_diff_line_truncation_marker_uses_default() {
        // The `… (N more lines)` marker is emitted by `render_file_tool_diff`
        // for truncated write content; it has no diff prefix and must keep
        // the default style.
        let default = Style::default().fg(Color::Yellow);
        let s = style_diff_line("  … (5 more lines)", default);
        assert_eq!(s, default);
    }

    // `push_tail_rows` is what every progress-tail row goes through, including
    // the two that carry a marker of their own width.
    #[test]
    fn push_tail_rows_pads_continuations_by_the_prefix_width() {
        let mut out: Vec<Line<'static>> = Vec::new();
        for (prefix, pad) in [("⏳ ", 3), ("⚠ ", 2)] {
            out.clear();
            push_tail_rows(&mut out, 40, prefix, "w".repeat(60), Style::default());
            assert!(out.len() > 1, "60 columns cannot fit a 40-column pane");
            assert_eq!(&*out[0].spans[1].content, prefix);
            let blank = " ".repeat(pad);
            for row in &out[1..] {
                // The pad follows the prefix's own display width, so a narrow
                // marker does not shove its continuation rows right.
                assert_eq!(&*row.spans[1].content, blank, "{row:?}");
            }
        }
    }

    #[test]
    fn push_tail_rows_gives_an_empty_entry_one_row() {
        let mut out: Vec<Line<'static>> = Vec::new();
        push_tail_rows(&mut out, 40, "💭 ", String::new(), Style::default());
        assert_eq!(out.len(), 1, "an entry always owns a row: {out:?}");
    }

    // ── Progress animation ────────────────────────────────────────────────

    #[test]
    fn spinner_frame_turns_once_a_second() {
        assert_eq!(spinner_frame(0), '⠋');
        assert_eq!(spinner_frame(99), '⠋', "a frame lasts 100 ms");
        assert_eq!(spinner_frame(200), '⠹');
        assert_eq!(spinner_frame(900), '⠏');
        assert_eq!(spinner_frame(1000), SPINNER_FRAMES[0], "the list wraps");
        assert_eq!(
            spinner_frame(-150),
            '⠋',
            "a clock before the epoch is clamped"
        );
    }

    #[test]
    fn pulse_ramp_walks_gray_to_yellow_and_back() {
        let fg = |ms| pulse_style(ms).fg.unwrap();
        assert_eq!(fg(0), Color::DarkGray);
        assert_eq!(fg(250), Color::Gray);
        assert_eq!(fg(500), Color::LightYellow);
        assert_eq!(fg(750), Color::Gray);
        assert_eq!(fg(1000), Color::DarkGray, "one round trip per second");
        assert!(
            pulse_style(0).add_modifier.contains(Modifier::ITALIC),
            "keeps the italics the rest of the tail is drawn with"
        );
    }

    #[test]
    fn spinner_prefix_holds_the_three_column_marker() {
        use unicode_width::UnicodeWidthStr;
        // `⏳ ` measures three columns and wrapped rows pad by that width; the
        // single-column braille dot would otherwise shove the text left.
        assert_eq!(spinner_prefix(0).width(), 3, "{:?}", spinner_prefix(0));
    }

    #[test]
    fn activity_state_reads_the_tool_name_out_of_a_live_row() {
        for (text, want) in [
            ("Tool: bash — {\"command\":\"cargo check\"}", "bash"),
            ("Tool: bash (done, 0s) — {\"command\":\"ls\"}", "bash"),
            ("Tool: edit (failed) — old_string not found", "edit"),
            ("Tool: read — render.rs:441", "read"),
            ("Thinking... (iteration 14)", "thinking"),
            ("", "thinking"),
            ("   ", "thinking"),
        ] {
            assert_eq!(activity_state(text), want, "{text:?}");
        }
    }

    #[test]
    fn pulse_spans_splits_a_tool_row_around_its_name() {
        let base = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::ITALIC);
        let spans = pulse_spans("Tool: bash — {\"command\":\"ls\"}", base, 500);
        let texts: Vec<&str> = spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert_eq!(texts, vec!["Tool: ", "bash", " — {\"command\":\"ls\"}"]);
        assert_eq!(
            spans[1].style.fg,
            Some(Color::LightYellow),
            "the name pulses"
        );
        assert_eq!(spans[0].style, base, "the label stays put");
        assert_eq!(spans[2].style, base, "so does the input");
    }

    #[test]
    fn pulse_spans_pulses_a_line_without_a_tool_name_whole() {
        let spans = pulse_spans("Thinking... (iteration 14)", Style::default(), 0);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "Thinking... (iteration 14)");
        assert_eq!(spans[0].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn pulse_spans_cuts_the_name_before_a_status_marker() {
        let spans = pulse_spans("Tool: bash (done, 0s) — ok", Style::default(), 0);
        assert_eq!(spans[1].content.as_ref(), "bash");
        assert_eq!(spans[2].content.as_ref(), " (done, 0s) — ok");
    }

    #[test]
    fn minimal_progress_line_is_one_animated_row() {
        let entry = jyc_types::ActivityEntry {
            timestamp: Some("2026-08-13T10:00:00Z".to_string()),
            severity: jyc_types::Severity::Info,
            text: "Tool: bash — {\"command\":\"ls\"}".to_string(),
            id: 0,
            is_internal: false,
        };
        let text_of = |line: &Line<'static>| -> String {
            line.spans.iter().map(|sp| sp.content.as_ref()).collect()
        };

        let line = minimal_progress_line(Some(&entry), Some(12_400), 500);
        assert_eq!(text_of(&line), "  ⠴  12.4s · bash");
        assert_eq!(
            line.spans.last().unwrap().style.fg,
            Some(Color::LightYellow)
        );

        // A row with nothing to time yet: the state word, no separator.
        let undated = jyc_types::ActivityEntry {
            timestamp: None,
            ..entry.clone()
        };
        assert_eq!(
            text_of(&minimal_progress_line(Some(&undated), None, 500)),
            "  ⠴  bash"
        );

        // Processing started, no activity event at all.
        assert_eq!(
            text_of(&minimal_progress_line(None, None, 0)),
            "  ⠋  thinking"
        );
    }
}
