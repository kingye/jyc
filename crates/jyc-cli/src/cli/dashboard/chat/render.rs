//! Chat rendering helpers.

use super::table_wrap::wrap_tables;
use super::*;
use super::{App, ChatMessage};

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

/// Like [`truncate_to_width`], but keeps the **end** of a long string. A typed
/// answer is appended to as it is written, so its tail is the part the user
/// needs to see — the head is what they already said. One column pays for the
/// leading ellipsis that says text was cut.
pub(super) fn tail_to_width(s: &str, max_width: usize) -> String {
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
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0usize;
    for ch in s.chars().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > keep {
            break;
        }
        used += w;
        kept.push(ch);
    }
    kept.reverse();
    format!("…{}", kept.into_iter().collect::<String>())
}

/// Background of a human turn's block (see [`render_history_lines`]).
///
/// `#393552` — the muted violet backdrop for human turns. It is a hardcoded
/// RGB on purpose: the value stays independent of the terminal's ANSI palette.
///
/// The block sets **no foreground** — the text keeps the terminal's own color,
/// which is what makes a human turn read at exactly the same brightness as the
/// agent's reply.
pub(super) const USER_BG: Color = Color::Rgb(0x39, 0x35, 0x52);

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
/// tail (activity / live ticker) is appended by the caller after
/// these lines and stays per-frame.
///
/// Also returns the transcript line index of every user turn's opening rule —
/// the boundaries the message-area `[`/`]` keys jump between (see
/// [`crate::cli::dashboard::chat::ChatState::cursor_section_jump`]).
pub(crate) fn render_history_lines(
    messages: &[ChatMessage],
    width: usize,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut all_lines: Vec<Line<'static>> = Vec::new();
    let mut user_turn_lines: Vec<usize> = Vec::new();

    let dim_style = Style::default().fg(Color::DarkGray);
    // The pane is `width` wide; message bodies give up one column to the inset
    // they are drawn with (see the insert below), so a row never overflows.
    let body_width = width.saturating_sub(1);
    // The human side of the conversation: a background block in place of a
    // label. No foreground is set on purpose — see [`USER_BG`]: the text keeps
    // the terminal's own color, so a human turn reads at exactly the brightness
    // of the agent's reply.
    let user_style = Style::default().bg(USER_BG);
    let mut group_start_ts: Option<String> = None;
    // Last *conversation* sender (user/AI) — thinking pseudo-messages are
    // skipped so the human→AI blank row and the AI→human round close still
    // fire across an interleaved thinking block. Tracked incrementally.
    let mut prev_conv_sender: Option<&str> = None;

    for (idx, msg) in messages.iter().enumerate() {
        // Thinking pseudo-messages never render here: thinking content is
        // live furniture (see `render_thinking_block`), re-rendered every
        // frame so ctrl+p t expands without reprinting scrollback. Skipped
        // silently, so the human→AI blank row and the AI→human round close
        // still fire across an interleaved thinking block.
        if msg.sender == "thinking" {
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
            // The rule about to be pushed is this turn's first line — a
            // section boundary for the `[`/`]` keys.
            user_turn_lines.push(all_lines.len());
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
        // its background below). The human side's leading indentation is
        // protected from CommonMark's paragraph de-indentation (see
        // `protect_leading_whitespace`).
        let md_input = if is_user {
            protect_leading_whitespace(&msg.text)
        } else {
            msg.text.clone()
        };
        let md_text = softbreaks_to_hardbreaks(&format!("{md_input}\n"));
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

    (all_lines, user_turn_lines)
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
/// What a tool row's rendered first line starts with (see
/// [`render_activity_entry`]).
const TOOL_PREFIX: &str = "Tool: ";

/// Wall-clock milliseconds. The epoch floor is enforced in [`step_index`] —
/// the only place this value indexes anything.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
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

/// The spinner's color — the one bright spot on the progress tail. Everything
/// else on the tail is static; only the glyph itself turns.
fn spinner_style() -> Style {
    Style::default().fg(Color::LightYellow)
}

/// The spinner column. Two trailing spaces, because the `💭 ` / `⚠ ` prefixes
/// it replaces measure three columns and wrapped rows pad by their width —
/// one space would shift the text a column left.
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

/// The name of the tool an entry reports, if its formatted row is a tool row.
/// `edit`/`write` events arrive as raw JSON and only take the
/// `Tool: edit — foo.rs` shape inside `render_activity_entry`, so reading the
/// name off the raw text would call a live edit "no tool". Read the same
/// formatted row the tail draws — the collapsed shape, because that is the
/// one that carries the `Tool: ` prefix even while `ctrl+p T` has the diff
/// open.
fn entry_tool_name(entry: &jyc_types::ActivityEntry) -> Option<String> {
    let line_text = entry_activity_text(entry);
    let formatted = render_activity_entry(&line_text, false);
    let text = formatted.first().map(String::as_str).unwrap_or(&line_text);
    let name = activity_state(text);
    (name != "thinking").then(|| name.to_string())
}

/// The entire progress tail in minimal mode: `⠹  12.4s · thinking... | bash | ⋮2`.
///
/// The state word names the current phase; when the phase is not a tool of
/// its own (thinking, error), the most recent tool entry rides after ` | ` so
/// a thinking streak does not push the tool name off the line. A queued-count
/// suffix appears when another message is waiting behind this turn. The time
/// is the live loop ticker (1 Hz), falling back to the current entry's own
/// age before the first tick arrives; with neither, the line is just the
/// spinner and the phase word.
fn minimal_progress_line(
    entries: &[jyc_types::ActivityEntry],
    live_tick_ms: Option<u64>,
    now_ms: i64,
    queued: usize,
) -> Line<'static> {
    let last = entries.last();
    let timestamp = last.and_then(|entry| entry.timestamp.clone());
    let elapsed = match live_tick_ms {
        Some(ms) => format_elapsed_ms(ms),
        None => format_elapsed(&timestamp),
    };
    let dim = Style::default().fg(Color::Gray);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(spinner_prefix(now_ms), spinner_style()),
    ];
    if !elapsed.is_empty() {
        spans.push(Span::styled(format!("{elapsed} · "), dim));
    }
    let (state, failed, is_tool) = match last {
        // One line has to say when the round is failing: in this mode the
        // full tail — and its red error row — is off screen.
        Some(entry) if entry_severity(entry) == jyc_types::Severity::Error => {
            ("error".to_string(), true, false)
        }
        Some(entry) => match entry_tool_name(entry) {
            Some(name) => (name, false, true),
            None => ("thinking...".to_string(), false, false),
        },
        None => ("thinking...".to_string(), false, false),
    };
    let last_tool = if is_tool {
        None
    } else {
        entries.iter().rev().find_map(entry_tool_name)
    };
    spans.push(Span::styled(
        state,
        if failed {
            Style::default().fg(Color::Red)
        } else {
            dim
        },
    ));
    if let Some(name) = last_tool {
        // The phase is not a tool of its own — keep the most recent tool on
        // the line after ` | `.
        spans.push(Span::styled(format!(" | {name}"), dim));
    }
    if queued > 0 {
        spans.push(Span::styled(format!(" | ⋮{queued}"), dim));
    }
    Line::from(spans)
}

/// Rebuild the one-line text the backend used to compose, now from the raw
/// event the entry carries. Wording matches the old server-side formatter
/// exactly — the render pipeline below (`render_activity_entry`) consumes
/// these lines unchanged. Legacy rows written before the raw-event shape
/// have no `event.type`; their prose rides in `text` and passes through.
/// Dynamic progress tail (activity / live ticker) — rebuilt
/// every frame and drawn as viewport furniture (see `render_chat_viewport`),
/// so the spinner animates and elapsed-time ticks in place instead of
/// freezing into a printed scrollback row. Extracted from the old pane
/// renderer byte-for-byte: only the data source (`app`) and the width
/// parameter remain.
pub(crate) fn render_progress_tail(app: &App, width: u16) -> Vec<Line<'static>> {
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
                jyc_types::find_topic(&s.topics, chat_name, app.chat.channel.as_deref())
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

        // Live wall-clock ticker (1 Hz, with the first tick at t=0). When
        // present, it is the authoritative elapsed-time display for the
        // last in-progress line — the polled `last_active_at` it
        // normally shows goes stale during silent LLM/tool work. Falls
        // back to the polled value when no tick has arrived yet.
        let live_tick_ms: Option<u64> = live_chan
            .as_deref()
            .zip(live_topic.as_deref())
            .and_then(|(c, t)| app.chat.live_tick_ms_for(c, t));

        // Thinking is rendered by `render_thinking_block` (the section
        // above this tail), not here — one owner for both the collapsed
        // summary and the expanded text.

        if minimal {
            let queued = selected_topic_detail(app)
                .map(|detail| detail.queued)
                .unwrap_or(0);
            tail_lines.push(minimal_progress_line(
                &activity_entries,
                live_tick_ms,
                now,
                queued,
            ));
        } else if activity_entries.is_empty() {
            // Pre-activity placeholder: shown only between ProcessingStarted
            // and the first ToolStarted/LLMRequestStarted event. There's
            // no "since-event" numerator to pair with, so we display the
            // live ticker alone in `(12.4s)` form. This intentionally
            // diverges from the dual-time format below — there's literally
            // no `a.timestamp` to compute the left half from.
            //
            // No tool name here — the whole sentence is the label, in the
            // same yellow italic as the in-progress rows below.
            let body = match live_tick_ms {
                Some(ms) => format!("AI is thinking... ({})", format_elapsed_ms(ms)),
                None => "AI is thinking...".to_string(),
            };
            tail_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(spinner_prefix(now), spinner_style()),
                Span::styled(
                    body,
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
                    render_activity_entry(&entry_activity_text(a), app.chat.tool_detail_expanded)
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
                                // spinner column.
                                "   ".to_string()
                            };
                            let text = if current && !elapsed.is_empty() {
                                format!("{line} {elapsed}")
                            } else {
                                line
                            };
                            let label_style = style_diff_line(&text, style);
                            // The current row is the only animated thing on the
                            // tail: its spinner turns in bright yellow. The
                            // text is static, so a diff-colored row (`+`/`-`)
                            // keeps the only color that means anything.
                            (prefix, vec![Span::styled(text, label_style)])
                        })
                        .collect();

                for (prefix, spans) in rendered_lines {
                    push_tail_rows_spans(&mut tail_lines, width, &prefix, spinner_style(), spans);
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
                        entry_severity(e) == jyc_types::Severity::Error
                            && entry_activity_text(e).starts_with("ERROR:")
                    })
                    .map(entry_activity_text)
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
            width,
            "⚠ ",
            Style::default(),
            message,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        );
    }
    tail_lines
}

pub(super) fn entry_activity_text(entry: &jyc_types::ActivityEntry) -> String {
    let event = &entry.event;
    let Some(kind) = event.get("type").and_then(|t| t.as_str()) else {
        return entry.text.clone().unwrap_or_default();
    };
    let str_field = |k: &str| event.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let u64_field = |k: &str| event.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    match kind {
        "processing_started" => "Processing started".to_string(),
        "processing_progress" => format!(
            "{} ({}s, {} chars)",
            str_field("activity"),
            u64_field("elapsed_secs"),
            u64_field("output_length")
        ),
        "processing_completed" => {
            let secs = u64_field("duration_secs");
            if event
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or_default()
            {
                format!("Completed ({secs}s)")
            } else {
                format!("Failed ({secs}s)")
            }
        }
        "llm_request_started" => format!("Thinking... (iteration {})", u64_field("iteration")),
        "tool_started" => {
            let name = str_field("tool_name");
            match name {
                "edit" | "write" => file_tool_event_text(event, name, None),
                _ => match event.get("input").and_then(|v| v.as_str()) {
                    Some(inp) => format!("Tool: {name} — {inp}"),
                    None => format!("Tool: {name} (running)"),
                },
            }
        }
        "tool_completed" => {
            let name = str_field("tool_name");
            let secs = u64_field("duration_secs");
            let success = event
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if success {
                match name {
                    "edit" | "write" => file_tool_event_text(event, name, Some(secs)),
                    _ => match event.get("input").and_then(|v| v.as_str()) {
                        Some(inp) => format!("Tool: {name} (done, {secs}s) — {inp}"),
                        None => format!("Tool: {name} (done, {secs}s)"),
                    },
                }
            } else {
                match event.get("output").and_then(|v| v.as_str()) {
                    Some(err) => {
                        format!("Tool: {name} (FAILED, {secs}s) {}", err.replace('\n', " "))
                    }
                    None => format!("Tool: {name} (FAILED, {secs}s)"),
                }
            }
        }
        "thinking" => {
            let text = str_field("text");
            if (u64_field("full_length") as usize) > text.len() {
                format!("Thinking: {text}...")
            } else {
                format!("Thinking: {text}")
            }
        }
        "incoming_message" => format!(
            "Message from {}: {}",
            str_field("sender"),
            str_field("text").replace('\n', " ")
        ),
        "reply_sent" => {
            let oneline = str_field("text").replace('\n', " ");
            let preview: String = oneline.chars().take(100).collect();
            format!("Reply sent: {preview}")
        }
        "session_status" => {
            let label = match str_field("status_type") {
                "retry" => "RETRY",
                "error" => "ERROR",
                "rate_limit" => "RATE LIMITED",
                "timeout" => "TIMEOUT",
                "no_reply" => "NO REPLY",
                other => other,
            };
            let mut text = match event.get("attempt").and_then(|v| v.as_u64()) {
                Some(n) => format!("{label} (attempt #{n})"),
                None => label.to_string(),
            };
            // The old backend appended for any `Some`, even an empty message
            // ("ERROR: ") — keep that, don't second-guess the writer.
            if let Some(message) = event.get("message").and_then(|v| v.as_str()) {
                text.push_str(&format!(": {}", message.replace('\n', " ")));
            }
            text
        }
        "loop_tick" => format!("LoopTick ({}ms)", u64_field("elapsed_ms")),
        _ => entry.text.clone().unwrap_or_default(),
    }
}

/// Severity for an entry, derived from the raw event — error kinds only,
/// everything else is Info. Mirrors the mapping the old backend applied at
/// write time. Legacy rows (no `event.type`) default to Info.
pub(super) fn entry_severity(entry: &jyc_types::ActivityEntry) -> jyc_types::Severity {
    match entry.event.get("type").and_then(|t| t.as_str()) {
        Some("session_status") => match entry.event.get("status_type").and_then(|v| v.as_str()) {
            Some("error") | Some("timeout") => jyc_types::Severity::Error,
            Some("retry") | Some("rate_limit") | Some("no_reply") => jyc_types::Severity::Warning,
            _ => jyc_types::Severity::Info,
        },
        Some("tool_completed") | Some("processing_completed")
            if entry.event.get("success").and_then(|v| v.as_bool()) == Some(false) =>
        {
            jyc_types::Severity::Error
        }
        _ => jyc_types::Severity::Info,
    }
}

/// Reproduce the edit/write JSON blob the old backend embedded in the entry
/// text (the renderer's diff view parses it back out). `duration` is `Some`
/// for `tool_completed` — adds `duration_secs`, plus `line_no` parsed from
/// the tool output for edit — and `None` for `tool_started`.
fn file_tool_event_text(
    event: &serde_json::Value,
    tool_name: &str,
    duration: Option<u64>,
) -> String {
    let parsed: Option<serde_json::Value> = event
        .get("input")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str(s).ok());
    let field = |k: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
    };
    let file_path = field("file_path").unwrap_or("?");
    match (tool_name, duration) {
        ("edit", Some(secs)) => {
            // Parse "Edited 'file' at line N: M replacement(s) made" from the
            // tool output — the same derivation the old backend performed.
            let line_no = event
                .get("output")
                .and_then(|v| v.as_str())
                .and_then(|s| {
                    s.find("at line ").and_then(|pos| {
                        let rest = &s[pos + 8..];
                        rest.find(':').map(|end| &rest[..end])
                    })
                })
                .and_then(|n| n.trim().parse::<u64>().ok());
            serde_json::json!({
                "type": "edit",
                "file_path": file_path,
                "line_no": line_no,
                "old_string": field("old_string").unwrap_or(""),
                "new_string": field("new_string").unwrap_or(""),
                "duration_secs": secs,
            })
            .to_string()
        }
        ("edit", None) => serde_json::json!({
            "type": "edit",
            "file_path": file_path,
            "old_string": field("old_string").unwrap_or(""),
            "new_string": field("new_string").unwrap_or(""),
        })
        .to_string(),
        ("write", Some(secs)) => serde_json::json!({
            "type": "write",
            "file_path": file_path,
            "content": field("content").unwrap_or(""),
            "duration_secs": secs,
        })
        .to_string(),
        _ => serde_json::json!({
            "type": "write",
            "file_path": file_path,
            "content": field("content").unwrap_or(""),
        })
        .to_string(),
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
    prefix_style: Style,
    text: String,
    style: Style,
) {
    push_tail_rows_spans(
        out,
        pane_width,
        prefix,
        prefix_style,
        vec![Span::styled(text, style)],
    );
}

/// [`push_tail_rows`] with the row already split into styled spans — lets
/// [`wrap_styled_lines`] handle the wrapping (it rebuilds spans from the
/// per-character styles, so a multi-span row wraps without losing either color).
fn push_tail_rows_spans(
    out: &mut Vec<Line<'static>>,
    pane_width: u16,
    prefix: &str,
    prefix_style: Style,
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
        let mut spans = vec![Span::raw("  "), Span::styled(marker, prefix_style)];
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
        "grep" => &["pattern"],
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

/// Rows the viewport's live furniture occupies. The viewport is exactly
/// this tall (see `InlinePrinter::sync`), so the composer sits directly
/// under the printed transcript — and every section length here must match
/// `render_chat_viewport`'s layout one-to-one.
pub(crate) struct FurnitureLayout {
    pub tail: u16,
    pub thinking: u16,
    /// Editor chrome, `input_area_rows` semantics: includes its bottom rule.
    pub input: u16,
    pub popup: u16,
}

impl FurnitureLayout {
    /// Total viewport rows: top rule + input (incl. bottom rule) + popup +
    /// status line, under the tail/thinking blocks.
    pub fn total(&self) -> u16 {
        self.tail
            .saturating_add(self.thinking)
            .saturating_add(1) // top rule
            .saturating_add(self.input)
            .saturating_add(self.popup)
            .saturating_add(1) // status line
    }
}

/// Derives the furniture layout — the single source of truth for both the
/// viewport height (`InlinePrinter::sync`) and `render_chat_viewport`'s
/// section split. For terminals of two rows or more, `term_height` bounds
/// every section, so `total()` never exceeds it: the editor chrome and
/// status line keep their rows first
/// (input itself is capped at `term_height - 2`), then the popup, then the
/// progress tail (at least one row whenever it has anything to show), and
/// the thinking block gives up its rows last. On terminals shorter than
/// that minimum the overflow sections clip rather than push the composer
/// off screen.
pub(crate) fn furniture_layout(app: &App, width: u16, term_height: u16) -> FurnitureLayout {
    // The editor's two rules and the status line always render, so the
    // editor body itself is capped first.
    let input = app
        .chat
        .input_area_rows(width, term_height)
        .min(term_height.max(1).saturating_sub(2));
    // Rows left for the soft sections, in their cropping priority: popup
    // first, then the progress tail, then thinking.
    let budget = term_height.saturating_sub(input + 2);
    let popup = match (app.chat.command_popup.as_ref(), app.chat.leader.as_ref()) {
        (Some(state), _) => crate::cli::command_popup::popup_height(state, &app.chat.commands),
        // The leader popup keeps its grid layout, so reserve the full grid
        // height — the same number `render_anchored` draws.
        (None, Some(leader)) => leader.popup_height(width as usize),
        (None, None) => 0,
    }
    .min(budget);
    let after_popup = budget.saturating_sub(popup);
    let tail_needed = render_progress_tail(app, width).len() as u16;
    let tail = tail_needed
        .min(after_popup)
        .max(if tail_needed > 0 && after_popup > 0 {
            1
        } else {
            0
        });
    let thinking =
        (render_thinking_block(app, width).len() as u16).min(after_popup.saturating_sub(tail));
    FurnitureLayout {
        tail,
        thinking,
        input,
        popup,
    }
}

/// Live furniture block for the current turn's thinking: live blocks from
/// the websocket while the round is in flight, then the collapsed
/// pseudo-message the completed round was folded into. One-line summary by
/// default, the full wrapped text when ctrl+p t expanded it. Sits in
/// the viewport (redrawn every frame) rather than the printed transcript,
/// so toggling the expansion never reprints scrollback.
pub(crate) fn render_thinking_block(app: &App, width: u16) -> Vec<Line<'static>> {
    let style = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::ITALIC);
    let render_block = |text: &str, expanded: bool| -> Vec<Line<'static>> {
        if expanded {
            wrap_text_to_width(text, width as usize)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, style)))
                .collect()
        } else {
            let chars = text.chars().count();
            vec![Line::from(Span::styled(
                format!("💭 thinking — {chars} chars (ctrl+p t to expand)"),
                style,
            ))]
        }
    };
    // Live blocks for the in-flight turn come first (they are the state the
    // viewport actually shows); once processing completes they are folded
    // into a `thinking` pseudo-message in `messages` below. Collapsed, the
    // whole turn is one summary line no matter how many blocks arrived.
    let live: Vec<&str> = app
        .chat
        .channel
        .as_deref()
        .zip(app.chat.topic.as_deref())
        .and_then(|(c, t)| app.chat.live_thinking_for(c, t))
        .map(|blocks| blocks.iter().map(String::as_str).collect())
        .unwrap_or_default();
    // Compact progress hides the whole live stack (progress tail and
    // thinking alike) — the inline viewport is meant to stay nearly
    // static while a turn is in flight. The post-turn summary of a
    // completed turn stays: like the progress section, only the live
    // thinking is suppressed.
    if !live.is_empty() && app.chat.minimal_progress {
        return Vec::new();
    }
    if !live.is_empty() {
        if app.chat.thinking_expanded {
            let joined = live.join("\n\n");
            return wrap_text_to_width(&joined, width as usize)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, style)))
                .collect();
        }
        let chars: usize = live.iter().map(|b| b.chars().count()).sum();
        return vec![Line::from(Span::styled(
            format!(
                "💭 thinking — {} blocks · {chars} chars (ctrl+p t to expand)",
                live.len()
            ),
            style,
        ))];
    }
    let Some(msg) = app
        .chat
        .messages
        .iter()
        .rev()
        .find(|m| m.sender == "thinking" && !m.text.trim().is_empty())
    else {
        return Vec::new();
    };
    render_block(&msg.text, app.chat.thinking_expanded)
}

/// Renders the inline chat viewport — the live furniture only, since the
/// transcript lives in the terminal's own scrollback above (see
/// `chat::inline`). Replaces the old pane renderer
/// (`render_chat_conversation`): same widgets, but laid out top-to-bottom
/// inside the viewport rows instead of under a transcript. The live tail
/// and the thinking block live here (not in the transcript) so they redraw
/// every frame — spinner animation, elapsed-time ticker, and the
/// minimal/full progress + thinking-expansion toggles all take effect
/// immediately without touching printed scrollback.
pub(super) fn render_chat_viewport(frame: &mut Frame, area: Rect, app: &mut App) {
    let layout = furniture_layout(app, area.width, app.chat.term_height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            // Slack (a caller handing down a taller area than the furniture
            // needs — tests do): the real loop sizes the viewport to
            // `layout.total()` exactly, so this resolves to zero there.
            Constraint::Min(0),
            Constraint::Length(layout.thinking),
            Constraint::Length(layout.tail),
            Constraint::Length(1),
            // Editor body: `input_area_rows` already counts the bottom rule.
            Constraint::Length(layout.input.saturating_sub(1)),
            Constraint::Length(1),
            Constraint::Length(layout.popup),
            Constraint::Length(1),
        ])
        .split(area);

    // Latest-turn thinking block — the dynamic area above the progress
    // tail, the same position tool-call detail output occupies.
    if layout.thinking > 0 {
        let block = Rect {
            x: chunks[1].x,
            y: chunks[1].y,
            width: chunks[1].width,
            height: layout.thinking,
        };
        let lines = ratatui::widgets::Paragraph::new(render_thinking_block(app, area.width));
        frame.render_widget(lines, block);
    }

    // Live progress tail (activity / spinner / ticker) — the animated rows,
    // repainted every frame, sitting directly above the input's top rule.
    if layout.tail > 0 {
        let tail = render_progress_tail(app, area.width);
        let block = Rect {
            x: chunks[2].x,
            y: chunks[2].y,
            width: chunks[2].width,
            height: layout.tail,
        };
        let lines = ratatui::widgets::Paragraph::new(tail);
        frame.render_widget(lines, block);
    }

    // Top rule of the input area: also the popup's top boundary. Same style
    // the old full-screen composer used: sapphire when the chat pane has the
    // focus, the dim line-drawing color otherwise, yellow while a question
    // is open.
    let rule_style = if app.chat.active_question() {
        Style::default().fg(Color::Yellow)
    } else if app.chat.focus == Focus::ChatPane {
        Style::default()
            .fg(Color::Rgb(116, 199, 236))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(USER_BG)
    };
    let rule = Paragraph::new(Line::styled(
        "─".repeat(chunks[3].width as usize),
        rule_style,
    ));
    frame.render_widget(rule, chunks[3]);

    // Editor body (or the question box over it).
    frame.render_widget(&app.chat.editor, chunks[4]);
    if app.chat.active_question() {
        super::render_question_box(frame, chunks[4], app, app.chat.term_height);
    }

    // Bottom rule below the editor.
    let rule = Paragraph::new(Line::styled(
        "─".repeat(chunks[4].width as usize),
        rule_style,
    ));
    frame.render_widget(rule, chunks[5]);

    // Popup slot — only rows the popup actually needs are reserved, so the
    // status line sits flush under the editor when no popup is open.
    if layout.popup > 0
        && let Some(ref popup) = app.chat.command_popup
    {
        let popup_area = Rect {
            x: chunks[6].x,
            y: chunks[6].y,
            width: chunks[6].width,
            height: layout.popup,
        };
        crate::cli::command_popup::render_command_popup(
            frame,
            popup_area,
            popup,
            &app.chat.commands,
        );
    } else if let Some(ref leader) = app.chat.leader {
        // Anchored grid popup: fills the reserved slot top-to-bottom, the
        // same grid `furniture_layout` counted rows for.
        leader.render_anchored(
            frame,
            Rect {
                x: chunks[6].x,
                y: chunks[6].y,
                width: chunks[6].width,
                height: layout.popup,
            },
        );
    }

    // Status line — the same left counters + right chips strip, at the
    // viewport's last row.
    let status = build_status_line(chunks[7].width as usize, selected_topic_detail(app), false);
    frame.render_widget(Paragraph::new(status), chunks[7]);
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
    fn furniture_height_tracks_tail_and_thinking() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<super::super::WsEvent>();
        drop(tx);
        let mut app = App::new(rx, None);
        app.chat.channel = Some("chan".into());
        app.chat.topic = Some("t1".into());
        let width = 40;
        // Idle + no thinking: just the editor, its rules, and the status
        // line.
        let idle = furniture_layout(&app, width, 24);
        assert_eq!(idle.tail, 0);
        assert_eq!(idle.thinking, 0);
        assert_eq!(
            idle.total(),
            idle.input + 1 + idle.popup + 1,
            "idle furniture is composer chrome plus the status line"
        );
        // A round in flight adds the progress tail.
        app.chat.awaiting_response = true;
        // Live thinking renders as furniture — collapsed one-liner by
        // default, full wrapped block when expanded.
        app.chat.live_thinking.insert(
            ("chan".to_string(), "t1".to_string()),
            vec!["some thinking".to_string()],
        );
        let collapsed = furniture_layout(&app, width, 24);
        assert_eq!(collapsed.thinking, 1, "collapsed thinking is one row");
        let thinking_rows = render_thinking_block(&app, width);
        assert!(
            !thinking_rows.is_empty(),
            "live thinking renders at least the header row"
        );
        assert!(
            thinking_rows
                .iter()
                .any(|l| l.spans.iter().any(|s| s.content.contains("💭")))
        );
        // Collapsed stays one row no matter how many blocks arrived — one
        // summary line per delta is what the inline viewport report was
        // about.
        app.chat
            .live_thinking
            .get_mut(&("chan".to_string(), "t1".to_string()))
            .unwrap()
            .push("more thinking".to_string());
        let rows = render_thinking_block(&app, width);
        assert_eq!(
            rows.len(),
            1,
            "two blocks still collapse to one summary row"
        );
        app.chat.thinking_expanded = true;
        let expanded = furniture_layout(&app, width, 24);
        assert!(
            expanded.thinking >= 1,
            "expanded thinking keeps at least the header row"
        );
        assert_eq!(
            expanded.total(),
            expanded.tail + expanded.thinking + expanded.input + 1 + expanded.popup + 1,
            "expanded thinking rows are part of the furniture total"
        );
    }

    #[test]
    fn short_terminal_shrinks_soft_furniture_before_the_essentials() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<super::super::WsEvent>();
        drop(tx);
        let mut app = App::new(rx, None);
        app.chat.channel = Some("chan".into());
        app.chat.topic = Some("t1".into());
        // Long thinking plus an open ten-row command popup on a terminal
        // with exactly one spare row above the essentials: the soft
        // sections give up their rows before the tail/editor/status do.
        app.chat
            .live_thinking
            .entry(("chan".to_string(), "t1".to_string()))
            .or_default()
            .push("word ".repeat(200));
        app.chat.command_popup = Some(crate::cli::command_popup::CommandPopupState::new());
        app.chat.commands = (0..10)
            .map(|i| jyc_types::CommandInfo {
                name: format!("/c{i}"),
                ..Default::default()
            })
            .collect();
        let width = 40;
        let essentials = app.chat.input_area_rows(width, u16::MAX) + 2;
        let layout = furniture_layout(&app, width, essentials + 1);
        assert_eq!(
            layout.popup, 1,
            "the one spare row goes to the popup, clamped from ten"
        );
        assert_eq!(layout.thinking, 0, "thinking gives up its rows first");
        assert!(
            layout.total() <= essentials + 1,
            "the furniture fits the short terminal"
        );

        // Even shorter than the editor chrome + rules: soft sections
        // collapse to zero, the editor body is capped too, and the total
        // still honors the terminal (what cannot fit clips at render time
        // instead of pushing the composer off screen).
        let tiny = app.chat.input_area_rows(width, u16::MAX);
        let cramped = furniture_layout(&app, width, tiny);
        assert_eq!(cramped.popup, 0);
        assert_eq!(cramped.thinking, 0);
        assert_eq!(cramped.tail, 0);
        assert!(
            cramped.total() <= tiny.max(1),
            "total ({}) fits a {}-row terminal",
            cramped.total(),
            tiny
        );
    }

    #[test]
    fn tail_floor_wins_over_thinking_when_rows_run_out() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<super::super::WsEvent>();
        drop(tx);
        let mut app = App::new(rx, None);
        app.chat.channel = Some("chan".into());
        app.chat.topic = Some("t1".into());
        app.chat.awaiting_response = true;
        app.chat
            .live_thinking
            .entry(("chan".to_string(), "t1".to_string()))
            .or_default()
            .push("word ".repeat(200));
        let width = 40;
        let essentials = app.chat.input_area_rows(width, u16::MAX) + 2;
        // One spare row above the essentials: the tail keeps its floor row
        // (it has content while we await the reply) and thinking gets none.
        let one = furniture_layout(&app, width, essentials + 1);
        assert_eq!(one.tail, 1, "the live tail keeps its floor row");
        assert_eq!(one.thinking, 0, "thinking yields the last row to the tail");
        // Two spare rows: the tail still takes only its floor and thinking
        // may grow into what remains.
        let two = furniture_layout(&app, width, essentials + 2);
        assert_eq!(two.tail, 1);
        assert_eq!(two.thinking, 1, "one more row goes to thinking");
        assert!(
            two.total() <= essentials + 2,
            "the furniture fits the terminal"
        );
    }

    #[test]
    fn furniture_thinking_rows_never_exceed_the_terminal() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<super::super::WsEvent>();
        drop(tx);
        let mut app = App::new(rx, None);
        app.chat.channel = Some("chan".into());
        app.chat.topic = Some("t1".into());
        app.chat.awaiting_response = true;
        app.chat.live_thinking.insert(
            ("chan".to_string(), "t1".to_string()),
            vec!["word ".repeat(200)],
        );
        app.chat.thinking_expanded = true;
        for term_height in [10u16, 24, 200] {
            let layout = furniture_layout(&app, 40, term_height);
            assert!(
                layout.total() <= term_height,
                "furniture ({}) must fit a {term_height}-row terminal",
                layout.total()
            );
        }
    }

    #[test]
    fn history_lines_skip_thinking_pseudo_messages() {
        let msgs = vec![
            ChatMessage {
                sender: "ai".into(),
                text: "answer".into(),
                timestamp: None,
                id: 0,
                client_id: None,
            },
            ChatMessage {
                sender: "thinking".into(),
                text: "secret chain".into(),
                timestamp: None,
                id: 1,
                client_id: None,
            },
        ];
        let (lines, _) = render_history_lines(&msgs, 40);
        let joined: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            !joined.iter().any(|l| l.contains("secret chain")),
            "thinking content stays out of the printed transcript:\n{}",
            joined.join("\n")
        );
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
            push_tail_rows(
                &mut out,
                40,
                prefix,
                Style::default(),
                "w".repeat(60),
                Style::default(),
            );
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
        push_tail_rows(
            &mut out,
            40,
            "💭 ",
            Style::default(),
            String::new(),
            Style::default(),
        );
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
    fn spinner_is_the_one_bright_spot_on_the_tail() {
        assert_eq!(spinner_style().fg, Some(Color::LightYellow));
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
    fn minimal_progress_line_is_one_animated_row() {
        let entry = jyc_types::ActivityEntry {
            timestamp: Some("2026-08-13T10:00:00Z".to_string()),
            event: serde_json::Value::Null,
            text: Some("Tool: bash — {\"command\":\"ls\"}".to_string()),
            id: 0,
        };
        let text_of = |line: &Line<'static>| -> String {
            line.spans.iter().map(|sp| sp.content.as_ref()).collect()
        };

        // A tool is the phase: its name alone, no pipe.
        let line = minimal_progress_line(std::slice::from_ref(&entry), Some(12_400), 500, 0);
        assert_eq!(text_of(&line), "  ⠴  12.4s · bash");
        assert_eq!(
            line.spans.last().unwrap().style.fg,
            Some(Color::Gray),
            "the state word sits in the row's gray — it does not blink"
        );
        assert_eq!(
            line.spans[1].style.fg,
            Some(Color::LightYellow),
            "the spinner is the one bright spot"
        );

        // A row with nothing to time yet: the state word, no separator.
        let undated = jyc_types::ActivityEntry {
            timestamp: None,
            ..entry.clone()
        };
        assert_eq!(
            text_of(&minimal_progress_line(&[undated], None, 500, 0)),
            "  ⠴  bash"
        );

        // Processing started, no activity event at all.
        assert_eq!(
            text_of(&minimal_progress_line(&[], None, 0, 0)),
            "  ⠋  thinking..."
        );

        // A thinking streak keeps the most recent tool on the line instead of
        // overwriting it — this row's whole point.
        let thinking = jyc_types::ActivityEntry {
            text: Some("Thinking... (iteration 14)".to_string()),
            timestamp: Some("2026-08-13T10:00:02Z".to_string()),
            ..entry.clone()
        };
        let line = minimal_progress_line(&[entry.clone(), thinking], Some(12_400), 500, 0);
        assert_eq!(text_of(&line), "  ⠴  12.4s · thinking... | bash");

        // A failing round has to say so on its one line — in this mode the
        // full tail, and its red error row, are off screen.
        let failed = jyc_types::ActivityEntry {
            event: jyc_core::topic_event::TopicEvent::ProcessingCompleted {
                topic_name: "t".to_string(),
                message_id: "m1".to_string(),
                success: false,
                duration_secs: 30,
                timestamp: chrono::Utc::now(),
            }
            .to_activity_json(),
            text: None,
            timestamp: None,
            ..entry.clone()
        };
        let line = minimal_progress_line(&[failed], None, 500, 0);
        assert_eq!(text_of(&line), "  ⠴  error");
        assert_eq!(
            line.spans.last().unwrap().style.fg,
            Some(Color::Red),
            "the failure word is the static red the error row would have had"
        );

        // ...and a failure after tool work still names the tool.
        let failed = jyc_types::ActivityEntry {
            event: jyc_core::topic_event::TopicEvent::ProcessingCompleted {
                topic_name: "t".to_string(),
                message_id: "m1".to_string(),
                success: false,
                duration_secs: 30,
                timestamp: chrono::Utc::now(),
            }
            .to_activity_json(),
            text: None,
            timestamp: None,
            ..entry.clone()
        };
        let line = minimal_progress_line(&[entry.clone(), failed], None, 500, 0);
        assert_eq!(text_of(&line), "  ⠴  error | bash");

        // The shape the server actually sends for an edit: a tool_completed
        // event whose `input` is the tool's own JSON. `entry_activity_text`
        // rebuilds the edit JSON the renderer's diff view parses — reading
        // the state off anything less called a live edit "thinking".
        let raw_json = jyc_types::ActivityEntry {
            event: jyc_core::topic_event::TopicEvent::ToolCompleted {
                topic_name: "t".to_string(),
                tool_name: "edit".to_string(),
                success: true,
                duration_secs: 1,
                output: Some("Edited 'src/foo.rs' at line 42: 1 replacement(s) made".to_string()),
                input: Some(
                    r#"{"file_path":"src/foo.rs","old_string":"old","new_string":"new"}"#
                        .to_string(),
                ),
                timestamp: chrono::Utc::now(),
            }
            .to_activity_json(),
            text: None,
            timestamp: None,
            ..entry.clone()
        };
        assert_eq!(
            text_of(&minimal_progress_line(&[raw_json], None, 500, 0)),
            "  ⠴  edit"
        );
    }

    #[test]
    fn minimal_progress_line_marks_queued_messages() {
        let entry = jyc_types::ActivityEntry {
            timestamp: Some("2026-08-13T10:00:00Z".to_string()),
            event: serde_json::Value::Null,
            text: Some("Tool: bash — {\"command\":\"ls\"}".to_string()),
            id: 0,
        };
        let text_of = |line: &Line<'static>| -> String {
            line.spans.iter().map(|sp| sp.content.as_ref()).collect()
        };
        let line = minimal_progress_line(std::slice::from_ref(&entry), Some(12_400), 500, 2);
        assert_eq!(text_of(&line), "  ⠴  12.4s · bash | ⋮2");
        assert_eq!(
            line.spans.last().unwrap().style.fg,
            line.spans[line.spans.len() - 2].style.fg,
            "the queued suffix matches the phase/tool color"
        );
    }

    #[test]
    fn entry_activity_text_roundtrips_real_events_to_legacy_prose() {
        // Parity pin: a real TopicEvent → to_activity_json (what the server
        // actually stores) → entry_activity_text must reproduce the prose
        // the old backend composed, word for word.
        use jyc_core::topic_event::TopicEvent;
        let ts = chrono::Utc::now();
        let ev = |event: TopicEvent, expected: &str| {
            let entry = jyc_types::ActivityEntry {
                event: event.to_activity_json(),
                text: None,
                timestamp: None,
                id: 0,
            };
            assert_eq!(entry_activity_text(&entry), expected);
        };
        ev(
            TopicEvent::ProcessingStarted {
                topic_name: "t".into(),
                message_id: "m1".into(),
                timestamp: ts,
            },
            "Processing started",
        );
        ev(
            TopicEvent::ProcessingCompleted {
                topic_name: "t".into(),
                message_id: "m1".into(),
                success: true,
                duration_secs: 5,
                timestamp: ts,
            },
            "Completed (5s)",
        );
        ev(
            TopicEvent::ProcessingCompleted {
                topic_name: "t".into(),
                message_id: "m1".into(),
                success: false,
                duration_secs: 5,
                timestamp: ts,
            },
            "Failed (5s)",
        );
        ev(
            TopicEvent::ProcessingProgress {
                topic_name: "t".into(),
                elapsed_secs: 10,
                activity: "tool execution".into(),
                progress: None,
                parts_count: 1,
                output_length: 200,
                timestamp: ts,
            },
            "tool execution (10s, 200 chars)",
        );
        ev(
            TopicEvent::LLMRequestStarted {
                topic_name: "t".into(),
                iteration: 2,
                timestamp: ts,
            },
            "Thinking... (iteration 2)",
        );
        ev(
            TopicEvent::ToolStarted {
                topic_name: "t".into(),
                tool_name: "bash".into(),
                input: Some("ls -la".into()),
                timestamp: ts,
            },
            "Tool: bash — ls -la",
        );
        ev(
            TopicEvent::ToolStarted {
                topic_name: "t".into(),
                tool_name: "bash".into(),
                input: None,
                timestamp: ts,
            },
            "Tool: bash (running)",
        );
        ev(
            TopicEvent::ToolCompleted {
                topic_name: "t".into(),
                tool_name: "bash".into(),
                success: true,
                duration_secs: 3,
                output: None,
                input: Some("ls".into()),
                timestamp: ts,
            },
            "Tool: bash (done, 3s) — ls",
        );
        ev(
            TopicEvent::ToolCompleted {
                topic_name: "t".into(),
                tool_name: "bash".into(),
                success: true,
                duration_secs: 3,
                output: None,
                input: None,
                timestamp: ts,
            },
            "Tool: bash (done, 3s)",
        );
        ev(
            TopicEvent::ToolCompleted {
                topic_name: "t".into(),
                tool_name: "bash".into(),
                success: false,
                duration_secs: 3,
                output: Some("boom\nfail".into()),
                input: None,
                timestamp: ts,
            },
            "Tool: bash (FAILED, 3s) boom fail",
        );
        ev(
            TopicEvent::ToolCompleted {
                topic_name: "t".into(),
                tool_name: "bash".into(),
                success: false,
                duration_secs: 3,
                output: None,
                input: None,
                timestamp: ts,
            },
            "Tool: bash (FAILED, 3s)",
        );
        ev(
            TopicEvent::Thinking {
                topic_name: "t".into(),
                text: "xyz".into(),
                full_length: 10,
                timestamp: ts,
            },
            "Thinking: xyz...",
        );
        ev(
            TopicEvent::Thinking {
                topic_name: "t".into(),
                text: "xyz".into(),
                full_length: 3,
                timestamp: ts,
            },
            "Thinking: xyz",
        );
        ev(
            TopicEvent::IncomingMessage {
                id: 0,
                client_msg_id: None,
                topic_name: "t".into(),
                sender: "u".into(),
                text: "hi".into(),
                timestamp: ts,
            },
            "Message from u: hi",
        );
        ev(
            TopicEvent::ReplySent {
                id: 0,
                topic_name: "t".into(),
                text: "x".repeat(150),
                timestamp: ts,
            },
            &format!("Reply sent: {}", "x".repeat(100)),
        );
        ev(
            TopicEvent::SessionStatus {
                topic_name: "t".into(),
                status_type: "retry".into(),
                attempt: Some(2),
                message: Some("slow".into()),
                timestamp: ts,
            },
            "RETRY (attempt #2): slow",
        );
        // Some("") still produced "ERROR: " (colon + trailing space) in the
        // old backend — an empty message must not drop the separator.
        ev(
            TopicEvent::SessionStatus {
                topic_name: "t".into(),
                status_type: "error".into(),
                attempt: None,
                message: Some(String::new()),
                timestamp: ts,
            },
            "ERROR: ",
        );
        ev(
            TopicEvent::SessionStatus {
                topic_name: "t".into(),
                status_type: "no_reply".into(),
                attempt: None,
                message: None,
                timestamp: ts,
            },
            "NO REPLY",
        );
        ev(
            TopicEvent::LoopTick {
                topic_name: "t".into(),
                elapsed_ms: 1000,
                timestamp: ts,
            },
            "LoopTick (1000ms)",
        );
    }
}
