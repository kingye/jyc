//! Second half of the chat tests.
//!
//! Split from the monolithic `dashboard/chat/tests.rs`.

use super::super::*;

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
    // Topic info pane and status bar default to visible; activity,
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
        topics: (0..5)
            .map(|i| jyc_types::TopicSummary {
                name: format!("t{i}"),
                channel: "test".to_string(),
                pattern: None,
                status: jyc_types::TopicStatus::Idle,
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
                topic_path: None,
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
fn opening_explorer_snaps_selection_to_chat_topic() {
    // Regression: the explorer opened on a stale row because
    // sync_explorer_selection only follows the chat topic while
    // the explorer is unfocused — and opening focuses it.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.state = Some(jyc_types::InspectOverview {
        topics: (0..5)
            .map(|i| jyc_types::TopicSummary {
                name: format!("t{i}"),
                channel: "test".to_string(),
                pattern: None,
                status: jyc_types::TopicStatus::Idle,
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
                topic_path: None,
                cost: None,
            })
            .collect(),
        ..Default::default()
    });
    app.chat.topic = Some("t2".to_string());
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
fn opening_explorer_keeps_selection_when_chat_topic_not_in_list() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![jyc_types::TopicSummary {
            name: "t0".to_string(),
            channel: "test".to_string(),
            pattern: None,
            status: jyc_types::TopicStatus::Idle,
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
            topic_path: None,
            cost: None,
        }],
        ..Default::default()
    });
    // Chat is bound to a topic absent from the overview (e.g. a
    // fresh adhoc topic not yet polled).
    app.chat.topic = Some("missing".to_string());
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
        topics: vec![
            jyc_types::TopicSummary {
                name: "current".to_string(),
                channel: "local_dev".to_string(),
                pattern: None,
                status: jyc_types::TopicStatus::Idle,
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
                topic_path: None,
                cost: None,
            },
            jyc_types::TopicSummary {
                name: "other".to_string(),
                channel: "local_dev".to_string(),
                pattern: None,
                status: jyc_types::TopicStatus::Idle,
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
                topic_path: None,
                cost: None,
            },
        ],
        ..Default::default()
    });
    app.chat.explorer_visible = true;
    app.chat.explorer_selected = 1;

    explorer_open_selected(&mut app);

    assert!(!app.chat.explorer_visible);
    assert_eq!(app.chat.topic.as_deref(), Some("other"));
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

    // Short topic name so the missing highlight (the bug) would leave
    // most of the row uncolored. The selection background must extend
    // to the pane's right edge, not just under the topic-name text.
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.explorer_visible = true;
    app.chat.focus = ChatFocus::ExplorerPane;
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![jyc_types::TopicSummary {
            name: "x".to_string(),
            channel: "test".to_string(),
            pattern: None,
            status: jyc_types::TopicStatus::Idle,
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
            topic_path: None,
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
        title_row.starts_with("── Topics"),
        "explorer title row should start with `── Topics`, got: {title_row:?}"
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
    app.chat.topic = Some("jyc".to_string());
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

/// Regression: the topic info pane title row must start with the `──`
/// prefix and the inner content area must start at y=1 (the top border
/// row acts as a separator).
#[test]
fn topic_info_pane_title_has_double_dash_prefix() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.phase = ChatPhase::Chatting;
    app.chat.topic = Some("jyc".to_string());
    app.chat.channel = Some("local_dev".to_string());
    app.chat.info_visible = true;
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![jyc_types::TopicSummary {
            name: "jyc".to_string(),
            channel: "local_dev".to_string(),
            pattern: Some("jyc".to_string()),
            status: jyc_types::TopicStatus::Idle,
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
            topic_path: None,
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
        .draw(|frame| render_topic_info_pane(frame, frame.area(), &mut app))
        .expect("draw");

    let buffer = terminal.backend().buffer().clone();
    let title_row: String = (0..width)
        .map(|x| buffer[(x, 0)].symbol().to_string())
        .collect();
    assert!(
        title_row.contains("── Topic Info"),
        "topic info title row should contain `── Topic Info`, got: {title_row:?}"
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
    app.chat.topic = Some("jyc".to_string());
    app.chat.channel = Some("local_dev".to_string());
    app.chat.info_visible = true;
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![jyc_types::TopicSummary {
            name: "jyc".to_string(),
            channel: "local_dev".to_string(),
            pattern: Some("jyc".to_string()),
            status: jyc_types::TopicStatus::Idle,
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
            topic_path: None,
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
        .draw(|frame| render_topic_info_pane(frame, frame.area(), &mut app))
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
    app.chat.topic = Some("jyc".to_string());
    app.chat.channel = Some("local_dev".to_string());
    app.chat.info_visible = true;
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![jyc_types::TopicSummary {
            name: "jyc".to_string(),
            channel: "local_dev".to_string(),
            pattern: Some("jyc".to_string()),
            status: jyc_types::TopicStatus::Idle,
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
            topic_path: None,
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
        .draw(|frame| render_topic_info_pane(frame, frame.area(), &mut app))
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
