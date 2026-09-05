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
    /// Thinking expand/collapse affects the rendered lines of
    /// `sender == "thinking"` pseudo-messages, so it must bust the cache.
    thinking_expanded: bool,
}

pub(super) fn history_fingerprint(
    messages: &[ChatMessage],
    width: usize,
    thinking_expanded: bool,
) -> RenderFingerprint {
    RenderFingerprint {
        count: messages.len(),
        text_len_sum: messages.iter().map(|m| m.text.len()).sum(),
        last_timestamp: messages.last().and_then(|m| m.timestamp.clone()),
        width,
        thinking_expanded,
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
/// top/bottom rules (time / duration), user→AI separators, and each
/// message's markdown body word-wrapped to `width`. Pure in
/// `(messages, width)` so the result is cached per frame — the dynamic
/// progress tail (thinking / activity / live ticker) is appended by the
/// caller after these lines and stays per-frame.
pub(super) fn render_history_lines(
    messages: &[ChatMessage],
    width: usize,
    thinking_expanded: bool,
) -> Vec<Line<'static>> {
    let mut all_lines: Vec<Line<'static>> = Vec::new();

    let dim_style = Style::default().fg(Color::DarkGray);
    let thinking_style = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::ITALIC);
    let mut group_start_ts: Option<String> = None;
    // Last *conversation* sender (user/AI) — thinking pseudo-messages are
    // skipped so the user→AI separator and AI→user round close still fire
    // across an interleaved thinking block. Tracked incrementally.
    let mut prev_conv_sender: Option<&str> = None;

    for (idx, msg) in messages.iter().enumerate() {
        // Completed-turn thinking pseudo-message: collapsed one-liner by
        // default, full wrapped text when expanded. Takes no part in
        // round-rule grouping — the rules key off user/AI turns only.
        if msg.sender == "thinking" {
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
        let prefix = if is_user { "**You:** " } else { "**AI:** " };

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
            all_lines.push(Line::from(""));
            all_lines.push(Line::from(Span::styled("┄".repeat(width), dim_style)));
            all_lines.push(Line::from(""));
        }

        // Render message (no side gutters).
        let md_text = softbreaks_to_hardbreaks(&format!("{prefix}{}\n", msg.text));
        let rendered =
            tui_markdown::from_str_with_options(&md_text, &chat_markdown_options()).lines;
        let msg_lines = wrap_styled_lines(wrap_tables(rendered, width), width);
        all_lines.extend(msg_lines);
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
            let dash_count = width.saturating_sub(elapsed.len() + 3);
            all_lines.push(Line::from(vec![
                Span::styled(format!("{} ", "─".repeat(dash_count)), dim_style),
                Span::styled(elapsed, dim_style),
                Span::styled(" ──", dim_style),
            ]));
        }
        all_lines.push(Line::from(""));
    }

    all_lines
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
    );
    let cache_hit = matches!(&app.chat.render_cache, Some((fp, _)) if *fp == fingerprint);
    if !cache_hit {
        let lines = render_history_lines(
            &app.chat.messages,
            messages_width,
            app.chat.thinking_expanded,
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

        let thinking_blocks: Option<&[String]> = live_chan
            .as_deref()
            .zip(live_topic.as_deref())
            .and_then(|(c, t)| app.chat.live_thinking_for(c, t));

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
                tail_lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!(
                            "💭 thinking — {} blocks · {chars} chars (ctrl+p t)",
                            blocks.len()
                        ),
                        gray_style,
                    ),
                ]));
            }
        }

        if activity_entries.is_empty() && !has_thinking {
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
            tail_lines.push(Line::from(vec![
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

                // Split multi-line entries (e.g. edit diff) into separate lines
                // — edit/write JSON events carry their own data; the helper
                // unifies the `ctrl+p T` toggle across every tool.
                //
                // Diff-specific styling (`-` gray, `+` green) is decided here
                // on the *unpadded* `line`, before the caller pads it with
                // `"⏳ "` / `"   "` — checking the padded label would always
                // miss because the prefix sits two columns in.
                let rendered_lines: Vec<(Style, String)> =
                    render_activity_entry(&a.text, app.chat.tool_detail_expanded)
                        .into_iter()
                        .enumerate()
                        .map(|(line_idx, line)| {
                            let label = if line_idx == 0 && is_last {
                                if elapsed.is_empty() {
                                    format!("⏳ {line}")
                                } else {
                                    format!("⏳ {line} {elapsed}")
                                }
                            } else {
                                // Pad with 3 spaces to visually align with "⏳ "
                                format!("   {line}")
                            };
                            let label_style = style_diff_line(&line, style);
                            (label_style, label)
                        })
                        .collect();

                for (label_style, label) in rendered_lines {
                    tail_lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(label, label_style),
                    ]));
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
        tail_lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("⚠ {message}"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]));
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
    let max_skip = (history_len + tail_lines.len()).saturating_sub(inner_height);
    app.chat.last_max_scroll = max_skip;
    app.chat.scroll = app.chat.scroll.min(max_skip);
    let skip = max_skip.saturating_sub(app.chat.scroll);
    // Clone only the visible window (≤ inner_height lines) — the
    // Paragraph clips anything beyond the area anyway.
    let hist_slice: &[Line] = if skip < history_len {
        &history[skip..]
    } else {
        &[]
    };
    let tail_start = skip.saturating_sub(history_len);
    let visible_lines: Vec<Line> = hist_slice
        .iter()
        .chain(&tail_lines[tail_start..])
        .take(inner_height)
        .cloned()
        .collect();

    let messages_para = Paragraph::new(visible_lines);
    frame.render_widget(messages_para, chunks[0]);

    // --- Input area (text editor, at bottom) ---
    // The editor renders its own wrapping and scroll-follow. A two-line
    // prompt gutter sits left of the editor: the header row shows
    // "╭─ {mode} · {channel} · {pattern}[ · {branch}]", and "╰─❯" on the
    // first editor row; both dim when the input field loses focus.
    // The cursor is a blinking underline when the input has focus and
    // invisible when another pane does (a default-styled cursor cell is
    // indistinguishable from the text under it).
    app.chat.editor.set_cursor_style(match app.chat.focus {
        ChatFocus::ChatPane => Style::default()
            .add_modifier(Modifier::UNDERLINED)
            .add_modifier(Modifier::SLOW_BLINK),
        ChatFocus::MessageArea
        | ChatFocus::ActivityPane
        | ChatFocus::ExplorerPane
        | ChatFocus::InfoPane => Style::default(),
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

    // ── Command popup overlay ──
    if let Some(ref popup) = app.chat.command_popup {
        render_command_popup(frame, area, popup, &app.chat.commands, &app.chat.models);
    }

    // ── Leader-key popup overlay (TUI-local commands) ──
    if let Some(ref leader) = app.chat.leader {
        leader.render(frame, area);
    }
}

/// Reformat an activity line of the form `Tool: <name> — <json input>`
/// (optionally `Tool: <name> (done, Xs) — <json>`) into a readable summary
/// using the shared extractor, e.g. `Tool: bash — ls -la`. Extraction
/// happens here — at render time — so the WS payload and `activity.jsonl`
/// keep the raw JSON. Lines that don't match, or whose input can't be
/// summarized, pass through unchanged.
/// Render the raw text of one activity entry into a list of display
/// lines. The caller applies the per-line `⏳ ` / `   ` padding based on
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
                lines.push(reformat_tool_line(line));
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
/// content (no `⏳ ` prefix) — the caller applies per-line padding.
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

/// Style one unpadded diff line. The caller pads with `"⏳ "` / `"   "`
/// later, so the prefix check runs on the **unpadded** input — checking
/// the padded label would always miss because the prefix sits two
/// characters in (`"⏳ -..."` or `"   -..."`), not at column 0.
///
/// Returns `default` for the header (`<file>:<line>`), truncation
/// marker (`… (N more lines)`), and the empty case, so the rest of the
/// progress tail keeps its yellow-italic styling.
fn style_diff_line(unpadded: &str, default: Style) -> Style {
    if unpadded.starts_with(DIFF_REMOVED_PREFIX) {
        Style::default().fg(Color::Gray)
    } else if unpadded.starts_with(DIFF_ADDED_PREFIX) {
        Style::default().fg(Color::Green)
    } else {
        default
    }
}

fn reformat_tool_line(text: &str) -> String {
    let Some(rest) = text.strip_prefix("Tool: ") else {
        return text.to_string();
    };
    let Some((prefix, input)) = rest.split_once(" — ") else {
        return text.to_string();
    };
    // `prefix` is either the bare tool name ("bash") or the completion
    // variant ("bash (done, 3s)") — the extractor needs the name only.
    let name = prefix.split(" (").next().unwrap_or(prefix);
    match jyc_types::inspect::tool_activity_summary(name, Some(input)) {
        Some(summary) => format!("Tool: {prefix} — {summary}"),
        None => text.to_string(),
    }
}

/// Render the raw JSON input of a `Tool: <name> — <json>` activity line as
/// an indented multi-line field listing (`  key: value`), for the expanded
/// tool-detail view (`ctrl+p T`). Multi-line string values keep their
/// newlines with indented continuation lines; nested values stay on one
/// line in compact form. Capped at 20 lines with a trailing
/// `… (N more lines)` marker — same convention as the edit-diff renderer.
/// Returns empty for non-tool lines or unparseable input.
/// Per-tool list of field names whose values already appear in the
/// one-line summary produced by `jyc_types::inspect::tool_activity_summary`
/// — skipping them in `format_tool_input_full` prevents a redundant line
/// appearing below a summary that already inlines the value.
///
/// When `tool_activity_summary` reads either of two alternate keys for a
/// tool (e.g. `read_image` accepts `file_path` *or* `path`), list both —
/// the skip is a set, not a single key.
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
            reformat_tool_line(r#"Tool: bash — {"command": "ls -la"}"#),
            "Tool: bash — ls -la"
        );
        assert_eq!(
            reformat_tool_line(r#"Tool: bash (done, 3s) — {"command": "ls -la"}"#),
            "Tool: bash (done, 3s) — ls -la"
        );
        assert_eq!(
            reformat_tool_line(
                r#"Tool: edit — {"file_path": "/home/jiny/projects/jyc/src/tools.rs"}"#
            ),
            "Tool: edit — tools.rs"
        );
    }

    #[test]
    fn reformat_tool_line_passthrough() {
        // Unknown tool / unparseable input: show the line unchanged — the
        // raw JSON still carries information in the activity view.
        let unknown = r#"Tool: context_browse — {"offset": 0}"#;
        assert_eq!(reformat_tool_line(unknown), unknown);
        let bad_json = "Tool: bash — not json";
        assert_eq!(reformat_tool_line(bad_json), bad_json);
        // Non-tool lines are untouched.
        let other = "Thinking... (iteration 2)";
        assert_eq!(reformat_tool_line(other), other);
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
        // `inspect.rs` `tool_activity_summary_extracts_basename_for_file_tools`).
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
    /// caller padded the line with `"⏳ "` / `"   "`.
    #[test]
    fn style_diff_line_minus_emitted_by_render_file_tool_diff_is_gray() {
        let lines = render_file_tool_diff(
            "edit",
            &serde_json::json!({
                "type": "edit",
                "file_path": "src/foo.rs",
                "old_string": "removed line",
                "new_string": "added line",
            }),
        );
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
}
