use super::render::{USER_BG, history_fingerprint, render_history_lines};
use super::*;
use crate::cli::command_popup::popup_height;

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
        history_fingerprint(&msgs, 80, false, false),
        history_fingerprint(&msgs, 80, false, false)
    );
}

#[test]
fn history_fingerprint_changes_on_message_mutations() {
    let msgs = vec![
        history_msg("user", "hello", Some("2026-08-13T10:00:00Z")),
        history_msg("ai", "world", Some("2026-08-13T10:00:05Z")),
    ];
    let base = history_fingerprint(&msgs, 80, false, false);

    // New message pushed.
    let mut pushed = msgs.clone();
    pushed.push(history_msg("user", "again", None));
    assert_ne!(base, history_fingerprint(&pushed, 80, false, false));

    // Streaming append to the last message's text.
    let mut streamed = msgs.clone();
    streamed[1].text.push_str(" more");
    assert_ne!(base, history_fingerprint(&streamed, 80, false, false));

    // Last message timestamp set after the fact.
    let mut stamped = msgs.clone();
    stamped[1].timestamp = Some("2026-08-13T10:00:06Z".to_string());
    assert_ne!(base, history_fingerprint(&stamped, 80, false, false));

    // A message flipping side: same text, count and timestamps, but the
    // background block now belongs to a different line.
    let mut flipped = msgs.clone();
    flipped[0].sender = "ai".to_string();
    assert_ne!(base, history_fingerprint(&flipped, 80, false, false));

    // Cleared history.
    assert_ne!(base, history_fingerprint(&[], 80, false, false));

    // Pane resize (re-wrap needed).
    assert_ne!(base, history_fingerprint(&msgs, 100, false, false));

    // Minimal progress mode drops the thinking line, so flipping it has to
    // invalidate the cache the same way `T` does — otherwise the cached
    // history keeps the old rows and the toggle looks like a no-op.
    let minimal = history_fingerprint(&msgs, 80, false, true);
    assert_ne!(base, minimal);
    assert_ne!(
        history_fingerprint(&msgs, 80, true, true),
        history_fingerprint(&msgs, 80, true, false),
        "the flag joins the fingerprint alongside the thinking toggle"
    );
}

#[test]
fn render_history_lines_deterministic_for_cache_reuse() {
    let msgs = vec![
        history_msg("user", "hello **bold**", Some("2026-08-13T10:00:00Z")),
        history_msg("ai", "world\nsecond line", Some("2026-08-13T10:00:05Z")),
    ];
    assert_eq!(
        render_history_lines(&msgs, 80, false, false),
        render_history_lines(&msgs, 80, false, false)
    );
    // Different width re-wraps — cache must not be reused.
    assert_ne!(
        render_history_lines(&msgs, 80, false, false),
        render_history_lines(&msgs, 20, false, false)
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
        .insert(done.clone(), vec!["old thinking".to_string()]);
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

/// The `/` popup has no input box of its own: text keys go to the chat
/// input field and the popup's filter mirrors whatever it holds.
#[test]
fn command_popup_filters_off_the_chat_input_field() {
    let mut app = chatting_app();
    let key = |code: KeyCode| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);

    handle_chat_keys(&mut app, key(KeyCode::Char('/')), &mut test_terminal());
    assert!(app.chat.command_popup.is_some(), "'/' opens the popup");
    assert_eq!(app.chat.text(), "/", "the slash lands in the input field");

    handle_chat_keys(&mut app, key(KeyCode::Char('p')), &mut test_terminal());
    handle_chat_keys(&mut app, key(KeyCode::Char('l')), &mut test_terminal());
    assert_eq!(
        app.chat.text(),
        "/pl",
        "typing continues in the input field"
    );
    let popup = app.chat.command_popup.as_ref().expect("popup stays open");
    assert_eq!(popup.filter, "/pl", "filter mirrors the input field");

    // Backspacing past the slash empties the field and dismisses the popup.
    for _ in 0..3 {
        handle_chat_keys(&mut app, key(KeyCode::Backspace), &mut test_terminal());
    }
    assert!(app.chat.text().is_empty());
    assert!(
        app.chat.command_popup.is_none(),
        "an empty input field closes the popup"
    );
}

/// Enter sends the highlighted command and leaves the field clean: the text
/// that filtered the list must not sit in the input field waiting to be
/// sent a second time.
#[test]
fn command_popup_send_clears_the_input_field() {
    let mut app = chatting_app();
    let key = |code: KeyCode| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
    handle_chat_keys(&mut app, key(KeyCode::Char('/')), &mut test_terminal());
    handle_chat_keys(&mut app, key(KeyCode::Enter), &mut test_terminal());

    assert!(app.chat.command_popup.is_none());
    assert!(
        app.chat.text().is_empty(),
        "no residue after sending, field holds {:?}",
        app.chat.text()
    );
    let sent = app.chat.messages.last().expect("command echoed locally");
    assert!(sent.text.starts_with('/'), "sent {:?}", sent.text);
}

/// Tab writes the completion into the field and re-filters in the same
/// keypress — the list must not lag one frame behind the field.
#[test]
fn command_popup_tab_completion_refilters_immediately() {
    let mut app = chatting_app();
    let key = |code: KeyCode| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
    handle_chat_keys(&mut app, key(KeyCode::Char('/')), &mut test_terminal());
    // Set after `/`: opening the popup refreshes the command list, and the
    // offline fallback carries no argument values.
    app.chat.commands = vec![CommandInfo {
        name: "/model".to_string(),
        description: "Switch AI model for this topic".to_string(),
        args: vec![jyc_types::CommandArg {
            value: "deepseek/deepseek-chat".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    }];
    handle_chat_keys(&mut app, key(KeyCode::Tab), &mut test_terminal());

    assert_eq!(app.chat.text(), "/model ", "Tab leaves a space");
    let popup = app
        .chat
        .command_popup
        .as_ref()
        .expect("the space opens the value level");
    assert_eq!(popup.filter, app.chat.text(), "list follows the completion");
    assert_eq!(popup.selected, 0, "a new level restarts at the top");
}

/// A command with nothing below it: the space Tab leaves behind is also what
/// drops the popup, so one Tab completes and dismisses.
#[test]
fn tab_on_a_command_without_values_closes_the_popup() {
    let mut app = chatting_app();
    let key = |code: KeyCode| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
    handle_chat_keys(&mut app, key(KeyCode::Char('/')), &mut test_terminal());
    app.chat.commands = vec![CommandInfo {
        name: "/plan".to_string(),
        description: "Switch to plan mode (read-only)".to_string(),
        ..Default::default()
    }];
    handle_chat_keys(&mut app, key(KeyCode::Tab), &mut test_terminal());

    assert_eq!(app.chat.text(), "/plan ");
    assert!(
        app.chat.command_popup.is_none(),
        "no values follow `/plan`, so the popup must be gone"
    );
}

/// `ctrl+p c` opens the same popup, which has no field of its own: with a
/// draft in the input field, further typing filters off that draft.
#[test]
fn leader_command_popup_filters_off_the_draft() {
    let mut app = chatting_app();
    app.chat.populate_editor("draft");
    execute_local_action(
        &mut app,
        &mut test_terminal(),
        local_commands::LocalAction::OpenCommandPopup,
    );
    assert!(
        !app.chat.commands.is_empty(),
        "the leader path loads the commands itself, or the popup renders Loading..."
    );
    let popup = app
        .chat
        .command_popup
        .as_ref()
        .expect("leader opens the popup");
    assert!(popup.filter.is_empty(), "opens with the full list");

    handle_chat_keys(
        &mut app,
        crossterm::event::KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
        &mut test_terminal(),
    );
    assert_eq!(app.chat.text(), "draft!", "the key went to the field");
    assert_eq!(
        app.chat.command_popup.as_ref().expect("still open").filter,
        "draft!",
        "the filter adopts the field"
    );
}

/// Draw `app` into the shared 80x24 test backend and hand back the cells.
fn draw_80x24(app: &mut App) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    terminal
        .draw(|frame| ui_chat_mode(frame, frame.area(), app))
        .expect("draw");
    terminal.backend().buffer().clone()
}

/// One screen row as a string, one char per cell.
fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol().to_string())
        .collect::<Vec<_>>()
        .join("")
}

/// The prompt row — the input field's last content row ("╰─❯ ").
fn prompt_row(buffer: &ratatui::buffer::Buffer) -> u16 {
    (0..buffer.area.height)
        .rev()
        .find(|&y| row_text(buffer, y).contains('❯'))
        .expect("prompt row rendered")
}

/// Column of `needle` in row `y`, counted in cells: a row string holds one
/// char per cell, so a byte offset would not survive the `→`.
fn col_of(buffer: &ratatui::buffer::Buffer, y: u16, needle: &str) -> u16 {
    let text = row_text(buffer, y);
    let byte = text.find(needle).expect("needle rendered");
    text[..byte].chars().count() as u16
}

/// The popup renders directly BELOW the input field: a borderless top rule
/// on the row after the prompt, with the list under it — so it can never
/// cover the text being typed.
#[test]
fn command_popup_renders_below_the_input_field() {
    let mut app = chatting_app();
    // Full-width chat pane: no info pane whose own border could land on the
    // row under test.
    app.chat.info_visible = false;
    handle_chat_keys(
        &mut app,
        crossterm::event::KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        &mut test_terminal(),
    );
    let buffer = draw_80x24(&mut app);

    let prompt = prompt_row(&buffer);
    assert!(
        row_text(&buffer, prompt).contains('/'),
        "the input field must stay visible: {prompt}"
    );
    let rule = row_text(&buffer, prompt + 1);
    assert!(rule.contains("Commands"), "top rule missing: {rule:?}");
    assert!(
        rule.contains('─') && !rule.contains('│'),
        "expected a side-border-free rule, got: {rule:?}"
    );
    assert!(
        rule.chars().filter(|c| *c == '─').count() > 50,
        "the rule should stretch across the pane: {rule:?}"
    );
    assert!(
        row_text(&buffer, prompt + 2).contains('/'),
        "command list should follow the rule"
    );
}

/// The popup follows the command tree out: once the text reaches an argument
/// position with nothing to complete (`/plan <free text>`), the popup closes
/// and the field behaves as if it had never opened.
#[test]
fn command_popup_closes_on_a_free_text_argument() {
    let mut app = chatting_app();
    let key = |code: KeyCode| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);

    for c in "/plan".chars() {
        handle_chat_keys(&mut app, key(KeyCode::Char(c)), &mut test_terminal());
    }
    assert!(
        app.chat.command_popup.is_some(),
        "the root level is still completable"
    );

    handle_chat_keys(&mut app, key(KeyCode::Char(' ')), &mut test_terminal());
    assert_eq!(app.chat.text(), "/plan ");
    assert!(
        app.chat.command_popup.is_none(),
        "/plan declares no argument values, so the popup goes away"
    );
}

/// A command's argument level renders the same way: the rule names the
/// level, the rows are its values, still directly below the input field.
#[test]
fn command_popup_renders_a_deeper_level() {
    let mut app = chatting_app();
    app.chat.info_visible = false;
    let char_key = |c: char| crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    handle_chat_keys(&mut app, char_key('/'), &mut test_terminal());
    // What a server sends for a `/model` topic: the picker's values ride on
    // the command itself, so the popup needs no knowledge of `/model`.
    app.chat.commands = vec![CommandInfo {
        name: "/model".to_string(),
        description: "Switch AI model for this topic".to_string(),
        args: vec![jyc_types::CommandArg {
            value: "deepseek/deepseek-chat".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    }];
    for c in "model ".chars() {
        handle_chat_keys(&mut app, char_key(c), &mut test_terminal());
    }
    let buffer = draw_80x24(&mut app);

    let prompt = prompt_row(&buffer);
    let rule = row_text(&buffer, prompt + 1);
    assert!(
        rule.contains("/model"),
        "the rule should name the level: {rule:?}"
    );
    assert!(
        row_text(&buffer, prompt + 2).contains("deepseek/deepseek-chat"),
        "the level's values should follow: {:?}",
        row_text(&buffer, prompt + 2)
    );
}

/// A list deeper than the popup's rows scrolls to follow the cursor, so the
/// tail is reachable. Before this the whole list went into a fixed-height
/// `Paragraph`: the arrow walked down past the clipped rows, stopped being
/// visible, and Enter fired a command the user could not see.
#[test]
fn command_popup_scrolls_to_keep_the_cursor_visible() {
    let mut app = chatting_app();
    app.chat.info_visible = false;
    let key = |code: KeyCode| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
    handle_chat_keys(&mut app, key(KeyCode::Char('/')), &mut test_terminal());
    // A list deeper than the popup reserves, set after `/` because opening
    // refreshes the commands from the topic.
    app.chat.commands = (0..21)
        .map(|i| CommandInfo {
            name: format!("/c{i}"),
            description: "row".to_string(),
            ..Default::default()
        })
        .collect();
    let rows = {
        let popup = app.chat.command_popup.as_ref().expect("popup open");
        // The top rule is not a list row.
        popup_height(popup, &app.chat.commands) as usize - 1
    };
    // All the way down: the last command is the one that used to be lost.
    for _ in 0..20 {
        handle_chat_keys(&mut app, key(KeyCode::Down), &mut test_terminal());
    }
    let buffer = draw_80x24(&mut app);

    let prompt = prompt_row(&buffer);
    let list: Vec<String> = ((prompt + 2)..=(prompt + 1 + rows as u16))
        .map(|y| row_text(&buffer, y))
        .collect();
    let popup = list.join("\n");

    assert_eq!(
        app.chat
            .command_popup
            .as_ref()
            .expect("popup still open")
            .selected,
        20
    );
    assert_eq!(list.len(), rows, "the popup shows its reserved rows");
    assert!(
        list[rows - 1].contains("→ /c20"),
        "the selected row must ride the bottom edge, not sit below the clip:\n{popup}"
    );
    assert!(
        list[0].contains("/c11"),
        "the window slides with the cursor:\n{popup}"
    );
    assert!(
        !popup.contains("/c10"),
        "the rows the cursor walked over must scroll away:\n{popup}"
    );
}

/// The focused row carries a `→` in the gutter and is dimmed — the popup
/// paints no highlight bar, so the arrow is the only cursor cue.
#[test]
fn command_popup_marks_the_selected_row_with_an_arrow() {
    let mut app = chatting_app();
    app.chat.info_visible = false;
    handle_chat_keys(
        &mut app,
        crossterm::event::KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        &mut test_terminal(),
    );
    let buffer = draw_80x24(&mut app);

    let row = prompt_row(&buffer) + 2;
    let selected = row_text(&buffer, row);
    assert!(
        selected.contains("→ "),
        "selected row needs the arrow: {selected:?}"
    );

    let x = col_of(&buffer, row, "→");
    assert_eq!(
        buffer[(x, row)].modifier,
        ratatui::style::Modifier::DIM,
        "the selected row is dimmed: {selected:?}"
    );
    assert_eq!(
        buffer[(x, row)].bg,
        Color::Reset,
        "the highlight bar is gone: {selected:?}"
    );

    // The arrow occupies exactly the blank gutter every other row has, so
    // the command names stay in one column as the cursor moves.
    let next = row_text(&buffer, row + 1);
    assert!(
        next.contains('/') && !next.contains('→'),
        "only the selected row gets the arrow: {next:?}"
    );
    assert_eq!(
        col_of(&buffer, row, "/"),
        col_of(&buffer, row + 1, "/"),
        "command names share a column"
    );
}

/// The `Select Pattern` list gets the same window as the popups: with more
/// patterns than rows the arrow rides the bottom edge instead of walking into
/// the clip. It used to draw every pattern into the pane, and its `Wrap` put a
/// long path on a second row — which moved the rows out from under the cursor.
#[test]
fn pattern_select_scrolls_to_keep_the_cursor_visible() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.phase = ChatPhase::PatternSelect;
    app.chat.patterns = (0..20).map(|i| format!("p{i:02}")).collect();
    app.chat.pattern_selected = 19;

    let (width, height) = (30, 8);
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_pattern_select(frame, frame.area(), &app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..height).map(|y| row_text(&buffer, y)).collect();
    let pane = rows.join("\n");

    // Borders::ALL: the inner rows are 1..=6, so the window's last row is 6.
    assert!(
        rows[6].contains("→ p19"),
        "the selected pattern must sit on the list's bottom row:\n{pane}"
    );
    assert!(
        rows[1].contains("p14"),
        "the window slides with the cursor, so the top row is no longer the \
         first pattern:\n{pane}"
    );
    assert!(
        !pane.contains("p0"),
        "the patterns the cursor walked over must scroll away:\n{pane}"
    );
}

/// A question with more options than the box has rows: the list scrolls to
/// follow the cursor, the option keeps its real number, and the hint below it
/// survives — the box is the one list whose rows are not all options.
#[test]
fn question_box_scrolls_to_keep_the_cursor_visible() {
    let mut app = chatting_app();
    app.chat.info_visible = false;
    app.chat.questions = vec![PendingQuestion {
        id: "q1".to_string(),
        topic: "jyc".to_string(),
        question: "Pick one?".to_string(),
        options: (0..20).map(|i| format!("opt{i:02}")).collect(),
        multi: false,
        selected: 19,
        marked: Vec::new(),
    }];
    let buffer = draw_80x24(&mut app);

    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| row_text(&buffer, y))
        .collect();
    let pane = rows.join("\n");
    let cursor_row = rows
        .iter()
        .find(|r| r.contains("opt19"))
        .expect("the selected option must be drawn, not clipped below the box");

    assert!(
        cursor_row.contains("→ 20. opt19"),
        "the last option must carry the cursor, numbered by its real index: \
         {cursor_row:?}"
    );
    assert!(
        !pane.contains("opt0"),
        "the options above the window must scroll away:\n{pane}"
    );
    assert!(
        pane.contains("Esc hide"),
        "the hint must still fit under the window:\n{pane}"
    );
}

/// With a batch on screen the border says which question the user is looking
/// at and how many there are — otherwise an answer that only settles one
/// question of three looks like the whole exchange is over.
#[test]
fn question_box_numbers_the_batch_in_its_title() {
    let mut app = chatting_app();
    app.chat.info_visible = false;
    app.chat.questions = vec![
        PendingQuestion {
            id: "q1".to_string(),
            topic: "jyc".to_string(),
            question: "Which sections?".to_string(),
            options: vec!["Added".to_string()],
            multi: false,
            selected: 0,
            marked: Vec::new(),
        },
        PendingQuestion {
            id: "q2".to_string(),
            topic: "jyc".to_string(),
            question: "Branch name?".to_string(),
            options: vec!["feat/x".to_string()],
            multi: false,
            selected: 0,
            marked: Vec::new(),
        },
    ];
    app.chat.question_index = 1;
    let buffer = draw_80x24(&mut app);

    let pane: String = (0..buffer.area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        pane.contains("2/2"),
        "the title must number the question:\n{pane}"
    );
    assert!(
        pane.contains("Branch name?") && !pane.contains("Which sections?"),
        "only the question under the cursor is drawn:\n{pane}"
    );
}

/// Inside a batch `Enter` advances rather than sending, and the hint is the
/// only place that says so. A box that claims "Enter send" mid-batch is a bug
/// report waiting to happen - the user thinks the answer left, and the tool
/// waits on the last question's timeout.
#[test]
fn a_batch_hint_says_enter_only_moves_to_the_next() {
    let mut app = chatting_app();
    app.chat.info_visible = false;
    app.chat.questions = vec![
        PendingQuestion {
            id: "q1".to_string(),
            topic: "jyc".to_string(),
            question: "Which sections?".to_string(),
            options: vec!["Added".to_string()],
            multi: false,
            selected: 0,
            marked: Vec::new(),
        },
        PendingQuestion {
            id: "q2".to_string(),
            topic: "jyc".to_string(),
            question: "Branch name?".to_string(),
            options: vec!["feat/x".to_string()],
            multi: false,
            selected: 0,
            marked: Vec::new(),
        },
    ];
    let buffer = draw_80x24(&mut app);

    let pane: String = (0..buffer.area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        pane.contains("Enter next"),
        "the hint must not promise a send mid-batch:\n{pane}"
    );

    // And a single question keeps its old wording - the hint is the only place
    // a lone Enter-send is stated.
    app.chat.questions.truncate(1);
    app.chat.question_index = 0;
    let single = draw_80x24(&mut app);
    let single_pane: String = (0..single.area.height)
        .map(|y| row_text(&single, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        single_pane.contains("Enter send"),
        "one question still means one Enter sends it:\n{single_pane}"
    );
}

/// The marks have to be readable on rows the cursor is not on, or multi-select
/// is just single-select with extra keystrokes.
#[test]
fn question_box_shows_marks_for_multi_select() {
    let mut app = chatting_app();
    app.chat.info_visible = false;
    app.chat.questions = vec![PendingQuestion {
        id: "q1".to_string(),
        topic: "jyc".to_string(),
        question: "Which?".to_string(),
        options: vec!["alpha".to_string(), "beta".to_string()],
        multi: true,
        selected: 0,
        marked: vec![1],
    }];
    let buffer = draw_80x24(&mut app);

    let pane: String = (0..buffer.area.height)
        .map(|y| row_text(&buffer, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        pane.contains("[x] 2. beta"),
        "a marked option carries a box even without the cursor:\n{pane}"
    );
    assert!(
        pane.contains("[ ] 1. alpha"),
        "an unmarked option says so:\n{pane}"
    );
}

/// The question box marks its selected option the same way, and follows the
/// cursor instead of always marking the first option.
#[test]
fn question_box_marks_the_selected_option_with_an_arrow() {
    let mut app = chatting_app();
    app.chat.info_visible = false;
    app.chat.questions = vec![PendingQuestion {
        id: "q1".to_string(),
        topic: "jyc".to_string(),
        question: "Pick one?".to_string(),
        options: vec!["alpha".to_string(), "beta".to_string()],
        multi: false,
        selected: 1,
        marked: Vec::new(),
    }];
    let buffer = draw_80x24(&mut app);

    let selected = (0..buffer.area.height)
        .find(|&y| row_text(&buffer, y).contains("2. beta"))
        .expect("option row rendered");
    let above = selected - 1;
    let line = row_text(&buffer, selected);
    assert!(
        line.contains("→ 2. beta"),
        "the selected option carries the arrow: {line:?}"
    );

    let x = col_of(&buffer, selected, "→");
    assert_eq!(
        buffer[(x, selected)].modifier,
        ratatui::style::Modifier::DIM,
        "the selected option is dimmed: {line:?}"
    );
    assert_eq!(
        buffer[(x, selected)].bg,
        Color::Reset,
        "the highlight bar is gone: {line:?}"
    );
    assert!(
        !row_text(&buffer, above).contains('→'),
        "the unselected option stays in the gutter: {:?}",
        row_text(&buffer, above)
    );
    // The blank gutter survives on the unselected row (`Wrap { trim: true }`
    // would strip it), so both option numbers land in the same column.
    assert_eq!(
        col_of(&buffer, selected, "2."),
        col_of(&buffer, above, "1."),
        "option numbers share a column"
    );
}

/// The ctrl+p leader gets the same treatment: top rule directly below the
/// input field, no side borders.
#[test]
fn leader_popup_renders_below_the_input_field() {
    let mut app = chatting_app();
    app.chat.info_visible = false;
    handle_chat_keys(
        &mut app,
        crossterm::event::KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &mut test_terminal(),
    );
    assert!(app.chat.leader.is_some());
    let buffer = draw_80x24(&mut app);

    let rule = row_text(&buffer, prompt_row(&buffer) + 1);
    assert!(rule.contains("Leader"), "top rule missing: {rule:?}");
    assert!(!rule.contains('│'), "no side borders expected: {rule:?}");
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
fn wheel_over_info_pane_scrolls_the_info_pane() {
    // The wheel follows the cursor: over the info column it must scroll that
    // pane rather than silently doing nothing — and must leave the transcript
    // alone.
    let backend = ratatui::backend::TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.phase = ChatPhase::Chatting;
    app.chat.info_visible = true;
    app.chat.topic = Some("jyc".to_string());
    app.chat.focus = ChatFocus::ChatPane;

    terminal
        .draw(|f| ui_chat_mode(f, f.area(), &mut app))
        .unwrap();
    let rect = app
        .chat
        .last_info_area
        .expect("render should cache the info-pane rect");

    // Seeded after the draw on purpose: the renderer clamps the offset to the
    // pane's content, and with no topic loaded the pane holds a single line —
    // a pre-draw seed would be clamped away. This test is about where the wheel
    // goes, not how far the pane can scroll.
    app.chat.info_scroll = 5;

    handle_chat_mouse(
        &mut app,
        mouse_event(MouseEventKind::ScrollUp, rect.x + 1, rect.y + 1),
    );
    // The info pane has no cursor, so the wheel must scroll it *without*
    // moving focus: pulling it away from the editor mid-typing is the failure
    // this guards.
    assert!(
        matches!(app.chat.focus, ChatFocus::ChatPane),
        "scrolling the info pane must not take focus"
    );
    // Its offset counts from the top, so wheel-up moves earlier.
    assert_eq!(app.chat.info_scroll, 4);
    assert_eq!(app.chat.scroll, 0, "the transcript must not move");

    // A hidden info pane must not steal the wheel through its stale rect.
    app.chat.info_visible = false;
    handle_chat_mouse(
        &mut app,
        mouse_event(MouseEventKind::ScrollUp, rect.x + 1, rect.y + 1),
    );
    assert_eq!(app.chat.info_scroll, 4, "there is no pane there to scroll");
    assert_eq!(app.chat.scroll, 0, "and the transcript must still not move");
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
        Some(&["I am thinking about the problem".to_string()][..])
    );
}

#[test]
fn thinking_events_accumulate_instead_of_overwriting() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut chat = ChatState::new(rx);
    // The agent publishes the cumulative reasoning content of the current LLM
    // request, so a text extending the last block updates it in place.
    for text in ["first block", "first block grows", "first block grows more"] {
        chat.handle_live_event(&serde_json::json!({
            "type": "thinking",
            "channel": "chan",
            "topic": "t1",
            "text": text,
        }));
    }
    assert_eq!(
        chat.live_thinking_for("chan", "t1"),
        Some(&["first block grows more".to_string()][..])
    );
    // Text that does not extend the last block starts a new block.
    chat.handle_live_event(&serde_json::json!({
        "type": "thinking",
        "channel": "chan",
        "topic": "t1",
        "text": "second block",
    }));
    assert_eq!(
        chat.live_thinking_for("chan", "t1"),
        Some(
            &[
                "first block grows more".to_string(),
                "second block".to_string()
            ][..]
        )
    );
}

#[test]
fn processing_start_clears_accumulated_thinking_blocks() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut chat = ChatState::new(rx);
    chat.handle_live_event(&serde_json::json!({
        "type": "thinking",
        "channel": "chan",
        "topic": "t1",
        "text": "old round",
    }));
    chat.handle_live_event(&serde_json::json!({
        "type": "processing",
        "channel": "chan",
        "topic": "t1",
        "is_processing": true,
        "has_error": false,
    }));
    assert!(chat.live_thinking_for("chan", "t1").is_none());
}

#[test]
fn processing_complete_folds_thinking_into_pseudo_message() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.phase = ChatPhase::Chatting;
    app.chat.channel = Some("chan".to_string());
    app.chat.topic = Some("t1".to_string());

    for text in ["block one", "block two"] {
        app.chat.handle_live_event(&serde_json::json!({
            "type": "thinking",
            "channel": "chan",
            "topic": "t1",
            "text": text,
        }));
    }
    app.chat.handle_live_event(&serde_json::json!({
        "type": "processing",
        "channel": "chan",
        "topic": "t1",
        "is_processing": false,
        "has_error": false,
    }));

    assert!(app.chat.live_thinking_for("chan", "t1").is_none());
    let thinking_msgs: Vec<_> = app
        .chat
        .messages
        .iter()
        .filter(|m| m.sender == "thinking")
        .collect();
    assert_eq!(thinking_msgs.len(), 1);
    assert_eq!(thinking_msgs[0].text, "block one\n\nblock two");
}

#[test]
fn processing_complete_drops_thinking_when_topic_not_open() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.phase = ChatPhase::Chatting;
    app.chat.channel = Some("chan".to_string());
    app.chat.topic = Some("other".to_string());

    app.chat.handle_live_event(&serde_json::json!({
        "type": "thinking",
        "channel": "chan",
        "topic": "t1",
        "text": "unwatched round",
    }));
    app.chat.handle_live_event(&serde_json::json!({
        "type": "processing",
        "channel": "chan",
        "topic": "t1",
        "is_processing": false,
        "has_error": false,
    }));

    assert!(app.chat.live_thinking_for("chan", "t1").is_none());
    assert!(
        app.chat.messages.iter().all(|m| m.sender != "thinking"),
        "no pseudo-message for a topic that is not open"
    );
}

#[test]
fn leader_toggle_thinking_flips_expanded_flag() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    assert!(!app.chat.thinking_expanded);
    execute_local_action(
        &mut app,
        &mut test_terminal(),
        local_commands::LocalAction::ToggleThinking,
    );
    assert!(app.chat.thinking_expanded);
    execute_local_action(
        &mut app,
        &mut test_terminal(),
        local_commands::LocalAction::ToggleThinking,
    );
    assert!(!app.chat.thinking_expanded);
}

#[test]
fn leader_toggle_tool_detail_flips_expanded_flag() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    assert!(!app.chat.tool_detail_expanded);
    execute_local_action(
        &mut app,
        &mut test_terminal(),
        local_commands::LocalAction::ToggleToolDetail,
    );
    assert!(app.chat.tool_detail_expanded);
    execute_local_action(
        &mut app,
        &mut test_terminal(),
        local_commands::LocalAction::ToggleToolDetail,
    );
    assert!(!app.chat.tool_detail_expanded);
}

#[test]
fn history_fingerprint_changes_on_thinking_expanded_flip() {
    let msgs = vec![history_msg("user", "hi", None)];
    assert_ne!(
        history_fingerprint(&msgs, 80, false, false),
        history_fingerprint(&msgs, 80, true, false)
    );
}

#[test]
fn render_history_thinking_collapsed_shows_summary_not_body() {
    let msgs = vec![
        history_msg("user", "question", Some("2026-08-13T10:00:00Z")),
        history_msg("thinking", "secret chain of thought body", None),
        history_msg("ai", "answer", Some("2026-08-13T10:00:05Z")),
    ];
    let lines = render_history_lines(&msgs, 80, false, false);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(text.contains("💭 thinking — 28 chars"), "got: {text}");
    assert!(!text.contains("secret chain of thought body"));
    // No dashed rule between the human block and the reply any more, and the
    // collapsed thinking message must not swallow the blank row either.
    assert!(!text.contains('┄'), "the rule should be gone: {text}");
    let answer = lines
        .iter()
        .position(|l| l.spans.iter().any(|s| s.content.contains("answer")))
        .expect("answer line");
    assert!(
        lines[answer.saturating_sub(1)]
            .spans
            .iter()
            .all(|s| s.content.trim_end().is_empty()),
        "the reply must not hug the block"
    );
}

#[test]
fn render_history_thinking_expanded_shows_full_text() {
    let msgs = vec![
        history_msg("user", "question", Some("2026-08-13T10:00:00Z")),
        history_msg("thinking", "full chain of thought", None),
        history_msg("ai", "answer", Some("2026-08-13T10:00:05Z")),
    ];
    let lines = render_history_lines(&msgs, 80, true, false);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(text.contains("full chain of thought"), "got: {text}");
}

/// No speaker labels any more: the human side is a full-width background
/// block, the agent's reply stays on the pane background.
#[test]
fn render_history_marks_human_turns_with_background_not_labels() {
    let msgs = vec![
        history_msg("user", "question", Some("2026-08-13T10:00:00Z")),
        history_msg("ai", "answer", Some("2026-08-13T10:00:05Z")),
    ];
    let lines = render_history_lines(&msgs, 80, false, false);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(!text.contains("You:"), "label must be gone: {text}");
    assert!(!text.contains("AI:"), "label must be gone: {text}");

    let is_block = |l: &Line| l.style.bg == Some(USER_BG);
    let block: Vec<&Line> = lines.iter().filter(|l| is_block(l)).collect();
    assert!(
        !block.is_empty() && block.iter().all(|l| l.width() == 80),
        "the block must fill the row instead of stopping at the last glyph"
    );
    // The block breathes: it opens and closes on a painted blank row.
    assert!(block.len() >= 3, "block too small for padding: {block:?}");
    for edge in [block[0], *block.last().unwrap()] {
        assert_eq!(
            edge.spans
                .iter()
                .map(|s| s.content.trim_end().len())
                .sum::<usize>(),
            0,
            "the block's first and last row must be blank padding: {edge:?}"
        );
    }
    // The block contributes only a background — no foreground — so a human turn
    // reads at the terminal's own brightness, same as the agent's reply. Checked
    // on the *line* style, which is where `user_style` lands via `Style::patch`
    // (spans keep markdown's own styles). Holds for this plain-text fixture;
    // markdown puts heading/blockquote colors on the line style, so keep the
    // fixture plain if this ever moves inside a styled block.
    assert!(
        block.iter().all(|l| l.style.fg.is_none()),
        "the block must not set a foreground"
    );
    // Neither side hugs the pane edge: both bodies start one column in.
    let question = lines
        .iter()
        .position(|l| is_block(l) && l.spans.iter().any(|s| s.content == "question"))
        .expect("the human turn's text row");
    let answer = lines
        .iter()
        .position(|l| !is_block(l) && l.spans.iter().any(|s| s.content == "answer"))
        .expect("the reply row, unpainted");
    assert_eq!(lines[question].spans[0].content, " ");
    assert_eq!(lines[answer].spans[0].content, " ");
    // A reply is followed by a blank row, so it never ends flush against what
    // comes next.
    let blank = |l: &Line| l.spans.iter().all(|s| s.content.trim_end().is_empty());
    assert!(
        lines.get(answer + 1).is_some_and(blank),
        "a reply must be followed by a blank row"
    );
}

/// Every row fits the pane: the round rules are exactly `width` (they used to be
/// `width + 1`, kept in check only by clipping), and the one-column inset leaves
/// no body row overflowing by a cell.
#[test]
fn render_history_rows_fit_the_pane() {
    let msgs = vec![
        history_msg("user", "question", Some("2026-08-13T10:00:00Z")),
        history_msg("ai", "answer", Some("2026-08-13T10:00:05Z")),
    ];
    let lines = render_history_lines(&msgs, 80, false, false);
    // `dim_style` on the line is what marks a round rule.
    let rules: Vec<&Line> = lines
        .iter()
        .filter(|l| l.style.fg == Some(Color::DarkGray))
        .collect();
    assert_eq!(rules.len(), 2, "expected the top and bottom rule");
    let widths = || rules.iter().map(|l| l.width()).collect::<Vec<_>>();
    assert!(
        widths().iter().all(|w| *w == 80),
        "a rule must be exactly pane-wide, got {:?}",
        widths()
    );
    assert!(
        lines.iter().all(|l| l.width() <= 80),
        "no row may overflow the pane: {:?}",
        lines.iter().map(|l| l.width()).collect::<Vec<_>>()
    );
}

/// A remote (piped-channel) sender is the human side too, so it gets the
/// block — the label used to be the only thing distinguishing it.
#[test]
fn render_history_blocks_piped_channel_sender() {
    let msgs = vec![history_msg(
        "金晔",
        "from feishu",
        Some("2026-08-13T10:00:00Z"),
    )];
    let lines = render_history_lines(&msgs, 40, false, false);
    assert!(
        lines
            .iter()
            .any(|l| l.style.bg == Some(USER_BG)
                && l.spans.iter().any(|s| s.content == "from feishu")),
        "human-side sender must carry the background"
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

// ── Message-pane cursor ───────────────────────────────────────────────────

/// A chat whose message pane is focused and drawn once, so the renderer has
/// measured the geometry the cursor moves against (the keys always work on last
/// frame's numbers).
fn cursor_app() -> App {
    let mut app = chatting_app();
    app.chat.messages = (0..40)
        .map(|i| history_msg("user", &format!("message {i:02}"), None))
        .collect();
    app.chat.focus = ChatFocus::MessageArea;
    let _ = draw_80x24(&mut app);
    assert!(
        app.chat.last_total_lines > 30,
        "the fixture needs rows below the fold, got {}",
        app.chat.last_total_lines
    );
    app
}

/// Send one key to the chat screen the way the event loop does.
fn press_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    handle_chat_keys(
        app,
        crossterm::event::KeyEvent::new(code, modifiers),
        &mut test_terminal(),
    );
}

fn press(app: &mut App, c: char) {
    press_key(app, KeyCode::Char(c), KeyModifiers::NONE);
}

/// The rendered transcript rows as plain text — what a yank copies.
fn transcript_rows(app: &App) -> Vec<String> {
    app.chat
        .render_cache
        .as_ref()
        .expect("drawn once")
        .1
        .iter()
        .map(line_text)
        .collect()
}

/// The status line the app is showing, if any.
fn status(app: &App) -> String {
    app.status_message
        .as_ref()
        .map(|(m, _)| m.clone())
        .unwrap_or_default()
}

/// A fresh cursor lands at the end of the newest message — where the reader
/// already is. Not on the blank spacer that closes a reply: the bar there paints
/// an empty line, and `yy` would copy nothing.
#[test]
fn message_cursor_starts_on_the_newest_message_row() {
    let app = cursor_app();
    let rows = transcript_rows(&app);
    assert_eq!(
        Some(app.chat.cursor_line),
        rows.iter().rposition(|r| !r.trim().is_empty()),
        "the cursor starts on the last row that carries text"
    );
    assert_ne!(
        app.chat.cursor_line,
        app.chat.last_total_lines - 1,
        "the end of the transcript is spacer/pending rows, not the newest message"
    );
}

/// A topic's messages reach the pane a frame or two after the switch (the REST
/// hydrate), so the unplaced cursor has to wait: resolving it against the empty
/// transcript would park it on row 0 for the rest of the session, which is what
/// the reader sees as "the cursor starts at the top".
#[test]
fn an_unplaced_cursor_waits_for_the_transcript_to_arrive() {
    let mut app = cursor_app();
    let messages = std::mem::take(&mut app.chat.messages);
    app.chat.render_cache = None;
    app.chat.reset_cursor();
    draw_80x24(&mut app);
    assert_eq!(
        app.chat.cursor_line,
        usize::MAX,
        "the sentinel must survive the frame without messages"
    );

    app.chat.messages = messages;
    app.chat.render_cache = None;
    draw_80x24(&mut app);
    let rows = transcript_rows(&app);
    assert_eq!(
        Some(app.chat.cursor_line),
        rows.iter().rposition(|r| !r.trim().is_empty()),
        "the first frame with messages places the cursor at their end"
    );
}

/// `j`/`k` move the cursor; the view starts scrolling only once the cursor runs
/// into the top or bottom row, so a long jump stops at the edge instead of
/// dragging the whole screen along.
#[test]
fn the_view_follows_the_cursor_only_at_its_edges() {
    let mut app = cursor_app();
    let start = app.chat.view_start_line();
    let rows_to_top = app.chat.cursor_line - start;
    assert!(
        rows_to_top > 1,
        "the fixture needs room to walk inside the view"
    );

    for _ in 0..rows_to_top {
        press(&mut app, 'k');
    }
    assert_eq!(app.chat.cursor_line, start);
    assert_eq!(app.chat.scroll, 0, "walking inside the view never scrolls");

    press(&mut app, 'k');
    assert_eq!(
        app.chat.scroll, 1,
        "one row off the top is one row of scroll"
    );
    assert_eq!(app.chat.view_start_line(), start - 1);
    assert_eq!(
        app.chat.cursor_line,
        start - 1,
        "and it sits on the new top row"
    );

    // Coming back inside the view does not undo the scroll — the cursor has
    // room to travel before it reaches the bottom edge again.
    press(&mut app, 'j');
    assert_eq!(app.chat.scroll, 1);
    assert_eq!(app.chat.cursor_line, start);
}

/// Digits before a movement key count lines. A jump past either end stops there.
#[test]
fn digits_before_a_movement_key_count_lines() {
    let mut app = cursor_app();
    let total = app.chat.last_total_lines;
    let start = app.chat.cursor_line;

    for c in ['5', 'k'] {
        press(&mut app, c);
    }
    assert_eq!(app.chat.cursor_line, start - 5, "`5k` steps five rows up");
    assert_eq!(app.chat.pending_count, 0, "the prefix is spent");

    for c in ['1', '2', 'j'] {
        press(&mut app, c);
    }
    assert_eq!(
        app.chat.cursor_line,
        total - 1,
        "twelve rows down clamps at the end"
    );
}

/// A view-only move (wheel, `PgDn`, `Ctrl+F`) carries the cursor the same number
/// of rows, so it keeps the screen row it was on while the text slides
/// underneath — through the partial page at the end of the transcript, where it
/// stops along with the view.
#[test]
fn a_view_move_carries_the_cursor_along_its_screen_row() {
    let mut app = cursor_app();
    // Top of the transcript, cursor on the third visible row.
    app.chat.scroll = app.chat.last_max_scroll;
    app.chat.cursor_line = app.chat.view_start_line() + 2;
    let row = app.chat.cursor_line - app.chat.view_start_line();

    for _ in 0..5 {
        app.chat.scroll_down();
        assert_eq!(
            app.chat.cursor_line - app.chat.view_start_line(),
            row,
            "the wheel moves the cursor with the text"
        );
    }
    // The bottom of the transcript, row by row, with the same promise.
    for _ in 0..64 {
        if app.chat.scroll == 0 {
            break;
        }
        app.chat.page_down();
        assert_eq!(
            app.chat.cursor_line - app.chat.view_start_line(),
            row,
            "a page does too, including the last one when it only fits partway"
        );
    }
    assert_eq!(app.chat.scroll, 0, "the walk reached the bottom");
    let parked = app.chat.cursor_line;
    app.chat.page_down();
    app.chat.scroll_down();
    assert_eq!(
        app.chat.cursor_line, parked,
        "with nowhere to go the cursor holds still"
    );

    // Same at the top.
    for _ in 0..64 {
        if app.chat.scroll == app.chat.last_max_scroll {
            break;
        }
        app.chat.page_up();
        assert_eq!(app.chat.cursor_line - app.chat.view_start_line(), row);
    }
    assert_eq!(
        app.chat.scroll, app.chat.last_max_scroll,
        "the walk reached the top"
    );
    let parked = app.chat.cursor_line;
    app.chat.page_up();
    assert_eq!(app.chat.cursor_line, parked);
}

/// `gg` and `G` are jumps, not steps: cursor and view go to the first / last row.
#[test]
fn gg_and_capital_g_take_the_cursor_to_the_ends() {
    let mut app = cursor_app();
    let total = app.chat.last_total_lines;

    press(&mut app, 'G');
    assert_eq!(app.chat.cursor_line, total - 1);
    assert_eq!(app.chat.scroll, 0);

    press(&mut app, 'g');
    press(&mut app, 'g');
    assert_eq!(app.chat.cursor_line, 0);
    assert_eq!(
        app.chat.scroll, app.chat.last_max_scroll,
        "the view follows the jump"
    );
}

/// `y` arms, the second `y` copies the row under the cursor and says how much it
/// took. The copy is the *rendered* row, so it matches what was on screen.
#[test]
fn yy_copies_the_row_under_the_cursor() {
    let mut app = cursor_app();
    let rows = transcript_rows(&app);
    let target = rows
        .iter()
        .position(|r| r.contains("message 07"))
        .expect("the fixture's rows");
    app.chat.cursor_line = target;

    press(&mut app, 'y');
    assert!(app.chat.pending_y, "one `y` only arms the pair");
    assert!(app.chat.pending_clipboard.is_none());

    press(&mut app, 'y');
    assert_eq!(
        app.chat
            .pending_clipboard
            .take()
            .expect("a queued clipboard write"),
        rows[target]
    );
    assert_eq!(status(&app), "Copied 1 line");
    assert!(!app.chat.pending_y, "the pair is closed");
}

/// Leaving the message pane drops a half-typed command, so a stray `y` cannot
/// complete itself minutes later.
#[test]
fn refocusing_the_input_drops_a_half_typed_yank() {
    let mut app = cursor_app();
    for c in ['y', '3'] {
        press(&mut app, c);
    }
    press(&mut app, 'x'); // any other key goes back to the input
    assert_eq!(app.chat.focus, ChatFocus::ChatPane);
    assert!(!app.chat.pending_y);
    assert_eq!(app.chat.pending_count, 0);

    app.chat.focus = ChatFocus::MessageArea;
    press(&mut app, 'y');
    assert!(app.chat.pending_y, "it arms again rather than copying");
    assert!(app.chat.pending_clipboard.is_none());
}

/// The count may sit between the two halves (`y3y`) or in front of them (`3yy`).
#[test]
fn a_count_in_a_yank_copies_that_many_rows() {
    for keys in [['y', '3', 'y'], ['3', 'y', 'y']] {
        let mut app = cursor_app();
        let rows = transcript_rows(&app);
        let target = rows
            .iter()
            .position(|r| r.contains("message 07"))
            .expect("the fixture's rows");
        app.chat.cursor_line = target;
        for c in keys {
            press(&mut app, c);
        }
        let copied = app
            .chat
            .pending_clipboard
            .take()
            .unwrap_or_else(|| panic!("{keys:?} queued nothing"));
        assert_eq!(copied.lines().count(), 3, "{keys:?} copies three rows");
        assert_eq!(copied.lines().next().unwrap(), rows[target]);
        assert_eq!(status(&app), "Copied 3 lines");
    }
}

/// The bar is the message pane's own: it spans the row edge to edge, it leaves the
/// text's colour alone while nothing is selected, and with the input focused the
/// transcript is plain text again.
#[test]
fn the_cursor_bar_spans_the_row_only_while_the_pane_has_focus() {
    use ratatui::style::Color;

    use super::render::SELECT_BG;

    let mut app = cursor_app();
    let area = app.chat.last_message_area.unwrap();
    let y = area.top() + (app.chat.cursor_line - app.chat.view_start_line()) as u16;

    let buffer = draw_80x24(&mut app);
    assert_eq!(buffer[(area.left(), y)].style().bg, Some(SELECT_BG));
    assert_eq!(
        buffer[(area.right() - 1, y)].style().bg,
        Some(SELECT_BG),
        "the bar reaches the edge even on a short row"
    );
    assert_ne!(
        buffer[(area.left(), y)].style().fg,
        Some(Color::White),
        "the cursor row alone keeps the transcript's own foreground — it is lit \
         while you are only reading"
    );

    app.chat.focus = ChatFocus::ChatPane;
    let buffer = draw_80x24(&mut app);
    assert_ne!(buffer[(area.left(), y)].style().bg, Some(SELECT_BG));
}

// ── Selecting rows with Shift ─────────────────────────────────────────────

/// Press a Shifted character the way the terminal reports it.
fn press_shift(app: &mut App, c: char) {
    press_key(app, KeyCode::Char(c), KeyModifiers::SHIFT);
}

/// Walk the cursor up `n` rows with the digit-count form (`n` < pane height).
fn move_up(app: &mut App, n: usize) {
    for digit in n.to_string().chars() {
        press(app, digit);
    }
    press(app, 'k');
}

/// Shift+J opens a selection at the cursor, and `y` copies exactly the rows
/// between the two ends and leaves visual mode.
#[test]
fn shift_movement_opens_a_selection_that_y_copies() {
    let mut app = cursor_app();
    let rows = transcript_rows(&app);
    let start = app.chat.cursor_line;
    move_up(&mut app, 5);

    press_shift(&mut app, 'J');
    press_shift(&mut app, 'J');
    assert_eq!(
        app.chat.selection_range(),
        Some((start - 5, start - 3)),
        "the anchor stayed put while the cursor moved"
    );
    assert_eq!(status(&app), "3 lines selected");

    press(&mut app, 'y');
    let copied = app
        .chat
        .pending_clipboard
        .take()
        .expect("a queued clipboard write");
    assert_eq!(
        copied,
        format!(
            "{}\n{}\n{}",
            rows[start - 5],
            rows[start - 4],
            rows[start - 3]
        ),
        "both ends are included"
    );
    assert_eq!(status(&app), "Copied 3 lines");
    assert_eq!(
        app.chat.selection_range(),
        None,
        "`y` is the end of the selection"
    );
    assert_eq!(
        app.chat.cursor_line,
        start - 5,
        "and the cursor lands back on the first copied row"
    );
}

/// Selecting upwards leaves the anchor at the *bottom* of the range, so "the
/// first row" cannot mean the anchor: `y` still ends on the topmost copied row.
#[test]
fn y_lands_on_the_topmost_copied_row_whichever_way_the_selection_was_made() {
    let mut app = cursor_app();
    let rows = transcript_rows(&app);
    let start = app.chat.cursor_line;
    move_up(&mut app, 3);
    press_shift(&mut app, 'K');
    assert_eq!(
        app.chat.selection_range(),
        Some((start - 4, start - 3)),
        "the anchor is the lower end of an upward selection"
    );

    press(&mut app, 'y');
    let copied = app
        .chat
        .pending_clipboard
        .take()
        .expect("a queued clipboard write");
    assert_eq!(
        copied,
        format!("{}\n{}", rows[start - 4], rows[start - 3]),
        "both copied rows, top first"
    );
    assert_eq!(app.chat.cursor_line, start - 4, "topmost, not the anchor");
}

/// `y` also brings the view with it: the row it lands on is the point of the
/// motion, so a range that was paged out of sight is revealed rather than copied
/// invisibly (`scrolling_leaves_an_open_selection_alone` is what lets the view get
/// away from the selection in the first place).
#[test]
fn y_reveals_the_row_it_lands_on() {
    let mut app = cursor_app();
    let height = app.chat.last_message_area.unwrap().height as usize;
    move_up(&mut app, 3);
    press_shift(&mut app, 'K');
    // `page_up` walks the view away toward the start; the selection does not
    // follow it, so the rows fall out of sight below the pane. (`page_down` would
    // be clamped at the end of the transcript, which is where the cursor sits.)
    for _ in 0..40 {
        app.chat.page_up();
        if app.chat.cursor_line >= app.chat.view_start_line() + height {
            break;
        }
    }
    assert!(
        app.chat.cursor_line >= app.chat.view_start_line() + height,
        "the setup must get the selection out of the view"
    );

    press(&mut app, 'y');
    let view = app.chat.view_start_line()..app.chat.view_start_line() + height;
    assert!(
        view.contains(&app.chat.cursor_line),
        "the yanked row is on screen again"
    );
}

/// Once a selection is open, every movement grows it — Shift or not, with a
/// count or without. Walking back across the anchor shrinks it and then flips
/// which end is which, so the range is always the rows in between.
#[test]
fn movement_keeps_extending_an_open_selection() {
    let mut app = cursor_app();
    let start = app.chat.cursor_line;
    move_up(&mut app, 5);
    press_shift(&mut app, 'J');

    press(&mut app, 'j');
    assert_eq!(app.chat.selection_range(), Some((start - 5, start - 3)));

    press(&mut app, '3');
    press(&mut app, 'j');
    assert_eq!(app.chat.selection_range(), Some((start - 5, start)));

    for _ in 0..4 {
        press(&mut app, 'k');
    }
    assert_eq!(
        app.chat.selection_range(),
        Some((start - 5, start - 4)),
        "shrinking back towards the anchor"
    );
    for _ in 0..2 {
        press(&mut app, 'k');
    }
    assert_eq!(
        app.chat.selection_range(),
        Some((start - 6, start - 5)),
        "crossing the anchor flips the ends"
    );
}

/// The jumps work the same way: with a selection open, `gg` takes the far end to
/// the top of the transcript and `G` pulls it back to the bottom.
#[test]
fn the_jumps_extend_an_open_selection() {
    let mut app = cursor_app();
    let start = app.chat.cursor_line;
    press_shift(&mut app, 'K');
    press_shift(&mut app, 'K');
    assert_eq!(app.chat.selection_range(), Some((start - 2, start)));

    press(&mut app, 'g');
    press(&mut app, 'g');
    assert_eq!(
        app.chat.selection_range(),
        Some((0, start)),
        "`gg` selected everything above"
    );

    press(&mut app, 'G');
    assert_eq!(
        app.chat.selection_range(),
        Some((start, app.chat.last_total_lines - 1)),
        "`G` extends the selection down to the last row of the transcript"
    );
}

/// `Esc` drops the selection and leaves the user in the message pane; the second
/// one is what returns to the input. A mis-click on `Esc` must not throw away a
/// selection *and* the focused pane at once.
#[test]
fn esc_leaves_the_selection_before_it_leaves_the_pane() {
    let mut app = cursor_app();
    move_up(&mut app, 3);
    press_shift(&mut app, 'K');
    assert!(app.chat.selection_range().is_some());
    let at = app.chat.cursor_line;

    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.chat.selection_range(), None);
    assert_eq!(
        app.chat.cursor_line, at,
        "`Esc` drops the selection where it is — unlike `y`, it is not a copy, so \
         the cursor keeps the row it moved to"
    );
    assert_eq!(app.chat.focus, ChatFocus::MessageArea);

    press_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.chat.focus, ChatFocus::ChatPane);
}

/// Scrolling to look around while a selection is open changes nothing about it:
/// the cursor is pinned to its text, unlike the view-riding behaviour it has
/// when nothing is selected (`a_view_move_carries_the_cursor_along_its_screen_row`).
#[test]
fn scrolling_leaves_an_open_selection_alone() {
    let mut app = cursor_app();
    let start = app.chat.cursor_line;
    move_up(&mut app, 5);
    press_shift(&mut app, 'J');
    let selected = app.chat.selection_range();
    assert_eq!(selected, Some((start - 5, start - 4)));
    let before = app.chat.view_start_line();

    for _ in 0..3 {
        app.chat.page_up();
        app.chat.scroll_up();
    }
    assert!(
        app.chat.view_start_line() < before,
        "the test needs the view to have actually moved"
    );
    assert_eq!(app.chat.selection_range(), selected, "same rows selected");
}

/// A yank armed *before* a selection must not outlive it: `y` + `J` + `y` copies
/// the selection and spends the arm, so the next `y` starts a fresh pair instead
/// of firing the stale one and silently yanking a second time.
#[test]
fn an_armed_yank_does_not_survive_a_selection() {
    let mut app = cursor_app();
    move_up(&mut app, 3);
    let rows = transcript_rows(&app);
    press(&mut app, 'y');
    press_shift(&mut app, 'J');
    let (from, to) = app.chat.selection_range().expect("open");
    assert_eq!(to - from, 1, "two rows");
    press(&mut app, 'y');

    let copied = app
        .chat
        .pending_clipboard
        .take()
        .expect("a queued clipboard write");
    assert_eq!(
        copied,
        rows[from..=to].join("\n"),
        "both ends, exactly as rendered"
    );
    assert_eq!(status(&app), "Copied 2 lines");
    assert!(!app.chat.pending_y, "the arm is spent with the selection");

    press(&mut app, 'y');
    assert!(
        app.chat.pending_clipboard.is_none(),
        "the next `y` arms, it does not copy"
    );
}

/// At the end of the transcript (`G` — a fresh cursor stops one row short, on the
/// newest message) there is nowhere to extend, so the selection is the single row
/// under the cursor — and the status line counts it as one line.
#[test]
fn a_one_row_selection_reports_a_single_line() {
    let mut app = cursor_app();
    press(&mut app, 'G');
    let last = app.chat.cursor_line;
    assert_eq!(last, app.chat.last_total_lines - 1, "`G` is the end");

    press_shift(&mut app, 'J');
    assert_eq!(app.chat.selection_range(), Some((last, last)));
    assert_eq!(status(&app), "1 line selected");

    press(&mut app, 'y');
    assert_eq!(status(&app), "Copied 1 line");
}

/// A selection is the message pane's alone. Focus can leave it by routes that do
/// not clear state themselves (`Tab`, a click, opening the explorer), so the
/// render drops the selection: an invisible one must not be yankable, and must
/// not keep pinning the cursor against the view.
#[test]
fn leaving_the_pane_drops_the_selection() {
    let mut app = cursor_app();
    move_up(&mut app, 3);
    press_shift(&mut app, 'J');
    assert!(app.chat.selection_range().is_some());

    app.chat.focus = ChatFocus::InfoPane;
    let _ = draw_80x24(&mut app);
    assert_eq!(app.chat.selection_range(), None, "dropped with the focus");

    app.chat.focus = ChatFocus::MessageArea;
    let _ = draw_80x24(&mut app);
    press(&mut app, 'y');
    assert!(
        app.chat.pending_clipboard.is_none(),
        "`y` arms a fresh pair instead of copying the old range"
    );
}

/// One background for the whole highlight — the cursor row and every selected row
/// look alike — and only the selected rows give up their own foreground.
#[test]
fn the_highlight_bar_covers_the_cursor_and_the_selection() {
    use ratatui::style::Color;

    use super::render::SELECT_BG;

    let mut app = cursor_app();
    move_up(&mut app, 3);
    press_shift(&mut app, 'J');
    press_shift(&mut app, 'J');
    let area = app.chat.last_message_area.unwrap();
    let skip = app.chat.view_start_line();
    let row_of = |line: usize| area.top() + (line - skip) as u16;
    let cursor = app.chat.cursor_line;
    let buffer = draw_80x24(&mut app);
    let cell = |line: usize| {
        let s = buffer[(area.left(), row_of(line))].style();
        (s.bg, s.fg)
    };
    let bg_at = |line: usize| cell(line).0;

    assert_eq!(bg_at(cursor), Some(SELECT_BG), "the cursor row");
    assert_eq!(bg_at(cursor - 1), Some(SELECT_BG), "a selected row");
    assert_eq!(bg_at(cursor - 2), Some(SELECT_BG), "the anchor row");
    assert_eq!(
        cell(cursor).1,
        Some(Color::White),
        "inside a selection even the cursor row takes the light foreground, so the \
         range has one look"
    );
    assert_eq!(
        cell(cursor - 1).1,
        Some(Color::White),
        "a selected row forces a light foreground so the navy works on any theme"
    );
    assert_ne!(
        bg_at(cursor - 3),
        Some(SELECT_BG),
        "rows outside are untouched"
    );
}

/// The other half of the thinking toggle: minimal progress mode leaves no
/// gap behind.
#[test]
fn render_history_minimal_progress_drops_the_thinking_line() {
    let msgs = vec![
        history_msg("user", "go", None),
        history_msg("thinking", "one", None),
        history_msg("thinking", "two", None),
        history_msg("ai", "**reply**", None),
    ];

    let full = render_history_lines(&msgs, 80, false, false);
    let minimal = render_history_lines(&msgs, 80, false, true);
    let full_rows = line_texts(&full);
    let minimal_rows = line_texts(&minimal);
    let thinking_rows =
        |rows: &[String]| rows.iter().filter(|l| l.starts_with("💭 thinking")).count();

    // Collapsed thinking renders its summary row and never the body, so the
    // marker is the thing that has to be there to be dropped — and dropping the
    // block takes its blank gap row along with it.
    assert_eq!(
        thinking_rows(&full_rows),
        2,
        "one summary per thinking block is there to be dropped:\n{}",
        full_rows.join("\n")
    );
    assert_eq!(
        thinking_rows(&minimal_rows),
        0,
        "minimal mode renders neither thinking block:\n{}",
        minimal_rows.join("\n")
    );

    // ...and the two rules around it still hold: the reply survives, and the
    // user row keeps its block — which sits on the *line* style, the text span
    // carrying only the pad's own copy of it.
    let flat = minimal_rows.join("\n");
    assert!(
        flat.contains("go") && flat.contains("reply"),
        "the round survives without its thinking line:\n{flat}"
    );
    let user_row = minimal
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content.trim() == "go"));
    assert!(
        user_row.is_some_and(|l| l.style.bg == Some(USER_BG)),
        "the user row is still a background block:\n{flat}"
    );
}

/// Wiring test for the mode itself: `p` has to be a chat-scoped command whose
/// dispatch moves the flag the render reads.
#[test]
fn minimal_progress_command_flips_the_flag_the_render_reads() {
    use crate::cli::dashboard::local_commands::{CommandScope, LocalAction, local_commands};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let cmd = local_commands()
        .iter()
        .find(|c| c.action == LocalAction::ToggleMinimalProgress)
        .expect("the leader popup offers the minimal progress toggle");
    assert_eq!(cmd.scope, CommandScope::Chat, "`p` is a chat command");
    assert_eq!(cmd.leader_keys, "p", "next to expand_tool_detail's `t`");

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    let mut terminal = Terminal::new(TestBackend::new(40, 20)).expect("test terminal");
    for expected in [true, false] {
        super::execute_local_action(&mut app, &mut terminal, cmd.action);
        assert_eq!(app.chat.minimal_progress, expected);
    }
}

#[cfg(test)]
mod part2;
