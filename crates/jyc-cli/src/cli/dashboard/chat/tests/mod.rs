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
    assert_eq!(format_elapsed_ms(3_600_000), "1h00m00s");
}

#[test]
fn format_elapsed_keeps_seconds_at_and_above_60s() {
    // Timestamps relative to now; `num_seconds` truncation absorbs the
    // sub-second drift between fixture creation and the call.
    let ago = |secs: i64| Some((chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339());
    assert_eq!(format_elapsed(&None), "");
    assert_eq!(format_elapsed(&Some("not-a-date".to_string())), "");
    assert_eq!(format_elapsed(&ago(0)), "0s");
    assert_eq!(format_elapsed(&ago(59)), "59s");
    assert_eq!(format_elapsed(&ago(60)), "1m00s");
    assert_eq!(format_elapsed(&ago(65)), "1m05s");
    assert_eq!(format_elapsed(&ago(125)), "2m05s");
    assert_eq!(format_elapsed(&ago(3599)), "59m59s");
    assert_eq!(format_elapsed(&ago(3600)), "1h00m");
    assert_eq!(format_elapsed(&ago(7387)), "2h03m");
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
        "topic": "t1",
        "elapsed_ms": 12_400,
    });
    chat.handle_live_event(&payload);
    assert_eq!(chat.live_tick_ms_for("chan", "t1"), Some(12_400));
    assert_eq!(chat.live_tick_ms_for("chan", "missing"), None);

    // `processing: false` should clear the tick (mirror of new-round).
    chat.handle_live_event(&serde_json::json!({
        "type": "processing",
        "channel": "chan",
        "topic": "t1",
        "is_processing": false,
        "has_error": false,
    }));
    assert_eq!(chat.live_tick_ms_for("chan", "t1"), None);

    // And a second tick updates the value.
    chat.handle_live_event(&serde_json::json!({
        "type": "loop_tick",
        "channel": "chan",
        "topic": "t1",
        "elapsed_ms": 7_500,
    }));
    assert_eq!(chat.live_tick_ms_for("chan", "t1"), Some(7_500));
}

#[test]
fn select_pattern_clears_chat_messages() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);

    // Simulate messages from a previous topic
    app.chat.messages.push(ChatMessage {
        sender: "user".to_string(),
        text: "hello from topic A".to_string(),
        timestamp: None,
    });
    app.chat.messages.push(ChatMessage {
        sender: "ai".to_string(),
        text: "reply from topic A".to_string(),
        timestamp: None,
    });
    assert_eq!(app.chat.messages.len(), 2);

    // Switch to a new topic
    app.chat.select_pattern_inner("topic-b".to_string());

    // Messages must be cleared so stale content doesn't leak across topics
    assert!(app.chat.messages.is_empty());
    assert_eq!(app.chat.topic.as_deref(), Some("topic-b"));
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
fn submit_while_browsing_resets_history_pos_and_recall_returns_most_recent() {
    // Regression test for PR #655: after a user navigates into history and
    // then submits the recalled text, the next Up arrow must show the message
    // just sent, not jump to a stale cursor position in the now-larger
    // history (which previously surfaced the second-to-last entry).
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);

    app.chat.input_history = vec![
        "first".to_string(),
        "second".to_string(),
        "third".to_string(),
    ];

    // Navigate into history: Up → "third" (newest), Up → "second".
    app.chat.recall_older();
    assert_eq!(app.chat.text(), "third");
    app.chat.recall_older();
    assert_eq!(app.chat.text(), "second");
    assert_eq!(app.chat.history_pos, Some(1));

    // User submits the recalled "second" entry.
    app.chat.send_message();

    // history_pos must be reset so the next Up starts fresh from len.
    assert_eq!(app.chat.history_pos, None);

    // Next Up must show the just-pushed entry ("second"), not "first".
    app.chat.recall_older();
    assert_eq!(app.chat.text(), "second");
}

#[test]
fn submit_empty_text_does_not_touch_history_pos() {
    // Regression guard: the empty-text early return in send_message_inner
    // must not invalidate the browsing cursor. (Useful for slash command
    // flows where the editor clears without a real submission.)
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);

    // Editor is empty (App::new). Pretend the user is mid-browse.
    app.chat.input_history = vec!["a".to_string(), "b".to_string()];
    app.chat.history_pos = Some(1);

    // Enter on empty editor → send_message_inner("") → early return.
    app.chat.send_message();

    // history_pos must NOT be reset (we returned before the reset).
    assert_eq!(app.chat.history_pos, Some(1));
    assert_eq!(app.chat.input_history, vec!["a", "b"]);
}

#[test]
fn select_pattern_clears_input_history() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);

    app.chat.input_history = vec!["msg from topic A".to_string()];
    app.chat.history_pos = Some(0);

    // Switch to a new topic
    app.chat.select_pattern_inner("topic-b".to_string());

    // History must be cleared so it doesn't leak across topics
    assert!(app.chat.input_history.is_empty());
    assert!(app.chat.history_pos.is_none());
}

#[test]
fn clear_live_transient_removes_stale_state_for_switched_topic() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);

    // Stale entries from earlier watches of two topics:
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
    // Other topics' live state is untouched.
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
    app.chat.topic = Some("jyc".to_string());
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
    app.chat.topic = Some("jyc".to_string());
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
    app.chat.topic = Some("jyc".to_string());
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
    app.chat.topic = Some("jyc".to_string());
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
    app.chat.topic = Some("jyc".to_string());
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
    app.chat.topic = Some("jyc".to_string());
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
    app2.chat.topic = Some("jyc".to_string());
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
    app.chat.topic = Some("jyc".to_string());

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
    app.chat.topic = Some("jyc".to_string());
    app.chat.focus = ChatFocus::ChatPane;
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    app.chat.ws_tx = Some(cmd_tx);

    assert!(app.chat.visible);
    assert_eq!(app.chat.phase, ChatPhase::Chatting);
    assert_eq!(app.chat.topic.as_deref(), Some("jyc"));

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
        "topic": "pr-1",
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
        "topic": "pr-1",
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
        "topic": "pr-1",
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
        "topic": "pr-1",
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
        "topic": "pr-1",
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

// ---------------------------------------------------------------------------
// Regression tests for the chat-pane poll-sync dedup
// (`ChatState::poll_sync_live_chat`).
//
// Bug history: the original `(sender, text)` dedup silently dropped repeated
// runs of `/context` and similar deterministic-output commands. Fixing that
// with an id-based dedup exposed a second bug: historical rows from
// `chat_log_store.rs` JSONL hydrate carry `id = 0`, so a naive id-tracker
// treated them as never-pushed and re-appended them on every 500 ms poll
// cycle, flooding `self.messages` and pushing the live content off-screen.
//
// `poll_sync_live_chat` implements three dedup rules:
//   1. Live entries (`id != 0`): skip if already pushed (`id <= last_pushed`).
//   2. User echoes (`sender == "user"`): dedup by `(sender, text)` so the
//      server's IncomingMessage echo is dropped against the local echo from
//      `send_message_inner`.
//   3. Historical rows (`id == 0`): dedup by `(sender, text)` so the poll
//      loop does not re-push them forever.
// ---------------------------------------------------------------------------

fn push_live_chat(
    app: &mut App,
    channel: &str,
    topic: &str,
    entries: Vec<jyc_types::ChatMessageEntry>,
) {
    use std::collections::VecDeque;
    let key = (channel.to_string(), topic.to_string());
    app.chat.live_chat.insert(key, VecDeque::from(entries));
}

#[test]
fn poll_sync_appends_each_live_entry_by_unique_id() {
    // Two live AI replies with byte-identical text but distinct monotonic
    // ids must both land in `self.messages` — this is the original bug
    // that motivated the fix.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    let text = "/context: current strategy is sliding_window (window=10) (default)".to_string();
    push_live_chat(
        &mut app,
        "agents",
        "t",
        vec![
            jyc_types::ChatMessageEntry {
                sender: "ai".into(),
                text: text.clone(),
                timestamp: Some("2026-01-01T00:00:01Z".into()),
                id: 2,
            },
            jyc_types::ChatMessageEntry {
                sender: "ai".into(),
                text,
                timestamp: Some("2026-01-01T00:00:02Z".into()),
                id: 4,
            },
        ],
    );
    assert!(app.chat.poll_sync_live_chat("agents", "t"));
    assert_eq!(app.chat.messages.len(), 2);
    // Second sync: nothing new; tracker stays at id 4.
    assert!(!app.chat.poll_sync_live_chat("agents", "t"));
    assert_eq!(app.chat.messages.len(), 2);
}

#[test]
fn poll_sync_does_not_repush_historical_id_zero_rows_across_polls() {
    // 5 identical historical AI rows (id == 0) flushed by REST hydrate.
    // After 10 sync cycles `self.messages` must still contain exactly one
    // copy — the bug that produced the `id == 0` flood.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    let text = "/context: current strategy is sliding_window (window=10) (default)".to_string();
    let rows = (0..5)
        .map(|i| jyc_types::ChatMessageEntry {
            sender: "ai".into(),
            text: text.clone(),
            timestamp: Some(format!("2026-01-01T00:00:0{i}Z")),
            id: 0,
        })
        .collect();
    push_live_chat(&mut app, "agents", "t", rows);
    for _ in 0..10 {
        app.chat.poll_sync_live_chat("agents", "t");
    }
    assert_eq!(
        app.chat.messages.len(),
        1,
        "historical id=0 row must not be re-pushed across polls"
    );
}

#[test]
fn poll_sync_keeps_distinct_historical_texts() {
    // Distinct historical texts must all be kept — `id == 0` should not
    // collapse every historical row to one.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    push_live_chat(
        &mut app,
        "agents",
        "t",
        vec![
            jyc_types::ChatMessageEntry {
                sender: "ai".into(),
                text: "first ai reply".into(),
                timestamp: Some("2026-01-01T00:00:00Z".into()),
                id: 0,
            },
            jyc_types::ChatMessageEntry {
                sender: "ai".into(),
                text: "second ai reply".into(),
                timestamp: Some("2026-01-01T00:00:01Z".into()),
                id: 0,
            },
            jyc_types::ChatMessageEntry {
                sender: "user".into(),
                text: "user said something".into(),
                timestamp: Some("2026-01-01T00:00:02Z".into()),
                id: 0,
            },
        ],
    );
    assert!(app.chat.poll_sync_live_chat("agents", "t"));
    assert_eq!(app.chat.messages.len(), 3);
    assert!(!app.chat.poll_sync_live_chat("agents", "t"));
    assert_eq!(app.chat.messages.len(), 3);
}

#[test]
fn poll_sync_local_user_echo_and_server_echo_are_deduped() {
    // `send_message_inner` pushes the local echo with no id; the server
    // echoes back via `live_chat` with id > 0. The local echo must stay,
    // the server echo must be dropped — this is rule (2).
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    // Local echo.
    app.chat.send_message_inner("/hello".into());
    // Server echoes back via live_chat.
    push_live_chat(
        &mut app,
        "agents",
        "t",
        vec![jyc_types::ChatMessageEntry {
            sender: "user".into(),
            text: "/hello".into(),
            timestamp: Some("2026-01-01T00:00:00.500Z".into()),
            id: 7,
        }],
    );
    app.chat.poll_sync_live_chat("agents", "t");
    let user_msgs: Vec<&ChatMessage> = app
        .chat
        .messages
        .iter()
        .filter(|m| m.sender == "user")
        .collect();
    assert_eq!(
        user_msgs.len(),
        1,
        "local echo must win, server echo dropped"
    );
}

#[test]
fn poll_sync_command_repeats_with_historical_backdrop() {
    // The user's exact scenario: 30 historical rows (id == 0) plus 3
    // fresh `/context` commands. All 3 user echoes + 3 AI replies must
    // be present, and the historical rows must not be re-pushed across
    // polls.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    // Seed 30 historical rows: 15 user + 15 ai, all id == 0, distinct text.
    let mut hist: Vec<jyc_types::ChatMessageEntry> = (0..15)
        .flat_map(|i| {
            [
                jyc_types::ChatMessageEntry {
                    sender: "user".into(),
                    text: format!("old-user-{i}"),
                    timestamp: Some(format!("2026-01-01T00:00:{i:02}Z")),
                    id: 0,
                },
                jyc_types::ChatMessageEntry {
                    sender: "ai".into(),
                    text: format!("old-ai-{i}"),
                    timestamp: Some(format!("2026-01-01T00:01:{i:02}Z")),
                    id: 0,
                },
            ]
        })
        .collect();
    push_live_chat(&mut app, "agents", "t", hist.clone());

    // User types /context three times. Each typing pushes a local echo;
    // each server reply arrives via live_chat with a fresh id.
    let ai_text = "/context: current strategy is sliding_window (window=10) (default)".to_string();
    for round in 0..3 {
        app.chat.send_message_inner("/context".into());
        hist.push(jyc_types::ChatMessageEntry {
            sender: "user".into(),
            text: "/context".into(),
            timestamp: Some(format!("2026-02-01T00:0{round}:00Z")),
            id: 100 + round as u64 * 2,
        });
        hist.push(jyc_types::ChatMessageEntry {
            sender: "ai".into(),
            text: ai_text.clone(),
            timestamp: Some(format!("2026-02-01T00:0{round}:01Z")),
            id: 101 + round as u64 * 2,
        });
        // Reset live_chat buffer to the new full set so each round
        // simulates the live state after the server has flushed events.
        use std::collections::VecDeque;
        let key = ("agents".to_string(), "t".to_string());
        app.chat.live_chat.insert(key, VecDeque::from(hist.clone()));
    }
    // One more sync to pick up the last batch.
    app.chat.poll_sync_live_chat("agents", "t");
    // Two extra sync cycles must not change the message count.
    app.chat.poll_sync_live_chat("agents", "t");
    app.chat.poll_sync_live_chat("agents", "t");

    let user_msgs: Vec<&ChatMessage> = app
        .chat
        .messages
        .iter()
        .filter(|m| m.sender == "user")
        .collect();
    let ai_msgs: Vec<&ChatMessage> = app
        .chat
        .messages
        .iter()
        .filter(|m| m.sender == "ai")
        .collect();
    assert_eq!(
        user_msgs.len(),
        15 + 3,
        "15 historical user rows + 3 fresh /context echoes"
    );
    assert_eq!(
        ai_msgs.len(),
        15 + 3,
        "15 historical ai rows + 3 fresh /context replies"
    );
}

#[test]
fn seed_live_resets_last_pushed_chat_id_so_revisit_rehydrates() {
    // The bug fixed by `seed_live` resetting `last_pushed_chat_id` to 0:
    // when the user closes and reopens a topic, the egress tracker still
    // holds the previous visit's max id. Without the reset, the freshly
    // hydrated historical rows (whose ids ≤ old max) would all be
    // skipped by rule (1), so the chat pane would show only new live
    // entries — no historical context.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    let historical: Vec<jyc_types::ChatMessageEntry> = (0..10)
        .map(|i| jyc_types::ChatMessageEntry {
            sender: if i % 2 == 0 {
                "user".into()
            } else {
                "ai".into()
            },
            text: format!("history-{i}"),
            timestamp: Some(format!("2026-01-01T00:00:{i:02}Z")),
            id: i as u64 + 1,
        })
        .collect();
    // First visit: seed + sync.
    app.chat
        .seed_live("agents", "t", vec![], historical.clone());
    app.chat.poll_sync_live_chat("agents", "t");
    assert_eq!(app.chat.messages.len(), 10);
    // The egress tracker now sits at the max historical id (10).
    assert_eq!(
        app.chat
            .last_pushed_chat_id
            .get(&("agents".into(), "t".into()))
            .copied(),
        Some(10)
    );
    // Second visit: simulate `open()` clearing `messages` + a fresh
    // REST hydrate via `seed_live`. The tracker must be reset so the
    // hydrated historicals are re-pushed.
    app.chat.messages.clear();
    app.chat
        .seed_live("agents", "t", vec![], historical.clone());
    assert_eq!(
        app.chat
            .last_pushed_chat_id
            .get(&("agents".into(), "t".into()))
            .copied(),
        Some(0),
        "seed_live must reset the egress tracker so the freshly hydrated rows are not skipped"
    );
    app.chat.poll_sync_live_chat("agents", "t");
    assert_eq!(
        app.chat.messages.len(),
        10,
        "all 10 hydrated historical rows must be re-pushed on revisit"
    );
}

#[cfg(test)]
mod part2;
