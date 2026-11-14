use super::render::{USER_BG, render_history_lines};
use super::*;

fn history_msg(sender: &str, text: &str, ts: Option<&str>) -> ChatMessage {
    ChatMessage {
        id: 0,
        client_id: None,
        sender: sender.to_string(),
        text: text.to_string(),
        timestamp: ts.map(|s| s.to_string()),
    }
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
fn topic_switch_clears_chat_messages() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);

    // Simulate messages from a previous topic
    app.chat.messages.push(ChatMessage {
        id: 0,
        client_id: None,
        sender: "user".to_string(),
        text: "hello from topic A".to_string(),
        timestamp: None,
    });
    app.chat.messages.push(ChatMessage {
        id: 0,
        client_id: None,
        sender: "ai".to_string(),
        text: "reply from topic A".to_string(),
        timestamp: None,
    });
    assert_eq!(app.chat.messages.len(), 2);

    // Switch to a new topic
    app.chat.reset_chat_state("topic-b");

    // Messages must be cleared so stale content doesn't leak across topics
    assert!(app.chat.messages.is_empty());
    assert_eq!(app.chat.topic.as_deref(), Some("topic-b"));
}

#[test]
fn scroll_to_top_and_bottom_follow_activity_focus() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.focus = Focus::ActivityPane;
    app.chat.activity_scroll = 3;
    app.chat.scroll_to_bottom();
    assert_eq!(app.chat.activity_scroll, 0);
    // `usize::MAX` is the "as far up as possible" sentinel; the renderer
    // clamps it to the pane's own row count at draw time.
    app.chat.scroll_to_top();
    assert_eq!(app.chat.activity_scroll, usize::MAX);
    // Other panes: no scroll state moves.
    app.chat.focus = Focus::InfoPane;
    app.chat.activity_scroll = 3;
    app.chat.scroll_to_top();
    app.chat.scroll_to_bottom();
    assert_eq!(app.chat.activity_scroll, 3);
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
    // `g` after reset arms a fresh sequence instead of jumping
    assert!(!app.chat.gg_step(true));
    assert!(app.chat.pending_g);
    // ...and the next `g` completes that fresh sequence
    assert!(app.chat.gg_step(true));
    assert!(!app.chat.pending_g);
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
fn topic_switch_clears_input_history() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);

    app.chat.input_history = vec!["msg from topic A".to_string()];
    app.chat.history_pos = Some(0);

    // Switch to a new topic
    app.chat.reset_chat_state("topic-b");

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

/// A `/topics <name>` delivery marker never reaches the transcript: the
/// live-event handler parks it on `pending_topics_switch` for the run loop
/// to re-open the chat screen on the named topic.
#[test]
fn topics_marker_is_intercepted_not_rendered() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut chat = super::ChatState::new(rx);
    chat.handle_live_event(&serde_json::json!({
        "type": "chat_message",
        "channel": "email",
        "topic": "billing",
        "entry": {
            "sender": "user",
            "text": "\u{1}topics-switch\u{1}email\u{1}invoice",
            "id": 7,
        }
    }));
    assert_eq!(
        chat.pending_topics_switch
            .as_ref()
            .map(|(c, t)| (c.as_str(), t.as_str())),
        Some(("email", "invoice")),
        "the run loop re-opens the chat on the marker's topic"
    );
    assert!(
        chat.messages.is_empty(),
        "the marker must not appear in the transcript"
    );
}

/// `/info` renders locally from the polled detail: one user echo, one info
/// reply, and no message queued for the server.
#[test]
fn info_command_renders_locally_without_server_round_trip() {
    let mut app = app_with_topic_summary();
    let before = app.chat.messages.len();
    let mut editor = super::chat_editor("/info");
    editor.input(crossterm::event::KeyEvent::new(
        KeyCode::End,
        KeyModifiers::NONE,
    ));
    app.chat.editor = editor;
    super::handle_chat_keys(
        &mut app,
        crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut test_terminal(),
    );
    assert_eq!(app.chat.messages.len(), before + 2, "echo + info reply");
    assert_eq!(app.chat.messages[before].sender, "user");
    assert_eq!(app.chat.messages[before].text, "/info");
    assert!(
        app.chat.messages[before + 1].text.contains("Topic:"),
        "the reply carries the pane's label rows: {:?}",
        app.chat.messages[before + 1].text
    );
    assert!(
        app.chat.ws_tx.is_none(),
        "no server round-trip for a local command"
    );
}

#[test]
fn esc_does_not_close_chat_with_input_focused() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.topic = Some("jyc".to_string());
    app.chat.focus = Focus::ChatPane;

    handle_chat_keys(&mut app, esc_key(), &mut test_terminal());
    assert!(app.chat.visible, "Esc must not close the chat screen");
}

#[test]
fn esc_does_not_close_chat_in_activity_pane() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.topic = Some("jyc".to_string());
    app.chat.focus = Focus::ActivityPane;

    handle_chat_keys(&mut app, esc_key(), &mut test_terminal());
    assert!(app.chat.visible, "Esc must not close the chat screen");
    assert_eq!(app.chat.focus, Focus::ActivityPane);
}

fn chatting_app() -> App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.topic = Some("jyc".to_string());
    // A fresh `App` starts on the overview screen, so its focus is the topics
    // table — a chat screen is never drawn in that state (opening one moves the
    // focus to the input). Keys that mean something only on the chat screen
    // (the `/` popup, typing) are gated on the input having the focus, so put
    // the app where the screen would before handing it keys.
    app.chat.focus = Focus::ChatPane;
    app
}

/// `ctrl+p /` is gone. The popup has no field of its own — it filters
/// off the chat input — so a leader-driven one could only show commands
/// filtered by whatever draft sat in the field. The slash in the field is
/// the only way into the popup now.
#[test]
fn leader_slash_does_not_open_the_command_popup() {
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
    assert!(
        app.chat.command_popup.is_none(),
        "`/` is no leader entry, so nothing opens"
    );
    assert_eq!(app.chat.text(), "", "the field stays untouched");
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

/// `ctrl+p x` empties the input field.
#[test]
fn leader_clear_input_empties_the_field() {
    let mut app = chatting_app();
    app.chat.populate_editor("draft");
    execute_local_action(
        &mut app,
        &mut test_terminal(),
        local_commands::LocalAction::ClearInput,
    );
    assert_eq!(app.chat.text(), "");
}

/// An open `/` popup has no field of its own — it filters off the input
/// field, so clearing the field closes the popup with it (an empty field
/// has nothing to filter, same as deleting the `/` by hand).
#[test]
fn leader_clear_input_closes_an_open_popup() {
    let mut app = chatting_app();
    let key = |code: KeyCode| crossterm::event::KeyEvent::new(code, KeyModifiers::NONE);
    handle_chat_keys(&mut app, key(KeyCode::Char('/')), &mut test_terminal());
    for c in "dr".chars() {
        handle_chat_keys(&mut app, key(KeyCode::Char(c)), &mut test_terminal());
    }
    assert_eq!(
        app.chat.command_popup.as_ref().expect("popup open").filter,
        "/dr",
        "typing filters the open popup (the filter is the field verbatim)"
    );
    execute_local_action(
        &mut app,
        &mut test_terminal(),
        local_commands::LocalAction::ClearInput,
    );
    assert_eq!(app.chat.text(), "");
    assert!(
        app.chat.command_popup.is_none(),
        "an empty field leaves nothing for the popup to filter"
    );
}

/// `ctrl+p space` switches the topic's mode by sending the `/plan` / `/build`
/// command the server applies — the TUI never sets a mode itself.
#[test]
fn leader_toggle_mode_sends_build_when_the_topic_is_in_plan() {
    let mut app = app_with_topic_summary(); // mode: Some("plan")
    execute_local_action(
        &mut app,
        &mut test_terminal(),
        local_commands::LocalAction::ToggleMode,
    );
    let sent = app.chat.messages.last().expect("command echoed locally");
    assert_eq!(sent.text, "/build", "plan mode must ask for build");
}

#[test]
fn leader_toggle_mode_sends_plan_when_the_topic_is_in_build() {
    let mut app = app_with_topic_summary();
    app.topic_detail.as_mut().expect("detail").mode = Some("build".to_string());
    execute_local_action(
        &mut app,
        &mut test_terminal(),
        local_commands::LocalAction::ToggleMode,
    );
    let sent = app.chat.messages.last().expect("command echoed locally");
    assert_eq!(sent.text, "/plan", "build mode must ask for plan");
}

/// Before the server reports a mode — no overview polled yet, or the field
/// absent — the topic counts as build (the server's default), so the first
/// press has to ask for plan.
#[test]
fn leader_toggle_mode_defaults_to_plan_when_the_mode_is_unknown() {
    let mut app = chatting_app();
    execute_local_action(
        &mut app,
        &mut test_terminal(),
        local_commands::LocalAction::ToggleMode,
    );
    assert_eq!(app.chat.messages.last().expect("echo").text, "/plan");

    let mut app = app_with_topic_summary();
    app.topic_detail.as_mut().expect("detail").mode = None;
    execute_local_action(
        &mut app,
        &mut test_terminal(),
        local_commands::LocalAction::ToggleMode,
    );
    assert_eq!(app.chat.messages.last().expect("echo").text, "/plan");
}

/// The key path: `ctrl+p` then the space bar — the space must reach the
/// leader (not the input field) and dispatch.
#[test]
fn leader_space_toggles_mode_through_the_key_handler() {
    let mut app = app_with_topic_summary();
    let key = |code: KeyCode, mods| crossterm::event::KeyEvent::new(code, mods);
    handle_chat_keys(
        &mut app,
        key(KeyCode::Char('p'), KeyModifiers::CONTROL),
        &mut test_terminal(),
    );
    assert!(app.chat.leader.is_some());
    handle_chat_keys(
        &mut app,
        key(KeyCode::Char(' '), KeyModifiers::NONE),
        &mut test_terminal(),
    );
    assert!(app.chat.leader.is_none(), "space dispatches");
    assert_eq!(app.chat.messages.last().expect("echo").text, "/build");
    assert_eq!(app.chat.text(), "", "the shortcut must not touch the field");
}

/// Draw `app` into a `width` x `height` test backend and hand back the cells.
fn draw_sized(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| ui_chat_mode(frame, frame.area(), app))
        .expect("draw");
    terminal.backend().buffer().clone()
}

/// Draw `app` into the shared 80x24 test backend and hand back the cells.
fn draw_80x24(app: &mut App) -> ratatui::buffer::Buffer {
    draw_sized(app, 80, 24)
}

/// The whole screen as one string, rows joined by newlines.
fn pane_text(buffer: &ratatui::buffer::Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| row_text(buffer, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// How many screen rows carry `needle` — one per wrapped row, so it counts the
/// rows a wrapped block actually took.
fn rows_showing(buffer: &ratatui::buffer::Buffer, needle: &str) -> usize {
    (0..buffer.area.height)
        .filter(|y| row_text(buffer, *y).contains(needle))
        .count()
}

/// One screen row as a string, one char per cell.
fn row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol().to_string())
        .collect::<Vec<_>>()
        .join("")
}

/// Whether row `y` is nothing but `─` — a full-width rule.
fn is_rule_row(buffer: &ratatui::buffer::Buffer, y: u16) -> bool {
    let text = row_text(buffer, y);
    !text.trim().is_empty() && text.chars().all(|c| c == '─')
}

/// The input area's bottom rule — the last row made entirely of `─`. It
/// sits directly below the editor and doubles as the popup slot's upper
/// boundary (message round separators carry a timestamp, so they never
/// read as a pure rule). Popup tests anchor on it.
fn rule_row(buffer: &ratatui::buffer::Buffer) -> u16 {
    // The status line (the pane's last row) may carry a full-width bar glyph
    // run that is not `─`, but the cost chip's `━` disqualifies it already;
    // skip at most the status line plus the popup's window to find the rule.
    (0..buffer.area.height)
        .rev()
        .find(|&y| is_rule_row(buffer, y) && y + 1 < buffer.area.height)
        .expect("input bottom rule rendered")
}

/// Column of `needle` in row `y`, counted in cells: a row string holds one
/// char per cell, so a byte offset would not survive the `→`.
#[cfg(test)]
#[allow(dead_code)]
fn col_of(buffer: &ratatui::buffer::Buffer, y: u16, needle: &str) -> u16 {
    let text = row_text(buffer, y);
    let byte = text.find(needle).expect("needle rendered");
    text[..byte].chars().count() as u16
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

/// An app with one open question carrying `context` — the box's input in every
/// test about how much of that context fits.
fn app_with_question(context: Option<String>) -> App {
    let mut app = chatting_app();
    app.chat.questions = vec![PendingQuestion {
        topic: "jyc".to_string(),
        context,
        question: "Which one?".to_string(),
        options: vec!["alpha".to_string(), "beta".to_string()],
        multi: false,
        skipped: false,
        typed: None,
        selected: 0,
        marked: Vec::new(),
        answer_as: Default::default(),
    }];
    app
}

/// A long narration gets ten rows, marked as cut, and measured the same way by
/// the renderer and the layout: an uncapped one would push the question and its
/// options out of a box whose height is fixed to the body around it.
#[test]
fn long_question_context_fills_ten_rows_on_a_tall_terminal() {
    let long = "A narration long enough to fill the box. ".repeat(40);
    let rows = question_context_lines(&long, 78, QUESTION_CONTEXT_ROWS);
    assert_eq!(rows.len(), QUESTION_CONTEXT_ROWS, "ten rows: {rows:?}");
    assert!(
        rows.last().unwrap().ends_with('\u{2026}'),
        "the cut is marked: {rows:?}"
    );

    let mut app = app_with_question(Some(long.clone()));
    let buffer = draw_sized(&mut app, 80, 60);
    let pane = pane_text(&buffer);
    assert_eq!(
        rows_showing(&buffer, "narration"),
        QUESTION_CONTEXT_ROWS,
        "ten context rows drawn:\n{pane}"
    );
    assert!(
        pane.contains("Which one?"),
        "the question survives:\n{pane}"
    );
    assert!(
        pane.contains("alpha") && pane.contains("beta"),
        "the options survive:\n{pane}"
    );
    // A 60-row terminal affords the whole cap, and the layout counts the same
    // rows the box draws.
    assert_eq!(
        question_context_budget("Which one?", 3, 1, 78, 53, false),
        QUESTION_CONTEXT_ROWS
    );
    assert_eq!(
        question_chrome_rows("Which one?", Some(&long), 3, 1, 78, 53, false),
        question_chrome_rows("Which one?", None, 3, 1, 78, 53, false) + QUESTION_CONTEXT_ROWS + 1,
        "the layout must measure exactly what the box draws"
    );
}

/// On a terminal with no room for ten rows the context is what gives way: the
/// question and its options — the reason the box is open — stay drawn, and the
/// box never grows past the rows the body had to spare.
#[test]
fn a_short_terminal_shrinks_the_context_not_the_options() {
    let long = "A narration long enough to fill the box. ".repeat(40);
    let mut app = app_with_question(Some(long.clone()));
    let buffer = draw_sized(&mut app, 80, 18);
    let pane = pane_text(&buffer);
    let drawn = rows_showing(&buffer, "narration");
    assert!(
        drawn > 0 && drawn < QUESTION_CONTEXT_ROWS,
        "the context gives rows back first: {drawn} of {QUESTION_CONTEXT_ROWS}\n{pane}"
    );
    assert!(
        pane.contains("Which one?"),
        "the question survives:\n{pane}"
    );
    assert!(
        pane.contains("alpha") && pane.contains("beta"),
        "the options survive:\n{pane}"
    );
    // An 18-row screen hands the chat pane 18 rows of body, and that is what
    // the box measured — the same number `input_area_rows` is handed below.
    // `3` is the list's two options plus the `✎` row closing it: the box draws
    // that row too, so it is part of what the budget is measured against.
    assert_eq!(
        question_context_budget("Which one?", 3, 1, 78, 18, false),
        drawn,
        "the budget is what the box draws"
    );
    for body in [18u16, 60] {
        let rows = app.chat.input_area_rows(80, body) as usize;
        assert!(
            rows <= body as usize - QUESTION_BOX_MESSAGE_ROWS + 1,
            "body {body}: the box took {rows} rows, leaving the pane none"
        );
    }
}

/// The input area measures top rule + body + the bottom rule in one place.
/// The body math is pre-existing; this pins the +1 rule row for both the
/// plain editor and an active question box.
#[test]
fn input_area_rows_reserves_the_bottom_rule() {
    let mut app = chatting_app();
    let body_height = 17; // a 24-row terminal's body, once the header is off
    let body = (count_wrapped_lines(&app.chat.text(), 80) + 1).clamp(2, 11) as u16;
    assert_eq!(app.chat.input_area_rows(80, body_height), body + 1);

    app.chat.questions = vec![PendingQuestion {
        topic: "jyc".to_string(),
        context: None,
        question: "Pick one?".to_string(),
        options: vec!["alpha".to_string(), "beta".to_string()],
        multi: false,
        skipped: false,
        typed: None,
        selected: 0,
        marked: Vec::new(),
        answer_as: Default::default(),
    }];
    let q = app.chat.current_question().expect("question");
    let body = (question_chrome_rows(
        &q.question,
        q.context.as_deref(),
        q.options.len() + 1,
        app.chat.questions.len(),
        78,
        body_height,
        app.chat.question_typing,
    ) + q.options.len()
        + 1
        + 3)
    .clamp(6, question_box_max_rows(body_height)) as u16;
    assert_eq!(app.chat.input_area_rows(80, body_height), body + 1);

    // The draft's hint is a shorter line than the list's, and the box is sized
    // from whichever one is showing: at 60 columns the list's hint wraps onto
    // two rows while the draft's stays on one, so the box comes out a row
    // shorter. A mismatch here would be a row drawn over or clipped.
    let question = q.question.clone();
    let context = q.context.clone();
    let options = q.options.len();
    let total = app.chat.questions.len();
    app.chat.question_typing = true;
    let typing_chrome = question_chrome_rows(
        &question,
        context.as_deref(),
        options + 1,
        total,
        58,
        body_height,
        true,
    );
    let list_chrome = question_chrome_rows(
        &question,
        context.as_deref(),
        options + 1,
        total,
        58,
        body_height,
        false,
    );
    assert_eq!(
        list_chrome - typing_chrome,
        1,
        "the draft's hint must be measured as itself, not as the list's"
    );
    assert_eq!(
        app.chat.input_area_rows(60, body_height),
        (typing_chrome + options + 1 + 3 + 1) as u16,
        "the box's own height follows the hint that is showing"
    );
}

/// A polled topic with its detail, so the status line has real content:
/// `plan · jyc · main` on the left, `[ kimi/kimi-for-coding (auto) ·
/// 10% ]` on the right. The list carries the topic, the detail its figures — the same
/// split the server keeps.
fn app_with_topic_summary() -> App {
    let mut app = chatting_app();
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![jyc_types::TopicSummary {
            name: "jyc".to_string(),
            channel: "agents".to_string(),
            agent: None,
            pattern: None,
            status: jyc_types::TopicStatus::Idle,
            context_input_tokens: Some(1_000),
            context_output_tokens: None,
            max_tokens: Some(10_000),
            last_active_at: None,
            topic_path: None,
            parent: None,
            commands: vec![],
        }],
        ..Default::default()
    });
    app.topic_detail = Some(jyc_types::TopicInfo {
        name: "jyc".to_string(),
        reasoning_effort: None,
        channel: "agents".to_string(),
        pattern: None,
        status: jyc_types::TopicStatus::Idle,
        queued: 0,
        queue_capacity: 0,
        parent: None,
        model: Some("kimi/kimi-for-coding".to_string()),
        mode: Some("plan".to_string()),
        context_input_tokens: Some(1_000),
        context_output_tokens: None,
        max_tokens: Some(10_000),
        context_composition: Vec::new(),
        output_tokens: None,
        turn_output_tokens: None,
        total_input_tokens: None,
        total_cache_hit_tokens: None,
        total_cache_creation_tokens: None,
        total_reasoning_tokens: None,
        turn_reasoning_tokens: None,
        turn_cache_hit_tokens: None,
        turn_input_tokens: None,
        today_all: Vec::new(),
        avg_call_ms: None,
        call_count: None,
        avg_turn_call_ms: None,
        turn_call_count: None,
        turn_total_call_duration_ms: None,
        activity: vec![],
        last_active_at: None,
        skills: vec![],
        available_skills: vec![],
        recent_messages: vec![],
        thinking_text: None,
        topic_path: None,
        branch: Some("main".to_string()),
        changed_files: None,
        tasks: Default::default(),
        cost: None,
        commands: vec![],
        subagents: None,
    });
    app
}

/// The status line owns the pane's last row: below the input field and the
/// popup slot, gray, with no rule of its own.
#[test]
fn status_line_renders_at_the_bottom_below_the_popup() {
    let mut app = app_with_topic_summary();
    handle_chat_keys(
        &mut app,
        crossterm::event::KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        &mut test_terminal(),
    );
    let buffer = draw_sized(&mut app, 87, 24);

    let bottom = row_text(&buffer, buffer.area.height - 1);
    assert!(
        bottom.starts_with("plan · jyc · main"),
        "status line takes the last row: {bottom:?}"
    );
    let bar = "━".repeat(10);
    assert!(
        bottom.trim_end().ends_with(&format!(
            "[ kimi/kimi-for-coding (auto) · {bar} 10% ↑0/0% ↓0/0 →0s/0 ]"
        )),
        "model chip stays right-aligned on the status line: {bottom:?}"
    );
    assert!(
        !bottom.contains('─'),
        "the status line is a footer, not a rule: {bottom:?}"
    );
    // The popup keeps the slot between the input rule and the status line.
    let rule = rule_row(&buffer);
    assert!(
        row_text(&buffer, rule + 1).contains('/'),
        "popup starts right below the input rule: {:?}",
        row_text(&buffer, rule + 1)
    );
    assert!(
        rule + 1 < buffer.area.height - 1,
        "the popup sits above the status line"
    );
}

/// Without a popup open, the status line is the row directly under the
/// input's bottom rule — padded gray, apart from the dark-gray empty
/// composition cells inside the right chip.
#[test]
fn status_line_sits_right_below_the_rule_and_keeps_padding_gray() {
    let mut app = app_with_topic_summary();
    // The chip grew by "(auto) " (7 cols); 87 still fits it whole.
    let buffer = draw_sized(&mut app, 87, 24);

    let rule = rule_row(&buffer);
    let status = rule + 1;
    assert_eq!(status, buffer.area.height - 1, "status takes the last row");
    assert!(
        row_text(&buffer, status).starts_with("plan · jyc"),
        "{:?}",
        row_text(&buffer, status)
    );
    // The gap around the segments stays gray, while the fixture's empty
    // composition occupies ten dark-gray bar cells.
    let bar_start = (0..buffer.area.width as usize)
        .find(|&x| buffer[(x as u16, status)].symbol() == "━")
        .expect("status chip draws the empty-composition bar");
    for x in 0..buffer.area.width {
        let cell = &buffer[(x, status)];
        let expected_fg = if (bar_start..bar_start + 10).contains(&(x as usize)) {
            Color::DarkGray
        } else {
            Color::Gray
        };
        assert_eq!(
            cell.style().fg,
            Some(expected_fg),
            "status cell {x} renders {expected_fg:?}: {cell:?}"
        );
    }
}

/// The input field is framed: a full-width rule directly above it and
/// another directly below — the header row's old slot is the top rule.
#[test]
fn input_field_has_a_top_and_bottom_rule() {
    let mut app = app_with_topic_summary();
    handle_chat_keys(
        &mut app,
        crossterm::event::KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        &mut test_terminal(),
    );
    let buffer = draw_80x24(&mut app);

    let bottom_rule = rule_row(&buffer);
    // The first non-blank row above the bottom rule is the editor's text: the
    // editor chunk may be taller than the text (a spare row for the next
    // line), so the text can sit more than one row above the rule.
    let editor = (0..bottom_rule)
        .rev()
        .find(|&y| !row_text(&buffer, y).trim().is_empty())
        .expect("editor text rendered");
    assert!(
        row_text(&buffer, editor).contains('/'),
        "the editor row sits directly above the bottom rule: {:?}",
        row_text(&buffer, editor)
    );
    let top_rule = (0..editor)
        .rev()
        .find(|&y| is_rule_row(&buffer, y))
        .expect("input top rule rendered");
    assert_eq!(top_rule + 1, editor, "the top rule sits on the editor");
    assert_eq!(
        row_text(&buffer, top_rule).trim(),
        "─".repeat(buffer.area.width as usize),
        "both rules span the full pane width"
    );
}

#[test]
fn esc_does_not_close_chat() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;

    handle_chat_keys(&mut app, esc_key(), &mut test_terminal());
    assert!(app.chat.visible, "Esc must not close the chat");
}

#[test]
fn leader_open_dashboard_closes_chat() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
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
    app.chat.topic = Some("jyc".to_string());
    app.chat.focus = Focus::ChatPane;
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    app.chat.ws_tx = Some(cmd_tx);

    assert!(app.chat.visible);
    assert_eq!(app.chat.topic.as_deref(), Some("jyc"));

    // close() is what Esc invokes — must return to overview
    app.chat.close();
    assert!(!app.chat.visible);
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

#[test]
fn handle_ws_message_routes_activity_events_to_live_buffer() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);

    // Simulate hydrate: seed an activity entry, then a WS event with
    // the same id arrives — should be deduped (id <= last_seen_activity_id).
    let entry = jyc_types::ActivityEntry {
        event: serde_json::Value::Null,
        text: Some("Tool: bash".to_string()),
        timestamp: Some("2026-01-01T00:00:00Z".to_string()),
        id: 42,
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
fn render_history_skips_thinking_pseudo_messages() {
    let msgs = vec![
        history_msg("user", "question", Some("2026-08-13T10:00:00Z")),
        history_msg("thinking", "secret chain of thought body", None),
        history_msg("ai", "answer", Some("2026-08-13T10:00:05Z")),
    ];
    let (lines, _turns) = render_history_lines(&msgs, 80);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    // Thinking is live furniture (see `render_thinking_block`), never a
    // printed transcript row — neither collapsed nor expanded.
    assert!(!text.contains("💭"), "got: {text}");
    assert!(!text.contains("secret chain of thought body"));
    assert!(text.contains("question"));
    assert!(text.contains("answer"));
    // The thinking message must not swallow the blank row between the
    // human block and the reply.
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

/// No speaker labels any more: the human side is a full-width background
/// block, the agent's reply stays on the pane background.
#[test]
fn render_history_marks_human_turns_with_background_not_labels() {
    let msgs = vec![
        history_msg("user", "question", Some("2026-08-13T10:00:00Z")),
        history_msg("ai", "answer", Some("2026-08-13T10:00:05Z")),
    ];
    let (lines, _turns) = render_history_lines(&msgs, 80);
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
    let (lines, _turns) = render_history_lines(&msgs, 80);
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
    let (lines, _turns) = render_history_lines(&msgs, 40);
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
            event: serde_json::Value::Null,
            text: Some("old".to_string()),
            timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            id: 1,
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
fn is_user_visible_activity_filters_heartbeats() {
    use jyc_types::ActivityEntry;

    let visible = ActivityEntry {
        event: jyc_core::topic_event::TopicEvent::ToolCompleted {
            topic_name: "t".to_string(),
            tool_name: "bash".to_string(),
            success: true,
            duration_secs: 1,
            output: None,
            input: None,
            timestamp: chrono::Utc::now(),
        }
        .to_activity_json(),
        text: None,
        timestamp: Some("2026-01-01T00:00:00Z".to_string()),
        id: 1,
    };
    assert!(is_user_visible_activity(&visible));

    // Heartbeat kinds are dropped by the tracker, but a live buffer may
    // still hold them — hidden by kind.
    let progress = ActivityEntry {
        event: jyc_core::topic_event::TopicEvent::ProcessingProgress {
            topic_name: "t".to_string(),
            elapsed_secs: 10,
            activity: "tool execution".to_string(),
            progress: None,
            parts_count: 1,
            output_length: 200,
            timestamp: chrono::Utc::now(),
        }
        .to_activity_json(),
        text: None,
        timestamp: Some("2026-01-01T00:00:00Z".to_string()),
        id: 2,
    };
    assert!(!is_user_visible_activity(&progress));

    let tick = ActivityEntry {
        event: jyc_core::topic_event::TopicEvent::LoopTick {
            topic_name: "t".to_string(),
            elapsed_ms: 1000,
            timestamp: chrono::Utc::now(),
        }
        .to_activity_json(),
        text: None,
        timestamp: Some("2026-01-01T00:00:00Z".to_string()),
        id: 4,
    };
    assert!(!is_user_visible_activity(&tick));

    // Legacy: prose shape for ProcessingProgress in pre-rotation log files.
    let legacy = ActivityEntry {
        event: serde_json::Value::Null,
        text: Some("tool execution (5s, 120 chars)".to_string()),
        timestamp: Some("2026-01-01T00:00:00Z".to_string()),
        id: 3,
    };
    assert!(!is_user_visible_activity(&legacy));
}

// ---------------------------------------------------------------------------
// Regression tests for the chat-pane fold
// (`ChatState::sync_live_chat`).
//
// Bug history: the original `(sender, text)` dedup silently dropped repeated
// runs of `/context` and similar deterministic-output commands. The fix gave
// every row a sequence number, allocated where the message enters the system;
// the pane then draws a row unless it already holds that number.
//
// `sync_live_chat` implements that as three cases:
//   1. Echo confirmation: a live entry carrying the client id of an
//      unconfirmed local echo (`id == 0`) replaces that echo in place.
//   2. Live entries (`id != 0`): append unless a row with that id is already
//      on screen. Membership, not a high-water mark — ids are allocated on
//      arrival but rows are written on delivery, so a hydrate can hand them
//      over out of order (`hydrate_draws_a_row_whose_number_is_behind_...`).
//   3. Rows with `id == 0` that match no echo: logged before the sequence
//      existed, so they have nothing to place them by — not drawn.
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
    let text = "/context: current strategy is sliding_window (window=6) (default)".to_string();
    push_live_chat(
        &mut app,
        "agents",
        "t",
        vec![
            jyc_types::ChatMessageEntry {
                client_msg_id: None,
                sender: "ai".into(),
                text: text.clone(),
                timestamp: Some("2026-01-01T00:00:01Z".into()),
                id: 2,
            },
            jyc_types::ChatMessageEntry {
                client_msg_id: None,
                sender: "ai".into(),
                text,
                timestamp: Some("2026-01-01T00:00:02Z".into()),
                id: 4,
            },
        ],
    );
    assert!(app.chat.sync_live_chat("agents", "t"));
    assert_eq!(app.chat.messages.len(), 2);
    // Second sync: both are already on screen, so nothing is appended.
    assert!(!app.chat.sync_live_chat("agents", "t"));
    assert_eq!(app.chat.messages.len(), 2);
}

#[test]
fn poll_sync_skips_rows_logged_before_the_chat_sequence() {
    // Rows logged before the sequence existed carry id 0. They are not
    // drawn: the sequence number is the whole dedup rule, and a row
    // outside the sequence has nothing to place it by. (They stay on
    // disk; the pane starts at the upgrade.)
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    let rows = (0..5)
        .map(|i| jyc_types::ChatMessageEntry {
            client_msg_id: None,
            sender: "ai".into(),
            text: format!("legacy-{i}"),
            timestamp: Some(format!("2026-01-01T00:00:0{i}Z")),
            id: 0,
        })
        .collect();
    push_live_chat(&mut app, "agents", "t", rows);
    for _ in 0..10 {
        app.chat.sync_live_chat("agents", "t");
    }
    assert_eq!(
        app.chat.messages.len(),
        0,
        "rows without a sequence number are not drawn"
    );
}

#[test]
fn poll_sync_server_row_replaces_the_local_echo() {
    // `send_message_inner` pushes the local echo with id 0 and a client id;
    // the server broadcasts the same message back with a sequence number
    // and that same client id. The row must replace the echo in place, not
    // be appended beside it.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.send_message_inner("/hello".into());
    let client_id = app
        .chat
        .messages
        .last()
        .and_then(|m| m.client_id.clone())
        .expect("echo carries a client id");
    assert_eq!(app.chat.messages[0].id, 0, "echo is unconfirmed");

    push_live_chat(
        &mut app,
        "agents",
        "t",
        vec![jyc_types::ChatMessageEntry {
            client_msg_id: Some(client_id),
            sender: "user".into(),
            text: "/hello".into(),
            timestamp: Some("2026-01-01T00:00:00.500Z".into()),
            id: 7,
        }],
    );
    assert!(app.chat.sync_live_chat("agents", "t"));

    let user_msgs: Vec<&ChatMessage> = app
        .chat
        .messages
        .iter()
        .filter(|m| m.sender == "user")
        .collect();
    assert_eq!(user_msgs.len(), 1, "the server row replaced the echo");
    assert_eq!(user_msgs[0].id, 7, "the echo adopted the server's sequence");
    assert_eq!(
        user_msgs[0].timestamp.as_deref(),
        Some("2026-01-01T00:00:00.500Z"),
        "the echo adopted the server's timestamp"
    );

    // A repeated fold must not append the confirmed row again.
    assert!(!app.chat.sync_live_chat("agents", "t"));
    assert_eq!(app.chat.messages.len(), 1);
}

#[test]
fn poll_sync_keeps_two_identical_messages() {
    // The bug the sequence fixes: two messages with the same sender and
    // text used to collapse into one, because text was the only key.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    push_live_chat(
        &mut app,
        "agents",
        "t",
        vec![
            jyc_types::ChatMessageEntry {
                client_msg_id: None,
                sender: "user".into(),
                text: "ok".into(),
                timestamp: Some("2026-01-01T00:00:00Z".into()),
                id: 3,
            },
            jyc_types::ChatMessageEntry {
                client_msg_id: None,
                sender: "user".into(),
                text: "ok".into(),
                timestamp: Some("2026-01-01T00:00:05Z".into()),
                id: 5,
            },
        ],
    );
    assert!(app.chat.sync_live_chat("agents", "t"));
    assert_eq!(
        app.chat.messages.len(),
        2,
        "identical text does not make a message a duplicate"
    );
}

#[test]
fn poll_sync_command_repeats_with_historical_backdrop() {
    // The user's exact scenario: 30 logged rows plus 3 fresh `/context`
    // commands. All 3 user echoes + 3 AI replies must be present, and the
    // logged rows must not be re-pushed across polls.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    // Seed 30 logged rows: 15 user + 15 ai, distinct text.
    let mut hist: Vec<jyc_types::ChatMessageEntry> = (0..15)
        .flat_map(|i| {
            [
                jyc_types::ChatMessageEntry {
                    client_msg_id: None,
                    sender: "user".into(),
                    text: format!("old-user-{i}"),
                    timestamp: Some(format!("2026-01-01T00:00:{i:02}Z")),
                    id: 2 * i as u64 + 1,
                },
                jyc_types::ChatMessageEntry {
                    client_msg_id: None,
                    sender: "ai".into(),
                    text: format!("old-ai-{i}"),
                    timestamp: Some(format!("2026-01-01T00:01:{i:02}Z")),
                    id: 2 * i as u64 + 2,
                },
            ]
        })
        .collect();
    push_live_chat(&mut app, "agents", "t", hist.clone());

    // User types /context three times. Each typing pushes a local echo; the
    // server echoes the message back through live_chat with a fresh id and
    // the echo's client id, followed by the AI reply.
    let ai_text = "/context: current strategy is sliding_window (window=6) (default)".to_string();
    for round in 0..3 {
        app.chat.send_message_inner("/context".into());
        let client_id = app
            .chat
            .messages
            .last()
            .and_then(|m| m.client_id.clone())
            .expect("the echo carries a client id");
        hist.push(jyc_types::ChatMessageEntry {
            client_msg_id: Some(client_id),
            sender: "user".into(),
            text: "/context".into(),
            timestamp: Some(format!("2026-02-01T00:0{round}:00Z")),
            id: 100 + round as u64 * 2,
        });
        hist.push(jyc_types::ChatMessageEntry {
            client_msg_id: None,
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
    app.chat.sync_live_chat("agents", "t");
    // Two extra sync cycles must not change the message count.
    app.chat.sync_live_chat("agents", "t");
    app.chat.sync_live_chat("agents", "t");

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
fn revisit_rehydrates_every_row() {
    // Closing and reopening a topic clears the pane (`reset_chat_state`) and
    // hydrates the same rows again. They must all come back: the pane is its
    // own record of what it drew, so a cleared pane draws everything.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    let historical: Vec<jyc_types::ChatMessageEntry> = (0..10)
        .map(|i| jyc_types::ChatMessageEntry {
            client_msg_id: None,
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
    app.chat.sync_live_chat("agents", "t");
    assert_eq!(app.chat.messages.len(), 10);
    // Second visit: simulate `open()` clearing `messages` + a fresh
    // REST hydrate via `seed_live`.
    app.chat.messages.clear();
    app.chat
        .seed_live("agents", "t", vec![], historical.clone());
    app.chat.sync_live_chat("agents", "t");
    assert_eq!(
        app.chat.messages.len(),
        10,
        "all 10 hydrated historical rows must be re-pushed on revisit"
    );
}

fn history_row(sender: &str, text: &str, id: u64) -> jyc_types::ChatMessageEntry {
    jyc_types::ChatMessageEntry {
        client_msg_id: None,
        sender: sender.to_string(),
        text: text.to_string(),
        timestamp: Some(format!("2026-01-01T00:00:{id:02}Z")),
        id,
    }
}

#[test]
fn ws_chat_message_reaches_the_pane_without_a_poll() {
    // The pane is fed where the buffer is written, not by a timer: a reply
    // shows up at websocket speed rather than at the poll interval.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.channel = Some("chan".to_string());
    app.chat.topic = Some("t1".to_string());

    app.chat.handle_live_event(&serde_json::json!({
        "type": "chat_message",
        "channel": "chan",
        "topic": "t1",
        "id": 7,
        "entry": {"sender": "ai", "text": "hello", "timestamp": "2026-01-01T00:00:07Z", "id": 7},
    }));

    assert_eq!(app.chat.messages.len(), 1);
    assert_eq!(app.chat.messages[0].text, "hello");
}

#[test]
fn ws_chat_message_for_another_topic_stays_in_its_buffer() {
    // A background topic streams too; its rows must not land in the pane the
    // user is reading.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.channel = Some("chan".to_string());
    app.chat.topic = Some("t1".to_string());

    app.chat.handle_live_event(&serde_json::json!({
        "type": "chat_message",
        "channel": "chan",
        "topic": "other",
        "id": 3,
        "entry": {"sender": "ai", "text": "elsewhere", "timestamp": "2026-01-01T00:00:03Z", "id": 3},
    }));

    assert!(app.chat.messages.is_empty());
    assert_eq!(app.chat.live_chat_for("chan", "other").count(), 1);
}

#[test]
fn resync_hydrate_does_not_repush_rows_already_on_screen() {
    // A resync hydrate replaces the buffer with the same rows the pane is
    // already showing; matching them by sequence number is what keeps them
    // from being stacked a second time.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.channel = Some("chan".to_string());
    app.chat.topic = Some("t1".to_string());
    let history: Vec<_> = (1..=3)
        .map(|i| history_row("ai", &format!("history-{i}"), i))
        .collect();

    app.chat.seed_live("chan", "t1", vec![], history.clone());
    assert!(app.chat.sync_live_if_visible("chan", "t1"));
    assert_eq!(app.chat.messages.len(), 3);

    // Resync: the socket fell behind, so the buffers are dropped and the same
    // history is re-hydrated.
    app.chat.handle_live_event(&serde_json::json!({
        "type": "resync",
        "channel": "chan",
        "topic": "t1",
    }));
    assert_eq!(app.chat.live_chat_for("chan", "t1").count(), 0);
    // The pane still holds the three rows, so the re-hydrated copies match by
    // sequence number and are not drawn a second time.
    app.chat.seed_live("chan", "t1", vec![], history.clone());
    app.chat.sync_live_if_visible("chan", "t1");
    assert_eq!(
        app.chat.messages.len(),
        3,
        "a resync hydrate must not re-push rows already on screen"
    );

    // Rows the pane has not drawn still land after the resync.
    let mut with_new = history;
    with_new.push(history_row("ai", "fresh", 4));
    app.chat.seed_live("chan", "t1", vec![], with_new);
    app.chat.sync_live_if_visible("chan", "t1");
    assert_eq!(app.chat.messages.len(), 4);
    assert_eq!(app.chat.messages[3].text, "fresh");
}

#[test]
fn hydrate_draws_a_row_whose_number_is_behind_the_newest() {
    // Numbers are allocated on arrival but rows are written on delivery, so a
    // message injected mid-turn is logged *after* replies that took higher
    // numbers. A resync hydrate therefore hands rows over out of id order; a
    // high-water mark would skip the reordered row for the rest of the
    // session, even though it never reached the pane.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.channel = Some("chan".to_string());
    app.chat.topic = Some("t1".to_string());

    // On screen: only the heartbeat reply, which took the higher number.
    app.chat.seed_live(
        "chan",
        "t1",
        vec![],
        vec![history_row("ai", "heartbeat", 110)],
    );
    assert!(app.chat.sync_live_if_visible("chan", "t1"));
    assert_eq!(app.chat.messages.len(), 1);

    // The socket fell behind: a resync drops the buffer, and the hydrate
    // returns the message the agent received mid-turn (logged later, lower
    // number) beside the reply that was already drawn.
    app.chat.handle_live_event(&serde_json::json!({
        "type": "resync",
        "channel": "chan",
        "topic": "t1",
    }));
    app.chat.seed_live(
        "chan",
        "t1",
        vec![],
        vec![
            history_row("ai", "heartbeat", 110),
            history_row("user", "typed mid-turn", 109),
        ],
    );
    assert!(app.chat.sync_live_if_visible("chan", "t1"));
    assert_eq!(
        app.chat.messages.len(),
        2,
        "the reordered row must reach the pane, the drawn one must not repeat"
    );
    assert_eq!(app.chat.messages[1].text, "typed mid-turn");

    // Folding the same buffer again adds nothing.
    assert!(!app.chat.sync_live_if_visible("chan", "t1"));
    assert_eq!(app.chat.messages.len(), 2);
}

// ── Message-pane cursor ───────────────────────────────────────────────────

/// A chat whose message pane is focused and drawn once, so the renderer has
/// measured the geometry the cursor moves against (the keys always work on last
/// A pending question box owns the keyboard, so the TUI's own shortcuts stay
/// out of it: `/` must not open the command popup (the popup's next Enter would
/// answer the question with a slash command) and `[` must not move the focus to
/// the transcript. Both are guarded on the box being up, draft or not.
#[test]
fn question_box_keeps_the_slash_and_bracket_shortcuts_out() {
    let mut app = chatting_app();
    app.chat.focus = Focus::ChatPane;
    app.chat.questions = vec![PendingQuestion {
        topic: "jyc".to_string(),
        context: None,
        question: "Pick one?".to_string(),
        options: vec!["alpha".to_string(), "beta".to_string()],
        multi: false,
        skipped: false,
        typed: None,
        selected: 0,
        marked: Vec::new(),
        answer_as: Default::default(),
    }];
    assert!(app.chat.active_question(), "the box must be on screen");

    press(&mut app, '/');
    assert!(
        app.chat.command_popup.is_none(),
        "the box owns the keyboard: no command popup over it"
    );
    assert_eq!(
        app.chat.text(),
        "",
        "and the slash does not reach the editor"
    );

    press(&mut app, '[');
    assert_eq!(
        app.chat.focus,
        Focus::ChatPane,
        "the box owns the keyboard: the bracket does not leave the input"
    );
}

/// `[` is special only when the input is empty: with a draft in it the
/// bracket is an ordinary character and the focus does not move.
#[test]
fn bracket_with_a_non_empty_input_is_typed() {
    let mut app = chatting_app();
    app.chat.focus = Focus::ChatPane;
    press(&mut app, 'x');

    press(&mut app, '[');

    assert_eq!(app.chat.text(), "x[", "the bracket is inserted as text");
    assert_eq!(app.chat.focus, Focus::ChatPane, "focus stays on the input");
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

/// Wiring test for the mode itself: `p` has to be a chat-scoped command whose
/// dispatch moves the flag the render reads. The mode starts minimal — a
/// restarted TUI should not need re-toggling — so the first toggle expands.
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
    for expected in [false, true] {
        super::execute_local_action(&mut app, &mut terminal, cmd.action);
        assert_eq!(app.chat.minimal_progress, expected);
    }
}

#[test]
fn protect_leading_whitespace_replaces_all_leading_spaces() {
    let nbsp = |n: usize| "\u{00A0}".repeat(n);
    assert_eq!(
        protect_leading_whitespace("  two\n   three\n    four\n          ten\nnone"),
        format!(
            "{}two\n{}three\n{}four\n{}ten\nnone",
            nbsp(2),
            nbsp(3),
            nbsp(4),
            nbsp(10)
        )
    );
}

#[test]
fn protect_leading_whitespace_skips_fences_and_blank_lines() {
    let md = "```\n  fenced\n```\n  after\n   \nend";
    assert_eq!(
        protect_leading_whitespace(md),
        "```\n  fenced\n```\n\u{00A0}\u{00A0}after\n   \nend"
    );
}

fn rendered_text(lines: &[ratatui::text::Line<'_>]) -> String {
    lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
        .collect()
}

#[test]
fn user_message_leading_spaces_survive_markdown_rendering() {
    let messages = vec![history_msg("user", "  indented\nplain", None)];
    let (lines, _) = render_history_lines(&messages, 40);
    let text = rendered_text(&lines);
    assert!(
        text.contains('\u{00A0}'),
        "leading spaces must survive as NBSP, got: {text:?}"
    );
    assert!(text.contains("indented"));
}

#[test]
fn user_message_deep_indent_renders_without_code_fence() {
    let messages = vec![history_msg("user", "    Hello", None)];
    let (lines, _) = render_history_lines(&messages, 40);
    let text = rendered_text(&lines);
    assert!(
        text.contains('\u{00A0}'),
        "deep indent must be preserved, got: {text:?}"
    );
    assert!(
        !text.contains("```"),
        "indented text must not render as a code block, got: {text:?}"
    );
}

#[test]
fn ai_message_leading_spaces_render_unchanged() {
    let messages = vec![history_msg("ai", "  indented", None)];
    let (lines, _) = render_history_lines(&messages, 40);
    let text = rendered_text(&lines);
    assert!(
        !text.contains('\u{00A0}'),
        "agent markdown must not be rewritten, got: {text:?}"
    );
}

#[test]
fn send_message_preserves_leading_whitespace() {
    let mut app = chatting_app();
    app.chat.populate_editor("  indented draft  ");
    app.chat.send_message();
    assert_eq!(app.chat.input_history.last().unwrap(), "  indented draft");
}

#[cfg(test)]
mod part2;
