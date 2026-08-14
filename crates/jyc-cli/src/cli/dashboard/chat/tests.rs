use super::render::{history_fingerprint, render_history_lines};
use super::*;

fn history_msg(sender: &str, text: &str, ts: Option<&str>) -> ChatMessage {
    ChatMessage {
        sender: sender.to_string(),
        text: text.to_string(),
        timestamp: ts.map(|s| s.to_string()),
    }
}

#[test]
fn history_fingerprint_stable_for_unchanged_input() {
    // The typing case: same messages + width → same fingerprint →
    // cache hit → no markdown re-parse.
    let msgs = vec![
        history_msg("user", "hello", Some("2026-08-13T10:00:00Z")),
        history_msg("ai", "world", Some("2026-08-13T10:00:05Z")),
    ];
    assert_eq!(
        history_fingerprint(&msgs, 80),
        history_fingerprint(&msgs, 80)
    );
}

#[test]
fn history_fingerprint_changes_on_message_mutations() {
    let msgs = vec![
        history_msg("user", "hello", Some("2026-08-13T10:00:00Z")),
        history_msg("ai", "world", Some("2026-08-13T10:00:05Z")),
    ];
    let base = history_fingerprint(&msgs, 80);

    // New message pushed.
    let mut pushed = msgs.clone();
    pushed.push(history_msg("user", "again", None));
    assert_ne!(base, history_fingerprint(&pushed, 80));

    // Streaming append to the last message's text.
    let mut streamed = msgs.clone();
    streamed[1].text.push_str(" more");
    assert_ne!(base, history_fingerprint(&streamed, 80));

    // Last message timestamp set after the fact.
    let mut stamped = msgs.clone();
    stamped[1].timestamp = Some("2026-08-13T10:00:06Z".to_string());
    assert_ne!(base, history_fingerprint(&stamped, 80));

    // Cleared history.
    assert_ne!(base, history_fingerprint(&[], 80));

    // Pane resize (re-wrap needed).
    assert_ne!(base, history_fingerprint(&msgs, 100));
}

#[test]
fn render_history_lines_deterministic_for_cache_reuse() {
    let msgs = vec![
        history_msg("user", "hello **bold**", Some("2026-08-13T10:00:00Z")),
        history_msg("ai", "world\nsecond line", Some("2026-08-13T10:00:05Z")),
    ];
    assert_eq!(
        render_history_lines(&msgs, 80),
        render_history_lines(&msgs, 80)
    );
    // Different width re-wraps — cache must not be reused.
    assert_ne!(
        render_history_lines(&msgs, 80),
        render_history_lines(&msgs, 20)
    );
}

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

    // info pane is visible by default → full cycle:
    // Chat → MessageArea → InfoPane → ActivityPane → Chat.
    assert_eq!(app.chat.focus, ChatFocus::ChatPane);
    app.chat.toggle_focus();
    assert_eq!(app.chat.focus, ChatFocus::MessageArea);
    app.chat.toggle_focus();
    assert_eq!(app.chat.focus, ChatFocus::InfoPane);
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

    // MessageArea → InfoPane (visible by default) → Chat: the hidden
    // activity pane must be skipped.
    app.chat.toggle_focus();
    assert_eq!(app.chat.focus, ChatFocus::MessageArea);
    app.chat.toggle_focus();
    assert_eq!(app.chat.focus, ChatFocus::InfoPane);
    app.chat.toggle_focus();
    assert_eq!(
        app.chat.focus,
        ChatFocus::ChatPane,
        "activity pane must be skipped when activity_split=0"
    );
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
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.info_visible = true; // also the default; set explicitly for clarity
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
    // With the info pane hidden, the cycle must skip InfoPane.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.info_visible = false;
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
    // Render stores the max before input; mirror that here.
    app.chat.last_max_scroll = 10;
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
fn message_area_scroll_up_clamps_at_rendered_max() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.focus = ChatFocus::MessageArea;
    app.chat.last_max_scroll = 5;
    for _ in 0..10 {
        app.chat.scroll_up();
    }
    // No overshoot past the rendered maximum...
    assert_eq!(app.chat.scroll, 5);
    // ...so reversing direction moves the view immediately.
    app.chat.scroll_down();
    assert_eq!(app.chat.scroll, 4);
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
fn esc_does_not_close_chat_with_input_focused() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.phase = ChatPhase::Chatting;
    app.chat.thread = Some("jyc".to_string());
    app.chat.focus = ChatFocus::ChatPane;

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

fn chatting_app() -> App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.phase = ChatPhase::Chatting;
    app.chat.thread = Some("jyc".to_string());
    app
}

#[test]
fn leader_c_focuses_message_area() {
    let mut app = chatting_app();
    handle_chat_keys(
        &mut app,
        crossterm::event::KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &mut test_terminal(),
    );
    assert!(app.chat.leader.is_some());
    handle_chat_keys(
        &mut app,
        crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        &mut test_terminal(),
    );
    assert!(app.chat.leader.is_none());
    assert_eq!(app.chat.focus, ChatFocus::MessageArea);
}

#[test]
fn leader_slash_opens_command_popup() {
    let mut app = chatting_app();
    handle_chat_keys(
        &mut app,
        crossterm::event::KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &mut test_terminal(),
    );
    assert!(app.chat.leader.is_some());
    handle_chat_keys(
        &mut app,
        crossterm::event::KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        &mut test_terminal(),
    );
    assert!(app.chat.leader.is_none());
    assert!(app.chat.command_popup.is_some());
}

#[test]
fn printable_key_refocuses_input_without_inserting() {
    for focus in [
        ChatFocus::MessageArea,
        ChatFocus::InfoPane,
        ChatFocus::ActivityPane,
        ChatFocus::ExplorerPane,
    ] {
        for ch in ['i', 'a', 'x'] {
            let mut app = chatting_app();
            app.chat.focus = focus;
            handle_chat_keys(
                &mut app,
                crossterm::event::KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut test_terminal(),
            );
            assert_eq!(app.chat.focus, ChatFocus::ChatPane, "{focus:?} {ch:?}");
            assert!(
                app.chat.text().is_empty(),
                "{focus:?} {ch:?}: refocus key must be consumed, not inserted"
            );
        }
    }
}

#[test]
fn local_scroll_keys_do_not_refocus() {
    for focus in [
        ChatFocus::MessageArea,
        ChatFocus::InfoPane,
        ChatFocus::ActivityPane,
        ChatFocus::ExplorerPane,
    ] {
        let mut app = chatting_app();
        app.chat.focus = focus;
        handle_chat_keys(
            &mut app,
            crossterm::event::KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            &mut test_terminal(),
        );
        assert_eq!(app.chat.focus, focus, "{focus:?}");
        assert!(
            app.chat.text().is_empty(),
            "{focus:?}: scroll key must not reach the editor"
        );
    }
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
    // Enough messages to overflow the 24-row pane, so the rendered
    // scroll maximum (last_max_scroll) is non-zero.
    for i in 0..100 {
        app.chat.messages.push(ChatMessage {
            sender: "user".into(),
            text: format!("msg {i}"),
            timestamp: Some("2026-01-01T00:00:00Z".into()),
        });
    }

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
    // Enough messages to overflow the pane, so the rendered scroll
    // maximum (last_max_scroll) is non-zero.
    for i in 0..100 {
        app.chat.messages.push(ChatMessage {
            sender: "user".into(),
            text: format!("msg {i}"),
            timestamp: Some("2026-01-01T00:00:00Z".into()),
        });
    }

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
    for i in 0..100 {
        app2.chat.messages.push(ChatMessage {
            sender: "user".into(),
            text: format!("msg {i}"),
            timestamp: Some("2026-01-01T00:00:00Z".into()),
        });
    }
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

/// Flatten wrapped lines to plain strings for assertions.
fn line_texts(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
        .collect()
}

#[test]
fn wrap_styled_lines_short_line_passes_through() {
    let out = wrap_styled_lines(vec![Line::from("hello")], 80);
    assert_eq!(line_texts(&out), vec!["hello"]);
}

#[test]
fn wrap_styled_lines_word_wraps_at_spaces() {
    // "hello world" fills row 1 exactly; the break drops the space
    // after "hello"; "world again" (11 cols) then fills row 2 exactly.
    let out = wrap_styled_lines(vec![Line::from("hello world again")], 11);
    assert_eq!(line_texts(&out), vec!["hello", "world again"]);
}

#[test]
fn wrap_styled_lines_hard_splits_long_words() {
    let out = wrap_styled_lines(vec![Line::from("abcdefghij")], 4);
    assert_eq!(line_texts(&out), vec!["abcd", "efgh", "ij"]);
}

#[test]
fn wrap_styled_lines_wide_chars_count_two_columns() {
    let out = wrap_styled_lines(vec![Line::from("你好你好")], 4);
    assert_eq!(line_texts(&out), vec!["你好", "你好"]);
}

#[test]
fn wrap_styled_lines_preserves_styles_across_breaks() {
    let red = Style::default().fg(Color::Red);
    let line = Line::from(vec![
        Span::styled("hello ", Style::default()),
        Span::styled("world", red),
    ]);
    let out = wrap_styled_lines(vec![line], 8);
    assert_eq!(line_texts(&out), vec!["hello", "world"]);
    // The styled word keeps its style after being wrapped onto row 2.
    assert_eq!(out[1].spans[0].style, red);
    // Adjacent same-style cells merge into a single span.
    assert_eq!(out[1].spans.len(), 1);
}

#[test]
fn wrap_styled_lines_preserves_blank_lines() {
    let out = wrap_styled_lines(vec![Line::from("a"), Line::default(), Line::from("b")], 80);
    assert_eq!(line_texts(&out), vec!["a", "", "b"]);
}

#[test]
fn wrap_styled_lines_preserves_line_style_on_every_row() {
    // tui-markdown puts heading/blockquote styling on `Line::style`
    // (spans unstyled), so every wrapped row must carry it.
    let bold = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
    let line = Line::styled("# heading text here", bold);
    let out = wrap_styled_lines(vec![line], 10);
    assert_eq!(line_texts(&out), vec!["# heading", "text here"]);
    assert!(out.iter().all(|l| l.style == bold));
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
fn opens_with_info_and_status_visible() {
    // Thread info pane and status bar default to visible; activity,
    // explorer and zen mode stay opt-in.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let app = App::new(rx, None);
    assert!(app.chat.info_visible);
    assert!(app.chat.status_visible);
    assert_eq!(app.chat.activity_split, 0);
    assert!(!app.chat.explorer_visible);
    assert!(app.chat.zen_saved.is_none());
}

#[test]
fn zen_mode_restores_explorer() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);

    // Explorer open alongside the default-visible info pane.
    app.chat.toggle_explorer();
    assert!(app.chat.explorer_visible);
    assert!(app.chat.info_visible);

    // Enter zen → both hidden.
    app.chat.toggle_zen_mode();
    assert!(!app.chat.explorer_visible);
    assert!(!app.chat.info_visible);

    // Exit zen → snapshot restored: explorer and info both back.
    app.chat.toggle_zen_mode();
    assert!(app.chat.explorer_visible);
    assert!(app.chat.info_visible);
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
    app.chat.info_visible = false; // isolate explorer cycling

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
fn zen_mode_restores_info_status_and_activity() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    // Default: info+status visible, activity hidden.
    assert!(app.chat.info_visible);
    assert!(app.chat.status_visible);
    assert_eq!(app.chat.activity_split, 0);

    // User opens activity via the leader popup (`Ctrl+P` then `a`).
    app.chat.toggle_activity();
    assert_eq!(app.chat.activity_split, 1);

    // Ctrl+P z → enter zen mode: info, status AND activity hidden.
    app.chat.toggle_zen_mode();
    assert!(!app.chat.info_visible);
    assert!(!app.chat.status_visible);
    assert_eq!(app.chat.activity_split, 0);

    // Ctrl+P z again → exit zen mode: the full snapshot is
    // restored, including the activity pane.
    app.chat.toggle_zen_mode();
    assert!(app.chat.info_visible);
    assert!(app.chat.status_visible);
    assert_eq!(app.chat.activity_split, 1);
}

#[test]
fn toggle_status_bar_independent_of_info_pane() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.toggle_status_bar();
    assert!(!app.chat.status_visible);
    assert!(app.chat.info_visible, "info pane must not follow status");
    app.chat.toggle_status_bar();
    assert!(app.chat.status_visible);
}

#[test]
fn toggle_info_pane_independent_and_refocuses() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.toggle_info_pane();
    assert!(!app.chat.info_visible);
    assert!(app.chat.status_visible, "status bar must not follow info");

    // Hiding the info pane while focused moves focus to the chat pane.
    app.chat.toggle_info_pane();
    app.chat.focus = ChatFocus::InfoPane;
    app.chat.toggle_info_pane();
    assert!(!app.chat.info_visible);
    assert_eq!(app.chat.focus, ChatFocus::ChatPane);
}

#[test]
fn zen_exit_restores_snapshot_over_in_zen_toggles() {
    // Documented edge: panes toggled individually while in zen are
    // discarded in favor of the pre-zen snapshot.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.toggle_status_bar(); // status hidden before zen
    app.chat.toggle_zen_mode(); // snapshot: status=false
    app.chat.toggle_status_bar(); // in-zen toggle: status=true
    assert!(app.chat.status_visible);
    app.chat.toggle_zen_mode(); // exit → snapshot wins
    assert!(!app.chat.status_visible);
    assert!(app.chat.zen_saved.is_none());
}

#[test]
fn toggle_resets_after_zen_mode() {
    // Regression: after zen mode hides the activity pane, the next toggle
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

#[test]
fn softbreaks_become_hardbreaks_outside_fences() {
    assert_eq!(
        softbreaks_to_hardbreaks("first\nsecond\n"),
        "first  \nsecond  \n"
    );
    // No trailing newline: last line is left as-is.
    assert_eq!(softbreaks_to_hardbreaks("a\nb"), "a  \nb");
}

#[test]
fn softbreaks_untouched_inside_fences() {
    let md = "before\n```rust\nlet x = 1;\nlet y = 2;\n```\nafter\n";
    assert_eq!(
        softbreaks_to_hardbreaks(md),
        "before  \n```rust\nlet x = 1;\nlet y = 2;\n```\nafter  \n"
    );
    // Tilde fences are recognized too.
    assert_eq!(
        softbreaks_to_hardbreaks("~~~\ncode\n~~~\n"),
        "~~~\ncode\n~~~\n"
    );
}

#[test]
fn transformed_message_renders_on_two_lines() {
    // End-to-end pin on the production render path: a two-line chat
    // message must emit two lines (regression: soft break → space).
    let md = softbreaks_to_hardbreaks("one\ntwo\n");
    let text = tui_markdown::from_str_with_options(&md, &chat_markdown_options());
    assert_eq!(text.lines.len(), 2);
}

#[test]
fn code_fence_renders_with_highlight_colors() {
    // Pin: chat render options keep syntect highlighting active (24-bit
    // colors come through as Rgb spans).
    let text = tui_markdown::from_str_with_options(
        "```rust\nfn main() {}\n```\n",
        &chat_markdown_options(),
    );
    let has_rgb_fg = text
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| matches!(span.style.fg, Some(Color::Rgb(..))));
    assert!(has_rgb_fg, "code fence produced no highlighted spans");
}
