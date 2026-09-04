//! Chat rendering helpers.

use super::table_wrap::wrap_tables;
use super::*;
use super::{App, ChatMessage, LINE_DRAWING};

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
                    // Plain text — split by newlines for display. Tool-call
                    // lines carry the raw JSON input; extract a readable
                    // summary at render time (the WS/log data stays raw).
                    let lines: Vec<String> = a
                        .text
                        .split('\n')
                        .enumerate()
                        .map(|(i, line)| {
                            if i == 0 {
                                reformat_tool_line(line)
                            } else {
                                line.to_string()
                            }
                        })
                        .collect();
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
}
