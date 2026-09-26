//! Second half of the chat tests.
//!
//! Split from the monolithic `dashboard/chat/tests.rs`.

use super::super::*;

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
                tasks: Default::default(),
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
                commands: vec![],
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
                tasks: Default::default(),
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
                commands: vec![],
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
            tasks: Default::default(),
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
            commands: vec![],
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
                tasks: Default::default(),
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
                commands: vec![],
            },
            jyc_types::TopicSummary {
                tasks: Default::default(),
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
                commands: vec![],
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

/// Regression: the explorer row under the cursor marks the selection with the
/// same two-column `→` gutter + DIM as the command/question popups — no
/// background fill, and the status dot keeps its own color. The gutter is
/// reserved on every row so the topic names never shift sideways, and the
/// cursor stays visible while the pane itself is unfocused.
#[test]
fn explorer_selection_uses_arrow_gutter_and_keeps_status_dot() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.explorer_visible = true;
    // Focus stays on the chat input: only the border color reacts to focus.
    app.chat.focus = ChatFocus::ChatPane;
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![explorer_topic("selected"), explorer_topic("other")],
        ..Default::default()
    });
    app.chat.explorer_selected = 0;
    // The second topic is the one open in the chat pane: cyan + BOLD marks it
    // even though it is not the cursor row.
    app.chat.topic = Some("other".to_string());
    app.chat.channel = Some("test".to_string());

    let width = 24;
    let height = 6;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_explorer(frame, frame.area(), &app))
        .expect("draw");

    let buffer = terminal.backend().buffer().clone();
    // No title and no top border, but one row of top padding: the
    // selected row sits at y=1, the next topic at y=2. Both rows use a
    // two-column gutter, then the dot, then the name.
    assert_eq!(
        buffer[(0, 1)].symbol(),
        "→",
        "cursor row needs the arrow gutter even while unfocused"
    );
    assert_eq!(
        buffer[(width - 1, 1)].fg,
        Color::DarkGray,
        "the border is what focus changes"
    );
    assert_eq!(buffer[(1, 1)].symbol(), " ");
    assert_eq!(buffer[(2, 1)].symbol(), "●");
    assert_eq!(buffer[(4, 1)].symbol(), "s", "name follows gutter + dot");
    assert_eq!(
        buffer[(2, 1)].fg,
        Color::DarkGray,
        "idle status dot keeps its own color"
    );
    assert_eq!(
        buffer[(0, 1)].bg,
        Color::Reset,
        "selection paints no background"
    );
    assert_eq!(buffer[(4, 1)].bg, Color::Reset);
    assert!(buffer[(4, 1)].modifier.contains(Modifier::DIM));

    assert_eq!(buffer[(0, 2)].symbol(), " ");
    assert_eq!(buffer[(1, 2)].symbol(), " ");
    assert_eq!(buffer[(2, 2)].symbol(), "●");
    assert_eq!(
        buffer[(4, 2)].symbol(),
        "o",
        "unselected name stays in column"
    );
    assert_eq!(
        buffer[(4, 2)].fg,
        Color::Cyan,
        "the chat pane's topic stays marked when it is not the cursor row"
    );
    assert!(buffer[(4, 2)].modifier.contains(Modifier::BOLD));
    assert!(!buffer[(4, 2)].modifier.contains(Modifier::DIM));
}

/// The explorer window follows the cursor to the bottom of the pane, so the
/// last topic stays reachable — the same rule the popups use, with the cursor
/// row still marked while the pane is unfocused.
#[test]
fn explorer_scrolls_to_keep_the_cursor_visible() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.explorer_visible = true;
    app.chat.focus = ChatFocus::ChatPane;
    app.state = Some(jyc_types::InspectOverview {
        topics: (0..20)
            .map(|i| explorer_topic(&format!("t{i:02}")))
            .collect(),
        ..Default::default()
    });
    app.chat.explorer_selected = 19;

    let (width, height) = (24, 8);
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_explorer(frame, frame.area(), &app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect()
        })
        .collect();
    let pane = rows.join("\n");

    // One row of top padding, then the seven rows that fit: the window shows
    // t13..=t19 with the cursor on the last one.
    assert_eq!(
        buffer[(0, 7)].symbol(),
        "→",
        "cursor on the bottom row:\n{pane}"
    );
    assert!(
        rows[7].contains("t19"),
        "the last topic is visible:\n{pane}"
    );
    assert!(
        rows[1].contains("t13"),
        "the window slides with the cursor:\n{pane}"
    );
    assert!(
        !pane.contains("t0"),
        "the topics it walked over must scroll away:\n{pane}"
    );
}

/// Minimal idle topic for the explorer rendering tests.
fn explorer_topic(name: &str) -> jyc_types::TopicSummary {
    jyc_types::TopicSummary {
        tasks: Default::default(),
        name: name.to_string(),
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
        commands: vec![],
    }
}

/// Regression: the pattern list marks its cursor with the same two-column
/// `→` gutter + DIM as the command and question popups, and the unselected
/// rows keep an equally wide blank gutter so nothing shifts sideways.
#[test]
fn pattern_select_uses_arrow_gutter_aligned_with_unselected_rows() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.patterns = vec!["alpha".to_string(), "beta".to_string()];
    app.chat.pattern_selected = 1;

    let backend = TestBackend::new(20, 6);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_pattern_select(frame, frame.area(), &app))
        .expect("draw");

    let buffer = terminal.backend().buffer().clone();
    // Bordered block: both rows start at x=1, the name column at x=3.
    assert_eq!(
        buffer[(1, 1)].symbol(),
        " ",
        "unselected row keeps its gutter"
    );
    assert_eq!(
        buffer[(3, 1)].symbol(),
        "a",
        "unselected name stays in column"
    );
    assert_eq!(buffer[(1, 2)].symbol(), "→", "selected row needs the arrow");
    assert_eq!(
        buffer[(3, 2)].symbol(),
        "b",
        "selected name stays in column"
    );
    assert!(buffer[(3, 2)].modifier.contains(Modifier::DIM));
    assert!(!buffer[(3, 1)].modifier.contains(Modifier::DIM));
    assert_eq!(
        buffer[(1, 2)].bg,
        Color::Reset,
        "selection paints no background"
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
            tasks: Default::default(),
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
            commands: vec![],
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
            tasks: Default::default(),
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
            commands: vec![],
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
        topic: Some("jyc"),
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
fn header_line_includes_mode_topic_and_chip() {
    let ctx = ctx_with_full_data();
    let line = build_chat_header_line(80, &ctx, test_header_style(), LINE_DRAWING);
    let text = line_text(&line);
    // Left segment includes mode + topic.
    assert!(
        text.contains("╭─ plan · jyc"),
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
fn header_line_omits_topic_when_missing() {
    let mut ctx = ctx_with_full_data();
    ctx.topic = None;
    let line = build_chat_header_line(80, &ctx, test_header_style(), LINE_DRAWING);
    let text = line_text(&line);
    assert!(
        text.starts_with("╭─ plan"),
        "missing mode segment in: {text:?}"
    );
    assert!(!text.contains("· jyc"));
}

#[test]
fn header_line_with_no_state_is_just_mode_and_padding() {
    let ctx = ChatHeaderCtx {
        mode: "build",
        topic: None,
        branch: None,
        model: None,
        pct: None,
    };
    let line = build_chat_header_line(80, &ctx, test_header_style(), LINE_DRAWING);
    let text = line_text(&line);
    // Defaults: mode = "build", topic/branch all absent.
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
    ctx.topic = Some("a-very-long-topic-name");
    // Width so tight that even truncating topic to 3 chars barely fits.
    let line = build_chat_header_line(20, &ctx, test_header_style(), LINE_DRAWING);
    let text = line_text(&line);
    // Topic must be truncated to fit; no chip ever rendered.
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
        "branch segment should be appended after topic, got: {text:?}"
    );
}

#[test]
fn header_line_omits_branch_segment_when_none() {
    // Same ctx as `header_line_includes_mode_topic`
    // but with branch=None — the left segment must end at "· jyc"
    // without a dangling separator.
    let ctx = ctx_with_full_data();
    let line = build_chat_header_line(120, &ctx, test_header_style(), LINE_DRAWING);
    let text = line_text(&line);
    assert!(
        text.contains("· jyc "),
        "topic should still render, got: {text:?}"
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
    // Left "╭─ plan · jyc" = 13 display cols.
    // Chip "[ claude-opus-4-6 · 10% ]" = 23 display cols.
    // total = 35 cols + 2 padding spaces. Width 34 forces dropping
    // the chip and falls back to dash padding only.
    let line = build_chat_header_line(34, &ctx, test_header_style(), LINE_DRAWING);
    let text = line_text(&line);
    assert!(
        !text.contains('['),
        "chip should be dropped when narrow, got: {text:?}"
    );
    assert!(
        text.contains("╭─ plan · jyc"),
        "left segment should still render: {text:?}"
    );
    assert!(text.width() <= 34);
}

#[test]
fn header_line_never_emits_dangling_separator() {
    // Width fits "╭─ plan · " (10 cols) but no room for topic content.
    let ctx = ChatHeaderCtx {
        mode: "plan",
        topic: Some("ch"),
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
// Tests for the `ask_user` question box flow in the chat pane.

fn chat_for_topic(topic: &str) -> (ChatState, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut chat = ChatState::new(rx);
    chat.topic = Some(topic.to_string());
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    chat.ws_tx = Some(cmd_tx);
    (chat, cmd_rx)
}

fn question_payload(topic: &str, id: &str, options: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "question",
        "id": id,
        "channel": "chan",
        "topic": topic,
        "question": "Pick one?",
        "options": options,
        "timeout_seconds": 300,
    })
}

/// The same frame with `allow_multiple` set - what the daemon sends for a
/// question the user may answer with several options.
fn question_payload_multi(topic: &str, id: &str, options: &[&str]) -> serde_json::Value {
    let mut payload = question_payload(topic, id, options);
    payload["allow_multiple"] = serde_json::json!(true);
    payload
}

/// Multi-select: `Space` marks under the cursor, digits mark too, and Enter
/// sends every mark in the option list's own order.
#[test]
fn multi_select_marks_then_sends_every_mark() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload_multi("jyc", "q1", &["a", "b", "c"]));
    assert!(chat.current_question().unwrap().multi);

    chat.select_question_next(); // -> b
    chat.space_question(); // mark b
    chat.select_question_next(); // -> c
    chat.space_question(); // mark c
    chat.pick_question_idx(1); // digits mark too, and toggle back off
    assert_eq!(chat.current_question().unwrap().marked, vec![2]);

    chat.confirm_question();
    let frame: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(frame["choices"], serde_json::json!(["c"]));
    assert!(frame.get("choice").is_none(), "one frame shape");
    assert!(!chat.active_question());
}

/// Not one marked is the user backing out, not an empty answer.
#[test]
fn multi_select_enter_without_marks_cancels() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload_multi("jyc", "q1", &["a", "b"]));

    chat.confirm_question();

    let frame: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(frame["cancelled"], serde_json::json!(true));
    assert!(frame.get("choices").is_none());
    assert!(!chat.active_question());
}

/// `Space` is a new key: in single-select it confirms the highlighted option,
/// which is all it can do, and the answer stays a one-element list.
#[test]
fn single_select_space_confirms_the_highlighted_option() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));

    chat.select_question_next();
    chat.space_question(); // Space still confirms when only one may be picked

    let frame: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(frame["choices"], serde_json::json!(["b"]));
    assert!(!chat.active_question());
}

#[test]
fn question_event_surfaces_for_matching_topic() {
    let (mut chat, _rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));

    assert!(chat.active_question());
    let q = chat.current_question().expect("question stored");
    assert_eq!(q.id, "q1");
    assert_eq!(q.question, "Pick one?");
    assert_eq!(q.options, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(q.selected, 0);
}

#[test]
fn question_event_ignored_for_other_topic() {
    let (mut chat, _rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("other", "q1", &["a"]));
    assert!(!chat.active_question());
    assert!(chat.questions.is_empty());
}

#[test]
fn question_event_ignored_without_options() {
    let (mut chat, _rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &[]));
    assert!(chat.questions.is_empty());
}

/// A second question joins the queue rather than cancelling the first: one
/// `ask_user` call asking several questions must not tell the tool the user
/// backed out of the ones already on screen.
#[test]
fn second_question_queues_instead_of_cancelling_the_first() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a"]));
    chat.handle_question_event(&question_payload("jyc", "q2", &["b"]));

    assert!(
        rx.try_recv().is_err(),
        "queueing a question must not send a frame for the previous one"
    );
    assert_eq!(
        chat.current_question().expect("first stays on screen").id,
        "q1"
    );
    chat.step_question(1);
    assert_eq!(
        chat.current_question().expect("second is reachable").id,
        "q2"
    );
}

/// A queued batch belongs to the topic it was asked in, so switching topics
/// leaves it behind the way Esc does. Keeping it would hide the next topic's
/// question for good: `current_question` reads `questions[question_index]`, and
/// a first entry from the old topic makes `active_question` false forever - no
/// box, no keys, while the tool waits out its timeout.
#[test]
fn switching_topics_leaves_the_old_question_batch_behind() {
    let (mut chat, _rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "qA1", &["x"]));
    chat.handle_question_event(&question_payload("jyc", "qA2", &["y"]));
    assert_eq!(chat.questions.len(), 2, "the qA batch is queued");

    chat.select_pattern_inner("other".to_string());
    chat.handle_question_event(&question_payload("other", "qB1", &["z"]));

    assert!(
        chat.active_question(),
        "the new topic's question must reach the screen"
    );
    assert_eq!(chat.current_question().expect("qB1 on screen").id, "qB1");
    assert_eq!(chat.question_index, 0);
}

/// The batch is the point: an intermediate Enter sends nothing, and the last
/// one flushes every answer under its own question id.
#[test]
fn batch_sends_every_answer_once_the_last_question_is_settled() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));
    chat.handle_question_event(&question_payload("jyc", "q2", &["c", "d"]));

    chat.select_question_next(); // q1 -> b
    chat.confirm_question(); // settle q1, step to q2
    assert!(
        rx.try_recv().is_err(),
        "nothing may leave while questions remain - that is what makes going back possible"
    );
    chat.confirm_question(); // last question settles: flush

    let first: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(first["id"], "q1");
    assert_eq!(first["choices"], serde_json::json!(["b"]));
    let second: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(second["id"], "q2");
    assert_eq!(second["choices"], serde_json::json!(["c"]));
    assert!(!chat.active_question());
}

/// Going back is free precisely because the answer is still buffered: the
/// earlier question keeps its marks.
#[test]
fn stepping_back_keeps_the_earlier_marks() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload_multi("jyc", "q1", &["a", "b"]));
    chat.handle_question_event(&question_payload("jyc", "q2", &["c"]));

    chat.space_question(); // mark a on q1
    chat.confirm_question(); // -> q2
    chat.step_question(-1); // back to q1
    let current = chat.current_question().expect("q1 again");
    assert_eq!(current.id, "q1");
    assert_eq!(current.marked, vec![0], "the first mark survived");
    chat.select_question_next(); // cursor -> b
    chat.space_question(); // mark b too
    chat.step_question(1); // -> q2
    chat.confirm_question(); // flush

    let first: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(first["id"], "q1");
    assert_eq!(first["choices"], serde_json::json!(["a", "b"]));
}

#[test]
fn confirm_sends_choice_frame() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b", "c"]));
    chat.select_question_next();
    chat.select_question_next();
    chat.confirm_question();

    assert!(!chat.active_question());
    let frame = rx.try_recv().expect("response frame");
    let parsed: serde_json::Value = serde_json::from_str(&frame).unwrap();
    assert_eq!(parsed["type"], "question_response");
    assert_eq!(parsed["id"], "q1");
    // One frame shape for both modes: even a single pick rides in the list.
    assert_eq!(parsed["choices"], serde_json::json!(["c"]));
    assert!(parsed.get("choice").is_none());
    assert!(parsed.get("cancelled").is_none());
}

#[test]
fn dismiss_hides_question_without_sending_a_frame() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a"]));
    chat.dismiss_question();

    // Esc only hides the box locally: the question stays pending
    // server-side so the next typed message answers it via the
    // websocket inbound adapter's try_answer interception. No
    // question_response frame may be sent.
    assert!(!chat.active_question());
    assert!(chat.questions.is_empty());
    assert!(
        rx.try_recv().is_err(),
        "no frame expected: the question stays pending server-side"
    );
}

#[test]
fn selection_clamps_at_bounds() {
    let (mut chat, _rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));

    chat.select_question_prev(); // already at top
    assert_eq!(chat.current_question().unwrap().selected, 0);
    chat.select_question_next();
    chat.select_question_next(); // past bottom
    chat.select_question_next();
    assert_eq!(chat.current_question().unwrap().selected, 1);
}

#[test]
fn confirm_out_of_range_digit_is_noop() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));
    // Simulate the digit guard: idx 5 >= 2 options → no frame, question stays.
    let idx = 5_usize;
    if idx < chat.current_question().map_or(0, |q| q.options.len()) {
        chat.pick_question_idx(idx);
    }
    assert!(chat.active_question());
    assert!(rx.try_recv().is_err());
}

/// Render the topic-info pane for the selected topic into a plain string
/// (wide enough that no row wraps, so assertions can match whole rows).
fn info_pane_text(app: &mut App, width: u16, height: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_topic_info_pane(frame, frame.area(), app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The chat topic the two info-pane tests below select: a priced model (so the
/// `Cost:` row renders) and an empty-but-resolved file list (so `Files:` does),
/// which is what the task section is positioned between.
fn info_pane_app(tasks: jyc_types::task::TaskList) -> App {
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
            // Empty-but-resolved, so `Files:` renders — that is the section the
            // task list has to sit above.
            changed_files: Some(vec![]),
            context_input_tokens: None,
            total_input_tokens: None,
            total_cache_hit_tokens: None,
            total_cache_creation_tokens: None,
            max_tokens: None,
            output_tokens: None,
            last_active_at: None,
            skills: vec![],
            topic_path: None,
            cost: Some(jyc_types::TopicCost {
                session: 0.42,
                today: 1.23,
                currency: "USD".to_string(),
            }),
            commands: vec![],
            tasks,
        }],
        ..Default::default()
    });
    app.table_state.select(Some(0));
    app
}

/// The task list has to sit where it was asked for — below the cost row, above
/// the files section — and show the same markers and ids the agent's own tools
/// print, since those ids are what `task_update` takes.
#[test]
fn info_pane_shows_task_list_between_cost_and_files() {
    use jyc_types::task::{TaskItem, TaskList, TaskStatus};
    let mut app = info_pane_app(TaskList {
        items: vec![
            TaskItem {
                id: 1,
                text: "inspect list_topics".into(),
                status: TaskStatus::Completed,
            },
            TaskItem {
                id: 2,
                text: "wire the pane".into(),
                status: TaskStatus::InProgress,
            },
            TaskItem {
                id: 7,
                text: "ship it".into(),
                status: TaskStatus::Pending,
            },
        ],
    });
    let pane = info_pane_text(&mut app, 46, 24);

    let cost = pane.find("Cost:").expect("cost row");
    let tasks = pane.find("Tasks (1/3):").expect("tasks header");
    let files = pane.find("Files:").expect("files section");
    assert!(
        cost < tasks && tasks < files,
        "tasks must sit between cost and files:\n{pane}"
    );
    assert!(pane.contains("[x] 1. inspect list_topics"), "{pane}");
    assert!(pane.contains("[~] 2. wire the pane"), "{pane}");
    assert!(pane.contains("[ ] 7. ship it"), "{pane}");
}

#[test]
fn info_pane_omits_the_task_section_when_there_is_no_list() {
    let mut app = info_pane_app(jyc_types::task::TaskList::default());
    let pane = info_pane_text(&mut app, 46, 24);
    assert!(!pane.contains("Tasks"), "no list, no section:\n{pane}");
    // The neighbours still render, so the omission is local.
    assert!(pane.contains("Cost:"), "{pane}");
    assert!(pane.contains("Files:"), "{pane}");
}
// The progress tail is not part of the markdown-rendered history, and the
// transcript `Paragraph` deliberately does not wrap (one line is exactly one
// screen row, which is what the scroll maths count) — so a row wider than the
// pane used to run off the right edge, unreachable even by scrolling.

/// Draw the conversation with one seeded progress-tail entry and return the
/// screen rows plus `last_total_lines` — the row count the scroll, cursor and
/// yank maths address. `expanded` is the `ctrl+p T` tool-detail toggle,
/// `minimal` the `ctrl+p p` minimal progress mode.
///
/// A test that counts a filler character has to seed text with no other
/// instance of it, and count only within `rows[..total]`: that window is the
/// rows the tail claims to occupy.
fn tail_screen(text: &str, expanded: bool) -> (Vec<String>, usize) {
    tail_screen_mode(text, expanded, false)
}

fn tail_screen_mode(text: &str, expanded: bool, minimal: bool) -> (Vec<String>, usize) {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.channel = Some("github".into());
    app.chat.topic = Some("pr-1".into());
    // No server state, so the tail shows on the local optimistic flag.
    app.chat.awaiting_response = true;
    app.chat.tool_detail_expanded = expanded;
    app.chat.minimal_progress = minimal;
    app.chat.seed_live(
        "github",
        "pr-1",
        vec![jyc_types::ActivityEntry {
            text: text.into(),
            timestamp: None,
            severity: jyc_types::Severity::Info,
            id: 1,
            is_internal: false,
        }],
        vec![],
    );

    let (width, height) = (40, 20);
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| render_chat_conversation(frame, frame.area(), &mut app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows = (0..height).map(|y| super::row_text(&buffer, y)).collect();
    (rows, app.chat.last_total_lines)
}

/// The write/edit tool detail is the shape that prompted the report: one long
/// content line inside a diff.
#[test]
fn long_tool_detail_wraps_inside_the_progress_tail() {
    let long = "x".repeat(120);
    let (rows, total) = tail_screen(
        &format!(r#"{{"type":"write","file_path":"src/a.rs","content":"{long}"}}"#),
        true,
    );
    let pane = rows.join("\n");

    // Every column has to land on a row the scroll maths count — one number
    // that fails both when a row clips and when the count stops matching the
    // screen (which would send `j`/`k` and `y` past the entry).
    assert_eq!(
        rows[..total]
            .iter()
            .map(|r| r.matches('x').count())
            .sum::<usize>(),
        long.len(),
        "the detail must wrap into counted rows, not clip (total={total}):\n{pane}"
    );
    let body: Vec<&String> = rows.iter().filter(|r| r.contains('x')).collect();
    assert!(
        body.len() >= 3,
        "a 120-column line needs several rows in a 40-column pane:\n{pane}"
    );
    for row in &body[1..] {
        assert!(
            row.starts_with("     "),
            "wrapped rows align under the first one: {row:?}"
        );
    }
}

/// The spinner marks the entry, not every row of it — and a single-row entry
/// long enough to wrap is exactly where that distinction shows up.
#[test]
fn wrapped_first_row_keeps_one_spinner() {
    let long = "x".repeat(120);
    let (rows, total) = tail_screen(&format!("tool detail: {long}"), true);
    let pane = rows.join("\n");

    assert_eq!(
        rows[..total]
            .iter()
            .map(|r| r.matches('x').count())
            .sum::<usize>(),
        long.len(),
        "the whole row has to reach the screen (total={total}):\n{pane}"
    );
    // The marker is now the animated braille frame rather than a static
    // `⏳`, so match the column it occupies (two-column indent + marker)
    // instead of one specific glyph — whichever frame the clock lands on.
    let spinner: Vec<&String> = rows
        .iter()
        .filter(|r| {
            r.chars()
                .nth(2)
                .is_some_and(|c| matches!(c, '\u{2800}'..='\u{28ff}'))
        })
        .collect();
    assert_eq!(spinner.len(), 1, "one entry, one spinner row:\n{pane}");
    assert!(
        !rows[..total].iter().any(|r| r.contains('⏳')),
        "the static hourglass is gone from the tail:\n{pane}"
    );
    let body: Vec<&String> = rows.iter().filter(|r| r.contains('x')).collect();
    for row in &body[1..] {
        assert!(
            row.starts_with("     "),
            "continuation rows pad instead of repeating the marker: {row:?}"
        );
    }
}

#[test]
fn info_pane_end_reaches_the_last_wrapped_row() {
    // The bug this guards: the pane clamped its offset against *logical* lines
    // while ratatui wrapped them into more screen rows. This pane is 20% of the
    // terminal width, so a changed-file path takes two rows; with a list deep
    // enough, `End` (which stores `usize::MAX`) resolved to an offset that left
    // the final entries clipped and the bottom of the list unreachable.
    //
    // The fillers are deliberately wider than this pane's content (59 columns)
    // so each one costs a second row: the gap between logical lines and drawn
    // rows is then about N entries, not a rounding difference. That is what
    // makes the marker below a real regression check — under the old clamp the
    // entry carrying it is never drawn at all.
    let filler = "crates/jyc-cli/src/cli/dashboard/chat/tests/part2_scroll_probe.rs";
    let mut files: Vec<jyc_types::ChangedFileEntry> = (0..19)
        .map(|_| jyc_types::ChangedFileEntry {
            path: filler.to_string(),
            uncommitted: false,
            change: jyc_types::ChangeKind::Modified,
        })
        .collect();
    // Unique to the final entry, and narrow enough to stay on one row, so the
    // assertion means "the bottom row of the list is on screen".
    files.push(jyc_types::ChangedFileEntry {
        path: "crates/jyc-cli/src/cli/dashboard/chat/LASTENTRY_probe.rs".to_string(),
        uncommitted: false,
        change: jyc_types::ChangeKind::Added,
    });
    let mut topic = explorer_topic("t");
    topic.changed_files = Some(files);

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.phase = ChatPhase::Chatting;
    app.chat.info_visible = true;
    app.chat.topic = Some("t".to_string());
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![topic],
        ..Default::default()
    });

    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 14)).expect("terminal");
    // First pass measures the pane; the renderer clamps the offset it holds.
    terminal
        .draw(|f| render_topic_info_pane(f, f.area(), &mut app))
        .expect("draw");
    app.chat.info_scroll = usize::MAX; // what `End` / `G` store
    terminal
        .draw(|f| render_topic_info_pane(f, f.area(), &mut app))
        .expect("draw");

    let rendered: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        rendered.contains("LASTENTRY"),
        "`End` must reach the last wrapped row of the file list, got:\n{rendered}"
    );
}

/// Without the toggle the entry collapses to a short summary, and the summary
/// has to stay one row — wrapping is for what does not fit, not for everything.
#[test]
fn collapsed_tool_detail_stays_one_row() {
    let (rows, _total) = tail_screen(
        r#"{"type":"write","file_path":"src/a.rs","content":"aaaaaaaaaaaaaaaaaaaa"}"#,
        false,
    );
    let pane = rows.join("\n");
    assert!(
        pane.contains("a.rs"),
        "the collapsed line names the file:\n{pane}"
    );
    assert!(
        !pane.contains("aaaa"),
        "the content stays hidden until ctrl+p T:\n{pane}"
    );
    assert_eq!(
        rows.iter().filter(|r| r.contains("a.rs")).count(),
        1,
        "one summary, one row:\n{pane}"
    );
}

/// Minimal progress mode, end to end: the live round is one animated row and
/// nothing else — no diff block, no thinking row, no orphan gap.
#[test]
fn minimal_progress_mode_renders_one_animated_row() {
    let entry = r#"{"type":"edit","file_path":"src/a.rs","diff":"--- a\n+++ b\n+x"}"#;
    let (rows, total) = tail_screen_mode(entry, true, true);
    let pane = rows[..total].join("\n");

    assert_eq!(total, 1, "one row for the whole live round:\n{pane}");
    assert!(
        pane.contains("edit"),
        "the state word carries the tool name:\n{pane}"
    );
    assert!(
        pane.chars().any(|c| matches!(c, '\u{2800}'..='\u{28ff}')),
        "that row is animated, not a static marker:\n{pane}"
    );
    assert!(
        !rows
            .iter()
            .any(|r| r.contains('💭') || r.contains("a.rs") || r.contains('x')),
        "no thinking row and no tool detail reaches the screen:\n{}",
        rows.join("\n")
    );
}
