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
                name: format!("t{i}"),
                channel: "test".to_string(),
                agent: None,
                pattern: None,
                status: jyc_types::TopicStatus::Idle,
                context_input_tokens: None,
                context_output_tokens: None,
                max_tokens: None,
                last_active_at: None,
                topic_path: None,
                parent: None,
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
                name: format!("t{i}"),
                channel: "test".to_string(),
                agent: None,
                pattern: None,
                status: jyc_types::TopicStatus::Idle,
                context_input_tokens: None,
                context_output_tokens: None,
                max_tokens: None,
                last_active_at: None,
                topic_path: None,
                parent: None,
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
            name: "t0".to_string(),
            channel: "test".to_string(),
            agent: None,
            pattern: None,
            status: jyc_types::TopicStatus::Idle,
            context_input_tokens: None,
            context_output_tokens: None,
            max_tokens: None,
            last_active_at: None,
            topic_path: None,
            parent: None,
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
    app.chat.focus = Focus::ExplorerPane;
    app.chat.toggle_explorer();
    assert_eq!(app.chat.focus, Focus::ChatPane);
}

#[test]
fn focus_cycle_includes_explorer_only_when_visible() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.info_visible = false; // isolate explorer cycling

    // Hidden: ChatPane → MessageArea → ChatPane (no activity pane).
    app.chat.focus = Focus::ChatPane;
    app.chat.toggle_focus();
    assert_eq!(app.chat.focus, Focus::MessageArea);
    app.chat.toggle_focus();
    assert_eq!(app.chat.focus, Focus::ChatPane);

    // Opening the explorer jumps focus straight into it so j/k/Enter
    // are immediately usable; Tab then returns to the chat input.
    app.chat.toggle_explorer();
    assert_eq!(app.chat.focus, Focus::ExplorerPane);
    app.chat.toggle_focus();
    assert_eq!(app.chat.focus, Focus::ChatPane);
}

#[test]
fn opening_explorer_moves_focus_into_it() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.focus = Focus::ChatPane;
    app.chat.toggle_explorer();
    assert!(app.chat.explorer_visible);
    assert_eq!(app.chat.focus, Focus::ExplorerPane);
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
                agent: None,
                pattern: None,
                status: jyc_types::TopicStatus::Idle,
                context_input_tokens: None,
                context_output_tokens: None,
                max_tokens: None,
                last_active_at: None,
                topic_path: None,
                parent: None,
                commands: vec![],
            },
            jyc_types::TopicSummary {
                name: "other".to_string(),
                channel: "local_dev".to_string(),
                agent: None,
                pattern: None,
                status: jyc_types::TopicStatus::Idle,
                context_input_tokens: None,
                context_output_tokens: None,
                max_tokens: None,
                last_active_at: None,
                topic_path: None,
                parent: None,
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
    assert_eq!(app.chat.focus, Focus::ChatPane);
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
    app.chat.focus = Focus::InfoPane;
    app.chat.toggle_info_pane();
    assert!(!app.chat.info_visible);
    assert_eq!(app.chat.focus, Focus::ChatPane);
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

/// Opening a chat restores the chat screen's own defaults: the topic info
/// pane and the status block are there, the activity pane is not. Those
/// flags are the chat screen's only — the overview keeps all three panes
/// and offers no way to close them — so each chat starts from them rather
/// than from whatever the previous one left closed.
#[test]
fn opening_a_chat_restores_the_chat_screens_own_pane_defaults() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    // Close what `Ctrl+P` can close; the activity pane already starts closed.
    app.chat.toggle_info_pane();
    app.chat.toggle_status_bar();
    assert!(!app.chat.info_visible);
    assert!(!app.chat.status_visible);
    assert_eq!(app.chat.activity_split, 0);

    app.chat.reset_chat_state("topic-a");

    assert!(app.chat.info_visible);
    assert!(app.chat.status_visible);
    assert_eq!(app.chat.activity_split, 0);
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
    app.chat.focus = Focus::ChatPane;
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
    app.chat.focus = Focus::ChatPane;
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

/// The info pane reads the topic's *detail* — the same source `/info` reads —
/// and only when that detail is the topic on screen: a detail left over from
/// another topic must not be shown.
#[test]
fn info_pane_reads_the_shown_topics_detail() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.info_visible = true;
    app.chat.topic = Some("jyc".to_string());
    app.chat.channel = Some("local_dev".to_string());
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![jyc_types::TopicSummary {
            name: "jyc".to_string(),
            channel: "local_dev".to_string(),
            agent: None,
            pattern: None,
            status: jyc_types::TopicStatus::Idle,
            context_input_tokens: None,
            context_output_tokens: None,
            max_tokens: None,
            last_active_at: None,
            topic_path: None,
            parent: None,
            commands: vec![],
        }],
        ..Default::default()
    });

    // No detail fetched yet — nothing to show.
    assert!(
        selected_topic_detail(&app).is_none(),
        "without a detail the pane has no topic to render"
    );

    // A detail for another topic is not the topic on screen.
    app.topic_detail = Some(topic_detail_for("other", "local_dev"));
    assert!(
        selected_topic_detail(&app).is_none(),
        "another topic's detail must not be shown"
    );

    // The detail of the topic on screen is.
    app.topic_detail = Some(topic_detail_for("jyc", "local_dev"));
    assert_eq!(
        selected_topic_detail(&app).map(|d| d.name.as_str()),
        Some("jyc"),
        "the pane must render the topic on screen"
    );
}

/// A failed detail fetch must not look like a detail that never arrives.
///
/// The fetch clears the detail, so the pane cannot tell "still loading"
/// from "the request failed" by looking at the detail alone — and a hub
/// that stopped answering looks exactly like a request that never returns.
/// The failure is reported, and it is reported for its own topic only.
#[test]
fn info_pane_separates_a_failed_fetch_from_a_pending_one() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.info_visible = true;
    app.chat.topic = Some("jyc".to_string());
    app.chat.channel = Some("local_dev".to_string());
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![jyc_types::TopicSummary {
            name: "jyc".to_string(),
            channel: "local_dev".to_string(),
            agent: None,
            pattern: None,
            status: jyc_types::TopicStatus::Idle,
            context_input_tokens: None,
            context_output_tokens: None,
            max_tokens: None,
            last_active_at: None,
            topic_path: None,
            parent: None,
            commands: vec![],
        }],
        ..Default::default()
    });

    let render = |app: &mut App| -> String {
        let (width, height) = (60, 12);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_topic_info_pane(frame, frame.area(), &mut *app))
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
    };

    // The fetch for the topic on screen failed.
    app.topic_detail_failed = Some(("local_dev".to_string(), "jyc".to_string()));
    let pane = render(&mut app);
    assert!(
        pane.contains("Failed to load topic details"),
        "a failed fetch must not read as a pending one:\n{pane}"
    );

    // Another topic's failure says nothing about this one.
    app.topic_detail_failed = Some(("local_dev".to_string(), "other".to_string()));
    let pane = render(&mut app);
    assert!(
        pane.contains("Loading topic details"),
        "a pending fetch must still read as pending:\n{pane}"
    );
    assert!(
        !pane.contains("Failed to load topic details"),
        "another topic's failure must not be shown here:\n{pane}"
    );
}

/// A topic's detail, with nothing but its identity filled in.
fn topic_detail_for(name: &str, channel: &str) -> jyc_types::TopicInfo {
    jyc_types::TopicInfo {
        name: name.to_string(),
        channel: channel.to_string(),
        pattern: None,
        status: jyc_types::TopicStatus::Idle,
        queued: 0,
        queue_capacity: 0,
        parent: None,
        model: None,
        mode: None,
        context_input_tokens: None,
        context_output_tokens: None,
        max_tokens: None,
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
        activity: vec![],
        last_active_at: None,
        skills: vec![],
        available_skills: vec![],
        recent_messages: vec![],
        thinking_text: None,
        topic_path: None,
        branch: None,
        changed_files: None,
        tasks: Default::default(),
        cost: None,
        commands: vec![],
        subagents: None,
    }
}

/// Minimal idle topic for the explorer rendering tests.
fn explorer_topic(name: &str) -> jyc_types::TopicSummary {
    jyc_types::TopicSummary {
        name: name.to_string(),
        channel: "test".to_string(),
        agent: None,
        pattern: None,
        status: jyc_types::TopicStatus::Idle,
        context_input_tokens: None,
        context_output_tokens: None,
        max_tokens: None,
        last_active_at: None,
        topic_path: None,
        parent: None,
        commands: vec![],
    }
}

/// A node of a lineage tree, as a topic's detail carries it.
fn subagent_node(name: &str, children: Vec<jyc_types::TopicNode>) -> jyc_types::TopicNode {
    jyc_types::TopicNode {
        name: name.to_string(),
        channel: "test".to_string(),
        status: jyc_types::TopicStatus::Idle,
        children,
    }
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
    use jyc_types::LineStats;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.topic = Some("jyc".to_string());
    app.chat.channel = Some("local_dev".to_string());
    app.chat.info_visible = true;
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![jyc_types::TopicSummary {
            name: "jyc".to_string(),
            channel: "local_dev".to_string(),
            agent: None,
            pattern: Some("jyc".to_string()),
            status: jyc_types::TopicStatus::Idle,
            context_input_tokens: None,
            context_output_tokens: None,
            max_tokens: None,
            last_active_at: None,
            topic_path: None,
            parent: None,
            commands: vec![],
        }],
        ..Default::default()
    });
    // The file list comes from the topic's detail — the same source `/info`
    // reads.
    app.topic_detail = Some(jyc_types::TopicInfo {
        name: "jyc".to_string(),
        channel: "local_dev".to_string(),
        pattern: Some("jyc".to_string()),
        status: jyc_types::TopicStatus::Idle,
        queued: 0,
        queue_capacity: 0,
        parent: None,
        model: None,
        mode: None,
        context_input_tokens: None,
        context_output_tokens: None,
        max_tokens: None,
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
        activity: vec![],
        last_active_at: None,
        skills: vec![],
        available_skills: vec![],
        recent_messages: vec![],
        thinking_text: None,
        topic_path: None,
        branch: None,
        // One of each kind — sorted alphabetically on render.
        // added.rs       Added      clean    +7 -0
        // deleted.rs     Deleted    clean    (binary: no counts)
        // dirty_add.rs   Added      dirty (yellow) +2 -0
        // dirty_mod.rs   Modified   dirty (yellow) (no counts)
        // modified.rs    Modified   clean    (no counts)
        changed_files: Some(vec![
            ChangedFileEntry {
                path: "modified.rs".into(),
                uncommitted: false,
                change: ChangeKind::Modified,
                lines: None,
            },
            ChangedFileEntry {
                path: "added.rs".into(),
                uncommitted: false,
                change: ChangeKind::Added,
                lines: Some(LineStats {
                    insertions: 7,
                    deletions: 0,
                }),
            },
            ChangedFileEntry {
                path: "deleted.rs".into(),
                uncommitted: false,
                change: ChangeKind::Deleted,
                lines: None,
            },
            ChangedFileEntry {
                path: "dirty_add.rs".into(),
                uncommitted: true,
                change: ChangeKind::Added,
                lines: Some(LineStats {
                    insertions: 2,
                    deletions: 0,
                }),
            },
            ChangedFileEntry {
                path: "dirty_mod.rs".into(),
                uncommitted: true,
                change: ChangeKind::Modified,
                lines: None,
            },
        ]),
        tasks: Default::default(),
        cost: None,
        commands: vec![],
        subagents: None,
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

    // Counts ride at the end of a row when git reported them, and are
    // absent (rather than a made-up `+0 -0`) when it did not.
    let row_text = |needle: &str| -> Option<String> {
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            if row.contains(needle) {
                return Some(row);
            }
        }
        None
    };
    assert!(
        row_text("dirty_add.rs")
            .expect("dirty_add.rs row")
            .contains("+ dirty_add.rs +2 -0"),
        "a dirty Added row carries its counts before the commit"
    );
    assert!(
        row_text("added.rs")
            .expect("added.rs row")
            .contains("+ added.rs +7 -0"),
        "a clean Added row carries its counts"
    );
    assert!(
        !row_text("modified.rs")
            .expect("modified.rs row")
            .contains('-'),
        "a row git gave no counts for renders without them"
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
    app.chat.topic = Some("jyc".to_string());
    app.chat.channel = Some("local_dev".to_string());
    app.chat.info_visible = true;
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![jyc_types::TopicSummary {
            name: "jyc".to_string(),
            channel: "local_dev".to_string(),
            agent: None,
            pattern: Some("jyc".to_string()),
            status: jyc_types::TopicStatus::Idle,
            context_input_tokens: None,
            context_output_tokens: None,
            max_tokens: None,
            last_active_at: None,
            topic_path: None,
            parent: None,
            commands: vec![],
        }],
        ..Default::default()
    });
    // The file list comes from the topic's detail — the same source `/info`
    // reads.
    app.topic_detail = Some(jyc_types::TopicInfo {
        name: "jyc".to_string(),
        channel: "local_dev".to_string(),
        pattern: Some("jyc".to_string()),
        status: jyc_types::TopicStatus::Idle,
        queued: 0,
        queue_capacity: 0,
        parent: None,
        model: None,
        mode: None,
        context_input_tokens: None,
        context_output_tokens: None,
        max_tokens: None,
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
        activity: vec![],
        last_active_at: None,
        skills: vec![],
        available_skills: vec![],
        recent_messages: vec![],
        thinking_text: None,
        topic_path: None,
        branch: None,
        // Three files — fits easily in a tall pane.
        changed_files: Some(vec![
            ChangedFileEntry {
                path: "a.rs".into(),
                uncommitted: false,
                change: jyc_types::ChangeKind::Modified,
                lines: None,
            },
            ChangedFileEntry {
                path: "b.rs".into(),
                uncommitted: false,
                change: jyc_types::ChangeKind::Modified,
                lines: None,
            },
            ChangedFileEntry {
                path: "c.rs".into(),
                uncommitted: false,
                change: jyc_types::ChangeKind::Modified,
                lines: None,
            },
        ]),
        tasks: Default::default(),
        cost: None,
        commands: vec![],
        subagents: None,
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

fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[test]
fn status_line_is_gray_and_carries_no_rule() {
    let ctx = ctx_with_full_data();
    let line = build_status_line(80, &ctx);
    let text = line_text(&line);
    // The status line is a footer: the input field's rules live above it.
    assert!(
        !text.contains('─'),
        "status line must not draw rules: {text:?}"
    );
    // Both segments are the same dim gray, whatever the focus state.
    for span in &line.spans {
        assert_eq!(
            span.style.fg,
            Some(Color::Gray),
            "every span renders gray: {text:?}"
        );
    }
}

#[test]
fn status_line_includes_mode_topic_and_chip() {
    let ctx = ctx_with_full_data();
    let line = build_status_line(80, &ctx);
    let text = line_text(&line);
    // Left segment includes mode + topic.
    assert!(
        text.starts_with("plan · jyc"),
        "missing left segment in: {text:?}"
    );
    // Right chip includes model + context-window percentage. The
    // version lives in the info-pane status block, not the chat status.
    assert!(
        text.ends_with("[ claude-opus-4-6 · 10% ]"),
        "missing model/pct chip in: {text:?}"
    );
    assert!(
        !text.contains("jyc ai v"),
        "version belongs in the info-pane status block, not the chat status: {text:?}"
    );
    // Spaces between the two segments fill the row exactly.
    assert_eq!(text.width(), 80);
}

#[test]
fn status_line_omits_topic_when_missing() {
    let mut ctx = ctx_with_full_data();
    ctx.topic = None;
    let line = build_status_line(80, &ctx);
    let text = line_text(&line);
    assert!(
        text.starts_with("plan"),
        "missing mode segment in: {text:?}"
    );
    assert!(!text.contains("· jyc"));
}

#[test]
fn status_line_with_no_state_is_just_mode() {
    let ctx = ChatHeaderCtx {
        mode: "build",
        topic: None,
        branch: None,
        model: None,
        pct: None,
    };
    let line = build_status_line(80, &ctx);
    let text = line_text(&line);
    // Defaults: mode = "build", topic/branch all absent.
    assert_eq!(text, "build");
    // No fallback placeholders either — there is no chip anymore.
    assert!(
        !text.contains('[') && !text.contains('?'),
        "no question-mark placeholders expected: {text:?}"
    );
}

#[test]
fn status_line_truncates_left_when_too_narrow() {
    let mut ctx = ctx_with_full_data();
    ctx.topic = Some("a-very-long-topic-name");
    // Width so tight that even truncating topic barely fits.
    let line = build_status_line(20, &ctx);
    let text = line_text(&line);
    // Topic must be truncated to fit; no chip ever rendered.
    assert!(
        !text.contains('['),
        "should not contain a chip, got: {text:?}"
    );
    assert!(text.starts_with("plan"));
    assert!(text.width() <= 20);
    // Never leave a dangling separator at the end.
    assert!(
        !text.ends_with("· "),
        "should not end with separator: {text:?}"
    );
}

#[test]
fn status_line_appends_branch_when_present() {
    let mut ctx = ctx_with_full_data();
    ctx.branch = Some("feat/issue-512-show-branch");
    let line = build_status_line(120, &ctx);
    let text = line_text(&line);
    assert!(
        text.contains("· jyc · feat/issue-512-show-branch"),
        "branch segment should be appended after topic, got: {text:?}"
    );
}

#[test]
fn status_line_omits_branch_segment_when_none() {
    // Same ctx as `status_line_includes_mode_topic_and_chip`
    // but with branch=None — the left segment must end at "· jyc"
    // without a dangling separator.
    let ctx = ctx_with_full_data();
    let line = build_status_line(120, &ctx);
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
fn status_line_renders_partial_chip_with_model_only() {
    // pct missing (e.g., session hasn't recorded context yet) — the
    // chip should still render with just the model name.
    let mut ctx = ctx_with_full_data();
    ctx.pct = None;
    let line = build_status_line(80, &ctx);
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
fn status_line_drops_chip_when_narrow() {
    // Width that fits the left segment but not the chip — chip
    // should be dropped, left segment preserved.
    let ctx = ctx_with_full_data();
    // Left "plan · jyc" = 10 display cols.
    // Chip "[ claude-opus-4-6 · 10% ]" = 23 display cols.
    // total = 33 cols + 1 separating space. Width 32 forces dropping
    // the chip.
    let line = build_status_line(32, &ctx);
    let text = line_text(&line);
    assert!(
        !text.contains('['),
        "chip should be dropped when narrow, got: {text:?}"
    );
    assert_eq!(text, "plan · jyc");
}

#[test]
fn status_line_never_emits_dangling_separator() {
    // Narrow enough that the topic is truncated to a single column.
    let ctx = ChatHeaderCtx {
        mode: "plan",
        topic: Some("ch"),
        branch: None,
        model: None,
        pct: None,
    };
    let line = build_status_line(10, &ctx);
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

/// The mirror image for a typed answer: its end is what is being written, so a
/// long one loses its head instead of its tail.
#[test]
fn tail_to_width_keeps_the_end() {
    assert_eq!(tail_to_width("hi", 5), "hi");
    assert_eq!(tail_to_width("hello world", 6), "…world");
    assert_eq!(tail_to_width("abc", 1), "…");
    assert_eq!(tail_to_width("abc", 0), "");
    // Same width accounting as the head-first cut: a wide char that does not
    // fit beside the ellipsis is dropped, not covered up.
    let out = tail_to_width("世界你好", 5);
    assert_eq!(out, "…你好");
    assert_eq!(out.width(), 5);
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
fn code_fence_renders_without_highlight_colors() {
    // Pin: tui-markdown is built without `highlight-code`, so a code fence
    // renders as plain text — no Rgb foreground spans reach the TUI.
    let text = tui_markdown::from_str_with_options(
        "```rust\nfn main() {}\n```\n",
        &chat_markdown_options(),
    );
    let has_rgb_fg = text
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .any(|span| matches!(span.style.fg, Some(Color::Rgb(..))));
    assert!(!has_rgb_fg, "code fence still produced highlighted spans");
}
// Tests for the `ask_user` question box flow in the chat pane.
//
// The box renders what the daemon pushed and answers with one ordinary chat
// message - the turn that asked the questions has already ended, so there is no
// response protocol here, only text the model can read.

fn chat_for_topic(topic: &str) -> (ChatState, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut chat = ChatState::new(rx);
    chat.topic = Some(topic.to_string());
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    chat.ws_tx = Some(cmd_tx);
    (chat, cmd_rx)
}

/// The frame the daemon pushes for one question. `label` is the question text,
/// so a test can tell its questions apart by what comes back - the frame carries
/// no id, and the box keeps no state of its own beyond the topic.
fn question_payload(topic: &str, label: &str, options: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "question",
        "topic": topic,
        "question": format!("{label}?"),
        "options": options,
    })
}

/// The context the daemon sends with a question is what the box shows above
/// it; the frame is the only source, and a daemon that does not send the field
/// leaves the box exactly as it was.
#[test]
fn question_payload_carries_the_context() {
    let (mut chat, _rx) = chat_for_topic("jyc");
    let mut payload = question_payload("jyc", "q1", &["a", "b"]);
    payload["context"] = serde_json::json!("I compared both branches");
    chat.handle_question_event(&payload);
    assert_eq!(
        chat.current_question().unwrap().context.as_deref(),
        Some("I compared both branches")
    );

    let (mut chat, _rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q2", &["a"]));
    assert!(
        chat.current_question().unwrap().context.is_none(),
        "no context in the frame, none in the box"
    );
}

/// The same frame with `allow_multiple` set - what the daemon sends for a
/// question the user may answer with several options.
fn question_payload_multi(topic: &str, label: &str, options: &[&str]) -> serde_json::Value {
    let mut payload = question_payload(topic, label, options);
    payload["allow_multiple"] = serde_json::json!(true);
    payload
}

/// The one message the box sends on submit, as text. An answer is an ordinary
/// chat `message`, not a question protocol.
fn sent_text(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
    let frame: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(frame["type"], "message", "an answer is a chat message");
    frame["text"].as_str().unwrap().to_string()
}

/// Multi-select: `Space` marks under the cursor, digits mark too, and the
/// answer lists every mark in the option list's own order.
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
    assert_eq!(sent_text(&mut rx), "Q1: 3) c");
    assert!(!chat.active_question());
}

/// Not one marked is the user backing out of that question, not an empty answer.
#[test]
fn multi_select_enter_without_marks_says_skipped() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload_multi("jyc", "q1", &["a", "b"]));

    chat.confirm_question();

    assert_eq!(sent_text(&mut rx), "Q1: (skipped)");
    assert!(!chat.active_question());
}

/// `Space` is a new key: in single-select it confirms the highlighted option,
/// which is all it can do.
#[test]
fn single_select_space_confirms_the_highlighted_option() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));

    chat.select_question_next();
    chat.space_question(); // Space still confirms when only one may be picked

    assert_eq!(sent_text(&mut rx), "Q1: 2) b");
    assert!(!chat.active_question());
}

#[test]
fn question_event_surfaces_for_matching_topic() {
    let (mut chat, _rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));

    assert!(chat.active_question());
    let q = chat.current_question().expect("question stored");
    assert_eq!(q.question, "q1?");
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

/// A second question joins the queue rather than replacing the first: one
/// `ask_user` call asking several questions must not drop one from the answer.
#[test]
fn second_question_queues_instead_of_replacing_the_first() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a"]));
    chat.handle_question_event(&question_payload("jyc", "q2", &["b"]));

    assert!(
        rx.try_recv().is_err(),
        "queueing a question must not send an answer for the previous one"
    );
    assert_eq!(
        chat.current_question()
            .expect("first stays on screen")
            .question,
        "q1?"
    );
    chat.step_question(1);
    assert_eq!(
        chat.current_question()
            .expect("second is reachable")
            .question,
        "q2?"
    );
}

/// A queued batch belongs to the topic it was asked in, so switching topics
/// drops it the way Esc does. Keeping it would hide the next topic's question
/// for good: `current_question` reads `questions[question_index]`, and a first
/// entry from the old topic leaves the new one off screen.
#[test]
fn switching_topics_leaves_the_old_question_batch_behind() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "qA1", &["x"]));
    chat.handle_question_event(&question_payload("jyc", "qA2", &["y"]));
    assert_eq!(chat.questions.len(), 2, "the qA batch is queued");

    chat.reset_chat_state("other");
    assert!(
        rx.try_recv().is_err(),
        "dropping a batch that was never answered sends nothing"
    );
    chat.handle_question_event(&question_payload("other", "qB1", &["z"]));

    assert!(
        chat.active_question(),
        "the new topic's question must reach the screen"
    );
    assert_eq!(chat.questions.len(), 1, "the old batch is gone");
    assert_eq!(
        chat.current_question().expect("qB1 on screen").question,
        "qB1?"
    );
    assert_eq!(chat.question_index, 0);
}

/// The batch is the point: an intermediate Enter sends nothing, and the last
/// one flushes every answer as one message, one line per question.
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

    assert_eq!(sent_text(&mut rx), "Q1: 2) b\nQ2: 1) c");
    assert!(rx.try_recv().is_err(), "one message for the whole set");
    assert!(!chat.active_question());
}

/// `d` skips the question on screen and the rest of the set still goes out with
/// its picks. Without it the only "no" was Esc, which cost every pick already
/// made.
#[test]
fn skipping_one_question_keeps_the_other_answer() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));
    chat.handle_question_event(&question_payload("jyc", "q2", &["c", "d"]));

    chat.toggle_question_skip(); // q1: no answer
    chat.confirm_question(); // -> q2
    chat.select_question_next(); // q2 -> d
    chat.confirm_question(); // flush

    assert_eq!(sent_text(&mut rx), "Q1: (skipped)\nQ2: 2) d");
}

/// The list ends with the user's own answer: an option set can only offer what
/// the model thought of, and a decision point sometimes needs words none of the
/// options hold. Enter on that row types one, and it answers for its question on
/// its own line - no option number, so the model can tell it from a pick.
#[test]
fn a_typed_answer_answers_for_its_question() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));

    chat.select_question_next(); // -> b
    chat.select_question_next(); // -> the ✎ row, the list's last one
    chat.select_question_next(); // clamped there: it is the end
    assert_eq!(
        chat.current_question().expect("q1").selected,
        2,
        "the cursor must reach the ✎ row and stop"
    );
    chat.confirm_question(); // start typing
    for c in "it's fine".chars() {
        chat.push_typed_char(c);
    }
    chat.confirm_question(); // keep it and send the batch

    assert_eq!(sent_text(&mut rx), "Q1: it's fine");
}

/// Esc in the draft hands the box back to the list, draft and all — so the
/// options still answer, and nothing half-typed rides out with the batch.
#[test]
fn escaping_the_draft_lets_the_options_answer_again() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));

    chat.select_question_next();
    chat.select_question_next();
    chat.confirm_question();
    chat.push_typed_char('x');
    chat.cancel_typed_answer();
    assert!(!chat.question_typing, "Esc must close the draft");
    assert_eq!(
        chat.current_question().expect("q1").typed.as_deref(),
        Some("x"),
        "the draft is kept, so coming back to it does not mean retyping"
    );
    chat.select_question_prev(); // back to an option
    chat.select_question_prev(); // and to the first one
    chat.confirm_question(); // which settles the question

    assert_eq!(sent_text(&mut rx), "Q1: 1) a");
}

/// A typed answer belongs to its question: stepping to the next one and back
/// finds it again, ready to be edited rather than retyped.
#[test]
fn a_typed_answer_waits_for_its_own_question() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));
    chat.handle_question_event(&question_payload("jyc", "q2", &["c", "d"]));

    chat.select_question_next();
    chat.select_question_next();
    chat.confirm_question();
    for c in "mine".chars() {
        chat.push_typed_char(c);
    }
    chat.confirm_question(); // q1 answered, on to q2
    assert!(
        !chat.question_typing,
        "the draft belongs to the question left behind"
    );
    chat.step_question(-1); // back to q1

    let q1 = chat.current_question().expect("q1 again");
    assert_eq!(q1.typed.as_deref(), Some("mine"));
    assert_eq!(q1.selected, 2, "the cursor comes back to the ✎ row");
    chat.confirm_question(); // reopen the draft, not a second answer
    for c in " now".chars() {
        chat.push_typed_char(c);
    }
    chat.confirm_question(); // q1 again, on to q2
    chat.confirm_question(); // q2's first option

    assert_eq!(sent_text(&mut rx), "Q1: mine now\nQ2: 1) c");
}

/// Space on the `✎` row opens the draft in a multi-select question too. The row
/// is not an option, so marking it would put a phantom index in the answer -
/// invisible on screen, dropped at submit - and would wipe what was typed.
#[test]
fn space_on_the_typed_row_opens_the_draft_in_multi() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload_multi("jyc", "q1", &["a", "b"]));

    chat.space_question(); // mark a
    chat.select_question_next();
    chat.select_question_next(); // -> the ✎ row
    chat.space_question(); // opens the draft instead of marking index 2
    assert!(chat.question_typing, "Space on the ✎ row opens the draft");
    assert_eq!(
        chat.current_question().expect("q1").marked,
        vec![0],
        "the only mark is the option the user marked"
    );
    for c in "both".chars() {
        chat.push_typed_char(c);
    }
    chat.confirm_question(); // the typed answer answers for it

    assert_eq!(sent_text(&mut rx), "Q1: both");
}

/// A popup or a leader session left over from before the question frame landed
/// is closed with it. Either one would otherwise take the box's Enter - the
/// popup's fires the highlighted slash command instead of keeping the answer -
/// and its Esc, so the question could not be answered at all.
#[test]
fn a_question_frame_closes_the_popup_and_the_leader() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.command_popup = Some(CommandPopupState::new());
    chat.leader = Some(leader::Leader::new(local_commands::CommandScope::Chat));

    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));

    assert!(chat.command_popup.is_none(), "the box owns the keyboard");
    assert!(chat.leader.is_none(), "and so does the leader session");

    // Which is what Enter then proves: it answers rather than firing a command.
    chat.confirm_question();
    assert_eq!(sent_text(&mut rx), "Q1: 1) a");
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
    assert_eq!(current.question, "q1?");
    assert_eq!(current.marked, vec![0], "the first mark survived");
    chat.select_question_next(); // cursor -> b
    chat.space_question(); // mark b too
    chat.step_question(1); // -> q2
    chat.confirm_question(); // flush

    assert_eq!(sent_text(&mut rx), "Q1: 1) a, 2) b\nQ2: 1) c");
}

/// Enter on the last question sends the answer as an ordinary chat message -
/// numbered by question and by option, so the model needs nothing else to read
/// it.
#[test]
fn confirm_sends_one_ordinary_message() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b", "c"]));
    chat.select_question_next();
    chat.select_question_next();
    chat.confirm_question();

    assert!(!chat.active_question());
    assert_eq!(sent_text(&mut rx), "Q1: 3) c");
    assert!(rx.try_recv().is_err(), "one message, no extra frames");
}

/// Esc closes the box and sends nothing: the turn that asked the questions
/// already ended, so there is nothing to abort, and the user may still type an
/// answer or anything else afterwards.
#[test]
fn esc_closes_the_box_without_sending() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a"]));
    chat.handle_question_event(&question_payload("jyc", "q2", &["b", "c"]));
    assert_eq!(chat.questions.len(), 2);

    chat.close_questions();

    assert!(!chat.active_question());
    assert!(chat.questions.is_empty());
    assert!(
        rx.try_recv().is_err(),
        "closing the box answers nothing and aborts nothing"
    );
}

/// Closing the pane drops the batch: the pane that draws it is going away, and
/// a half picked set must not come back as the answer to a later question.
#[test]
fn closing_the_pane_drops_the_pending_batch() {
    let (mut chat, mut rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a"]));

    chat.close();

    let frame: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(
        frame["type"], "disconnect",
        "the only thing a close sends is the socket goodbye"
    );
    assert!(chat.ws_tx.is_none(), "the socket is handed back after");

    // The batch is already gone, so reopening sends nothing.
    chat.reset_chat_state("jyc");
    assert!(
        rx.try_recv().is_err(),
        "a reopen must not answer questions it never showed"
    );
}

#[test]
fn selection_clamps_at_bounds() {
    let (mut chat, _rx) = chat_for_topic("jyc");
    chat.handle_question_event(&question_payload("jyc", "q1", &["a", "b"]));

    chat.select_question_prev(); // already at top
    assert_eq!(chat.current_question().unwrap().selected, 0);
    chat.select_question_next();
    chat.select_question_next(); // -> the ✎ row
    chat.select_question_next(); // past bottom
    chat.select_question_next();
    assert_eq!(
        chat.current_question().unwrap().selected,
        2,
        "the cursor stops on the `✎` row, the list's last one"
    );
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
    app.chat.topic = Some("jyc".to_string());
    app.chat.channel = Some("local_dev".to_string());
    app.chat.info_visible = true;
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![jyc_types::TopicSummary {
            name: "jyc".to_string(),
            channel: "local_dev".to_string(),
            agent: None,
            pattern: Some("jyc".to_string()),
            status: jyc_types::TopicStatus::Idle,
            context_input_tokens: None,
            context_output_tokens: None,
            max_tokens: None,
            last_active_at: None,
            topic_path: None,
            parent: None,
            commands: vec![],
        }],
        ..Default::default()
    });
    // The topic's own figures — the cost row, the task list, the files
    // section — come from the detail, the same source `/info` reads.
    app.topic_detail = Some(jyc_types::TopicInfo {
        name: "jyc".to_string(),
        channel: "local_dev".to_string(),
        pattern: Some("jyc".to_string()),
        status: jyc_types::TopicStatus::Idle,
        queued: 0,
        queue_capacity: 0,
        parent: None,
        model: None,
        mode: None,
        context_input_tokens: None,
        context_output_tokens: None,
        max_tokens: None,
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
        activity: vec![],
        last_active_at: None,
        skills: vec![],
        available_skills: vec![],
        recent_messages: vec![],
        thinking_text: None,
        topic_path: None,
        branch: None,
        // Empty-but-resolved, so `Files:` renders — that is the section the
        // task list has to sit above.
        changed_files: Some(vec![]),
        cost: Some(jyc_types::TopicCost {
            session: 0.42,
            today: 1.23,
            currency: "USD".to_string(),
        }),
        commands: vec![],
        subagents: None,
        tasks,
    });
    app.table_state.select(Some(0));
    app
}

/// The info pane draws the last call's context composition as one stacked
/// bar behind the section's `Context:` label, and a legend row per region
/// under it, indented to the column the bar starts in. The bar's segments
/// carry each region's colour, and the rows carry the same estimated size
/// and share `/info` prints — estimated tokens only, no wire bytes.
#[test]
fn info_pane_draws_context_composition_bar_and_legend() {
    use jyc_types::{ContextRegion, ContextRegionInfo};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = info_pane_app(jyc_types::task::TaskList::default());
    app.topic_detail
        .as_mut()
        .expect("the detail")
        .context_composition = vec![
        ContextRegionInfo {
            region: ContextRegion::History,
            messages: 31,
            bytes: 132_000,
            estimated_tokens: 18_000,
        },
        ContextRegionInfo {
            region: ContextRegion::Checkpoint,
            messages: 17,
            bytes: 41_000,
            estimated_tokens: 4_000,
        },
        ContextRegionInfo {
            region: ContextRegion::Current,
            messages: 9,
            bytes: 58_000,
            estimated_tokens: 8_000,
        },
    ];

    let width = 60;
    let height = 40;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_topic_info_pane(frame, frame.area(), &mut app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let screen = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    // The bar shares the title's row: the label introduces it, the cells
    // follow it. The section is titled `Context:` — nothing more.
    let title_row = screen
        .lines()
        .find(|line| line.contains("Context:"))
        .expect("the composition title");
    assert!(title_row.contains('▊'), "{screen}");
    assert!(!screen.contains("(estimated)"), "{screen}");
    // The legend rows are indented to the bar's first cell and share one
    // column of figures: no colon after a label, no byte count anywhere.
    assert!(
        screen.contains("         History      ~17.58K · 60%"),
        "{screen}"
    );
    assert!(
        screen.contains("         Checkpoint   ~3.91K · 13%"),
        "{screen}"
    );
    assert!(
        screen.contains("         Current turn ~7.81K · 27%"),
        "{screen}"
    );
    assert!(!screen.contains("messages"), "{screen}");

    let bar_colors: Vec<Color> = (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .filter_map(|(x, y)| {
            let cell = &buffer[(x, y)];
            (cell.symbol() == "▊").then_some(cell.fg)
        })
        .collect();
    assert!(bar_colors.contains(&Color::Green), "{screen}");
    assert!(bar_colors.contains(&Color::Yellow), "{screen}");
    assert!(bar_colors.contains(&Color::Cyan), "{screen}");
}

/// Render the info pane at `width` × `height` with `composition` on the
/// detail, and return it as one string with a line per terminal row.
fn info_pane_screen_with_composition(
    composition: Vec<jyc_types::ContextRegionInfo>,
    width: u16,
    height: u16,
) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = info_pane_app(jyc_types::task::TaskList::default());
    app.topic_detail
        .as_mut()
        .expect("the detail")
        .context_composition = composition;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_topic_info_pane(frame, frame.area(), &mut app))
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

/// A pane too narrow for the bar keeps the section header and drops only
/// the bar: below `MIN_BAR_WIDTH` a cell would carry too much of the share
/// to be honest, so the bar is refused rather than drawn. The header row
/// survives with nothing behind it — the bar helper's `false` means "no
/// bar on this row", not "no row at all".
#[test]
fn info_pane_keeps_the_context_header_when_the_bar_is_refused() {
    use jyc_types::{ContextRegion, ContextRegionInfo};

    // 15 columns less the pane's left border leaves 14: `Context: ` takes
    // 9, leaving 5, and `BAR_EDGE_ROOM` takes 2 of those — 3 cells, under
    // `MIN_BAR_WIDTH` (5).
    let screen = info_pane_screen_with_composition(
        vec![
            ContextRegionInfo {
                region: ContextRegion::History,
                messages: 3,
                bytes: 1_000,
                estimated_tokens: 600,
            },
            ContextRegionInfo {
                region: ContextRegion::Current,
                messages: 1,
                bytes: 200,
                estimated_tokens: 400,
            },
        ],
        15,
        24,
    );

    // The bar shares the header row, so a header with nothing behind it is
    // exactly a refused bar; a bar drawn here would follow the label. The
    // pane's left border leads every row, so it comes off before comparing.
    let header = screen
        .lines()
        .find(|line| line.contains("Context:"))
        .expect("the composition header");
    let header = header
        .trim_matches(|c: char| c == '│' || c.is_whitespace())
        .to_string();
    assert_eq!(header, "Context:", "{screen}");
}

/// A composition whose estimates are all zero has no weight to split: the
/// bar is refused and the legend reads `~0 · 0%` rather than dividing by
/// zero — the same rows, one bar short.
#[test]
fn info_pane_shows_a_zero_weight_composition_without_a_bar() {
    use jyc_types::{ContextRegion, ContextRegionInfo};

    let screen = info_pane_screen_with_composition(
        vec![ContextRegionInfo {
            region: ContextRegion::History,
            messages: 3,
            bytes: 1_000,
            estimated_tokens: 0,
        }],
        60,
        24,
    );

    let header = screen
        .lines()
        .find(|line| line.contains("Context:"))
        .expect("the composition header");
    let header = header
        .trim_matches(|c: char| c == '│' || c.is_whitespace())
        .to_string();
    assert_eq!(header, "Context:", "{screen}");
    // One region sets the label column to its own width (`History`, 7), not
    // to the widest of all four — see `composition_label_column_is_the_widest`.
    assert!(screen.contains("         History ~0 · 0%"), "{screen}");
}

/// The status row carries the topic's backlog: the blocks say how much of
/// the queue is taken — the queued ones yellow, the free slots gray — and
/// the word `queue` says what they count, so they cannot be read as the
/// progress of the turn the status names. The figures share the row.
#[test]
fn info_pane_draws_the_queue_on_the_status_row() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut app = info_pane_app(jyc_types::task::TaskList::default());
    let detail = app.topic_detail.as_mut().expect("the detail");
    detail.queued = 2;
    detail.queue_capacity = 10;

    let width = 40;
    let height = 24;
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_topic_info_pane(frame, frame.area(), &mut app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();

    // The row that carries the blocks.
    let row = (0..height)
        .find(|y| (0..width).any(|x| buffer[(x, *y)].symbol() == "▊"))
        .expect("the status row");
    let blocks: Vec<(u16, Color)> = (0..width)
        .filter_map(|x| {
            let cell = &buffer[(x, row)];
            (cell.symbol() == "▊").then_some((x, cell.fg))
        })
        .collect();
    assert_eq!(blocks.len(), 6, "the blocks fill the room the row has");
    let queued = blocks.iter().filter(|(_, fg)| *fg == Color::Yellow).count();
    let free = blocks
        .iter()
        .filter(|(_, fg)| *fg == Color::DarkGray)
        .count();
    assert_eq!(queued, 1, "the two messages waiting are yellow");
    assert_eq!(free, 5, "the free slots are gray");
    // The queued blocks come first: the row reads as a queue filling up.
    let first_gray = blocks
        .iter()
        .position(|(_, fg)| *fg == Color::DarkGray)
        .expect("the free slots");
    assert_eq!(first_gray, 1, "the yellow blocks lead the row");

    // The figures and the word share the row with the blocks.
    let line: String = (0..width)
        .map(|x| buffer[(x, row)].symbol().to_string())
        .collect();
    assert!(line.contains("2 / 10"), "{line}");
    assert!(line.contains("queue"), "{line}");
}

/// A topic whose source reports no capacity has no queue to draw: the
/// status row is the status word alone.
#[test]
fn info_pane_omits_the_queue_without_a_capacity() {
    let mut app = info_pane_app(jyc_types::task::TaskList::default());
    let detail = app.topic_detail.as_mut().expect("the detail");
    detail.queued = 0;
    detail.queue_capacity = 0;
    let pane = info_pane_text(&mut app, 40, 24);
    assert!(!pane.contains("queue"), "{pane}");
    assert!(!pane.contains('▊'), "{pane}");
    assert!(pane.contains("Status:"), "{pane}");
}

/// A pane too narrow for the blocks puts the figures under the status
/// word: the count still reads, and the row keeps its place.
#[test]
fn info_pane_puts_the_queue_figures_under_the_status_in_a_narrow_pane() {
    let mut app = info_pane_app(jyc_types::task::TaskList::default());
    let detail = app.topic_detail.as_mut().expect("the detail");
    detail.queued = 2;
    detail.queue_capacity = 10;
    // Six cells of pane: too narrow for a block of five.
    let pane = info_pane_text(&mut app, 7, 24);
    assert!(pane.contains("2 / 10"), "{pane}");
    assert!(
        !pane.contains('▊'),
        "a narrow pane draws no blocks:\n{pane}"
    );
    assert!(
        !pane.contains("queue"),
        "a narrow pane drops the word with the blocks:\n{pane}"
    );
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
    assert!(pane.contains("[✓] 1. inspect list_topics"), "{pane}");
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

/// App with server state, ready for the chat status-block tests: version,
/// stats, and an uptime the Coarse duration style renders as "38m".
fn status_block_app() -> App {
    let mut app = info_pane_app(jyc_types::task::TaskList::default());
    let state = app.state.as_mut().expect("state");
    state.version = "9.9.9-test".to_string();
    state.stats.active_workers = 2;
    state.stats.total_topics = 5;
    state.stats.messages_received = 12;
    state.stats.errors = 0;
    state.uptime_secs = 2280;
    app
}

#[test]
fn info_pane_status_block_sits_below_topic_info() {
    let mut app = status_block_app();
    let pane = info_pane_text(&mut app, 60, 24);
    // The block gets its own sub-pane pinned to the bottom of the info
    // column, below the topic info, separated by a gray top border.
    let topic = pane.find(&info_pane_topic_row()).expect("topic line");
    let version = pane.find("JYC AI v9.9.9-test").expect("version line");
    assert!(
        topic < version,
        "status block must sit below the topic info:\n{pane}"
    );
}

/// The info column is one bordered pane: the left border drawn by the outer
/// block must run for the full column height and join the status sub-pane's
/// top border with a `├` junction. Regression test for the split-before-block
/// ordering that left the border column holding the sub-pane's `─` border.
#[test]
fn info_pane_left_border_runs_the_full_column() {
    let mut app = status_block_app();
    let pane = info_pane_text(&mut app, 60, 24);
    let rows: Vec<&str> = pane.lines().collect();
    // The status sub-pane's top border is the row directly above the version
    // line; the pane's left border column joins it with a `├` junction. The
    // helper renders the pane over the whole frame, so the border column is
    // wherever the junction sits, not the real UI's 20%-from-right offset.
    let version_row = rows
        .iter()
        .position(|r| r.contains("JYC AI"))
        .expect("version line");
    let junction_row = version_row - 1;
    let border_x = rows[junction_row]
        .find('├')
        .expect("junction between the left border and the sub-pane border");
    for (y, row) in rows.iter().enumerate() {
        let expected = if y == junction_row { '├' } else { '│' };
        assert_eq!(
            row.chars().nth(border_x),
            Some(expected),
            "row {y}: expected {expected:?} at x={border_x}:\n{pane}"
        );
    }
}

#[test]
fn info_pane_status_block_stays_when_topic_info_scrolls() {
    use jyc_types::ChangeKind;
    use jyc_types::ChangedFileEntry;
    let mut app = status_block_app();
    // Overflow the topic info so `End`/`G`-style scrolling has somewhere
    // to go: ~30 changed files in a 24-row pane.
    if let Some(detail) = app.topic_detail.as_mut() {
        detail.changed_files = Some(
            (0..30)
                .map(|i| ChangedFileEntry {
                    path: format!("file_{i:02}.rs"),
                    uncommitted: false,
                    change: ChangeKind::Modified,
                    lines: None,
                })
                .collect(),
        );
    }
    app.chat.info_scroll = usize::MAX; // what `End` / `G` store
    let pane = info_pane_text(&mut app, 60, 24);
    assert!(
        pane.contains("JYC AI v9.9.9-test"),
        "scrolling the topic info must not move the status block:\n{pane}"
    );
    assert!(
        pane.contains("2 active / 5 topic · 12 recv · 0 err · up 38m"),
        "stats line must stay pinned with the version line:\n{pane}"
    );
}

#[test]
fn info_pane_transient_message_replaces_status_block() {
    let mut app = status_block_app();
    app.set_status("Copied 5 rows".to_string());
    let pane = info_pane_text(&mut app, 60, 24);
    assert!(pane.contains("Copied 5 rows"), "{pane}");
    assert!(
        !pane.contains("JYC AI v"),
        "transient message swaps the whole block out:\n{pane}"
    );
}

#[test]
fn info_pane_hides_status_block_when_status_toggled() {
    let mut app = status_block_app();
    app.chat.toggle_status_bar();
    let pane = info_pane_text(&mut app, 60, 24);
    assert!(!pane.contains("JYC AI v"), "{pane}");
    assert!(
        pane.contains(&info_pane_topic_row()),
        "only the status block is hidden, topic info still renders:\n{pane}"
    );
}

/// A narrow info column wraps the stats across rows instead of cutting a chip
/// mid-word: at 19 usable cells the old single row showed `2 active / 5 t`, now
/// the five chips spread over three whole-chip rows — and the `active / topic`
/// pair keeps its slash while the two share a row.
#[test]
fn status_block_wraps_chips_instead_of_clipping_them() {
    let mut app = status_block_app();
    let pane = info_pane_text(&mut app, 20, 24);
    let rows: Vec<&str> = pane.split('\n').collect();
    // The greedy rows themselves, not just "the text appears somewhere": a
    // chip that had to move down must sit on a row of its own.
    for row in ["2 active / 5 topic", "12 recv · 0 err", "up 38m"] {
        assert!(
            rows.iter().any(|r| r.contains(row)),
            "expected a stats row {row:?}:\n{pane}"
        );
    }
}

/// The floor is all-or-nothing at its boundary: a column one row short of the
/// block's border plus every wrapped row (five here) drops the whole block
/// instead of showing the version line with the stats cut off, and the topic
/// info keeps the six rows it is owed. Heights count the *pane*, whose inner
/// region loses a row to its own top padding — the boundary is at 12, not 11.
#[test]
fn status_block_hides_rather_than_clipping_its_own_rows() {
    let mut app = status_block_app();
    // 12 rows: inner 11 = five for the sub-pane plus the six the topic info keeps.
    let pane = info_pane_text(&mut app, 20, 12);
    assert!(
        pane.contains("up 38m"),
        "the last wrapped row must show when the block fits:\n{pane}"
    );
    // 11 rows: inner 10, one short of what the block needs, so none of it renders.
    let pane = info_pane_text(&mut app, 20, 11);
    assert!(
        !pane.contains("JYC AI") && !pane.contains("2 active"),
        "a column too short for the whole block must show none of it:\n{pane}"
    );
    // A column this narrow stacks the pane's rows: the label takes a row
    // of its own and the value follows under it.
    let rows = info_pane_rows(&pane);
    let topic = rows
        .iter()
        .position(|r| r == "Topic:")
        .expect("the topic's label row");
    assert_eq!(
        rows[topic + 1],
        "jyc",
        "the topic info keeps the column:\n{pane}"
    );
}

/// `pack_chips` is greedy and atomic: chips join the current row only while they
/// fit, a chip too wide to join takes a row alone (which also closes that row to
/// the chips after it), and a chip that starts a row drops its joining separator.
#[test]
fn pack_chips_is_greedy_and_atomic() {
    use crate::cli::dashboard::pack_chips;
    let chips = vec![
        ("", "aa".to_string()),
        (" · ", "bbbbbbbbbb".to_string()),
        (" · ", "cc".to_string()),
    ];
    // 5 cells: nothing joins, every chip stands alone.
    assert_eq!(
        pack_chips(&chips, 5),
        vec!["aa".to_string(), "bbbbbbbbbb".to_string(), "cc".to_string()]
    );
    // 15 cells: `aa · bbbbbbbbbb` fits exactly, `cc` cannot follow it.
    assert_eq!(
        pack_chips(&chips, 15),
        vec!["aa · bbbbbbbbbb".to_string(), "cc".to_string()]
    );
    // A chip pushed onto a new row does not lead that row with its separator.
    let pair = vec![("", "2 active".to_string()), (" / ", "5 topic".to_string())];
    assert_eq!(
        pack_chips(&pair, 6),
        vec!["2 active".to_string(), "5 topic".to_string()]
    );
    assert_eq!(pack_chips(&[], 12), Vec::<String>::new());
}

#[test]
fn chat_screen_renders_no_bottom_status_bar() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = status_block_app();
    // 120 cols so the 20% info pane (24 wide) fits the version line
    // without wrapping — the assertion needs it on one row.
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| ui_chat_mode(frame, frame.area(), &mut app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let text = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !text.contains("[^P]leader"),
        "chat screen must not render the bottom status bar:\n{text}"
    );
    assert!(
        text.contains("JYC AI v9.9.9-test"),
        "the info pane carries the status block instead:\n{text}"
    );
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
            event: serde_json::Value::Null,
            text: Some(text.into()),
            timestamp: None,
            id: 1,
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
            lines: None,
        })
        .collect();
    // Unique to the final entry, and narrow enough to stay on one row, so the
    // assertion means "the bottom row of the list is on screen".
    files.push(jyc_types::ChangedFileEntry {
        path: "crates/jyc-cli/src/cli/dashboard/chat/LASTENTRY_probe.rs".to_string(),
        uncommitted: false,
        change: jyc_types::ChangeKind::Added,
        lines: None,
    });
    let topic = explorer_topic("t");

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.info_visible = true;
    app.chat.topic = Some("t".to_string());
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![topic],
        ..Default::default()
    });
    // The file list comes from the topic's detail — the same source `/info`
    // reads.
    app.topic_detail = Some(jyc_types::TopicInfo {
        name: "t".to_string(),
        channel: "test".to_string(),
        pattern: None,
        status: jyc_types::TopicStatus::Idle,
        queued: 0,
        queue_capacity: 0,
        parent: None,
        model: None,
        mode: None,
        context_input_tokens: None,
        context_output_tokens: None,
        max_tokens: None,
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
        activity: vec![],
        last_active_at: None,
        skills: vec![],
        available_skills: vec![],
        recent_messages: vec![],
        thinking_text: None,
        topic_path: None,
        branch: None,
        changed_files: Some(files),
        tasks: Default::default(),
        cost: None,
        commands: vec![],
        subagents: None,
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

/// The chat screen's pane answers for the chat's own topic. When that topic has
/// not appeared in the poll yet, it says so rather than falling back to the row
/// the dashboard last had under the cursor — that row's model, mode and branch
/// belong to another topic (`render_activity_log` refuses the same fallback).
#[test]
fn info_pane_does_not_fall_back_to_the_cursor_row() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.info_visible = true;
    app.chat.topic = Some("not-polled-yet".to_string());
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![explorer_topic("other")],
        ..Default::default()
    });
    app.table_state.select(Some(0));

    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 14)).expect("terminal");
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
        rendered.contains("Select a topic"),
        "a chat whose topic is not in the poll must show no topic, got:\n{rendered}"
    );
    assert!(
        !rendered.contains("other"),
        "the pane must not fall back to the cursor row's topic, got:\n{rendered}"
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

// ── Pane border palette ──────────────────────────────────────────────────────

use super::super::super::{PANE_BORDER, PANE_BORDER_FOCUSED};
use ratatui::Frame;
use ratatui::layout::Rect;

/// The fg of the buffer cell at `(x, y)` after drawing `draw` into a fresh test
/// terminal of the given size.
fn border_fg_at(
    width: u16,
    height: u16,
    x: u16,
    y: u16,
    mut draw: impl FnMut(&mut Frame, Rect),
) -> Color {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw(frame, area);
        })
        .expect("draw");
    terminal.backend().buffer().clone()[(x, y)].fg
}

/// Every pane border uses the shared palette: `PANE_BORDER` at rest,
/// `PANE_BORDER_FOCUSED` on the pane that owns the keys. Each renderer fills
/// the whole test area, so its single bordered edge sits at a known position
/// and only the colour is in question.
#[test]
fn pane_borders_share_the_palette_resting_and_focused() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    let (w, h) = (40, 12);

    // Explorer: bordered on its right edge only.
    app.chat.focus = Focus::ChatPane;
    assert_eq!(
        border_fg_at(w, h, w - 1, 3, |f, a| render_explorer(f, a, &app)),
        PANE_BORDER,
        "the resting explorer border carries the shared colour"
    );
    app.chat.focus = Focus::ExplorerPane;
    assert_eq!(
        border_fg_at(w, h, w - 1, 3, |f, a| render_explorer(f, a, &app)),
        PANE_BORDER_FOCUSED,
        "the focused pane lights up in the shared focus colour"
    );

    // Topic info pane: bordered on its left edge only, on both screens.
    app.chat.focus = Focus::ChatPane;
    assert_eq!(
        border_fg_at(w, h, 0, 3, |f, a| render_topic_info_pane(f, a, &mut app)),
        PANE_BORDER,
        "the info pane used to inherit the terminal default here"
    );
    app.chat.focus = Focus::InfoPane;
    assert_eq!(
        border_fg_at(w, h, 0, 3, |f, a| render_topic_info_pane(f, a, &mut app)),
        PANE_BORDER_FOCUSED
    );

    // Activity pane: bordered on its top edge only on the chat screen.
    for (focused, want) in [(false, PANE_BORDER), (true, PANE_BORDER_FOCUSED)] {
        assert_eq!(
            border_fg_at(w, h, w / 2, 0, |f, a| render_activity_log_inner(
                f,
                a,
                &[],
                0,
                0,
                focused,
            )),
            want,
            "activity border should be {want:?} when focused={focused}"
        );
    }
}

/// The same instant written with a different UTC offset must render the same.
/// The pane converts the parsed timestamp to the local timezone; printing it in
/// the offset it was written with would make `12:00:00Z` and `20:00:00+08:00`
/// (one moment) read as two different times.
#[test]
fn activity_rows_render_the_same_instant_identically() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let draw = |timestamp: &str| {
        let entry = jyc_types::ActivityEntry {
            event: serde_json::Value::Null,
            text: Some("Tool: bash".to_string()),
            timestamp: Some(timestamp.to_string()),
            id: 0,
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_activity_log_inner(frame, area, std::slice::from_ref(&entry), 0, 0, false);
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    };

    assert_eq!(
        draw("2026-08-13T12:00:00+00:00"),
        draw("2026-08-13T20:00:00+08:00"),
        "the pane must render the instant, not the offset it was written with"
    );
}

/// The three separators on a live chat screen rest in one colour, so the split
/// does not read as three unrelated widgets. Positions are derived from the same
/// layout percentages `ui_chat_mode` applies, and every cell is checked for the
/// box-drawing glyph it should carry before its colour is.
#[test]
fn chat_screen_separators_all_rest_in_one_colour() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.explorer_visible = true;
    app.chat.toggle_activity(); // bottom 20%
    app.chat.focus = Focus::ChatPane;

    // Explorer = 20% of the width; info pane = 20% of the remaining 80%;
    // activity pane = the bottom 20% of the right column.
    let (w, h) = (100, 30);
    let separators = [
        (w / 5 - 1, h / 2, "│", "explorer right border"),
        (w - (w - w / 5) / 5, h / 4, "│", "info pane left border"),
        (w / 5 + 4, h - h / 5, "─", "activity pane top border"),
    ];

    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    terminal
        .draw(|f| ui_chat_mode(f, f.area(), &mut app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();

    for (x, y, glyph, what) in separators {
        let cell = &buffer[(x, y)];
        assert_eq!(cell.symbol(), glyph, "{what} is not where the layout says");
        assert_eq!(cell.fg, PANE_BORDER, "{what} carries another colour");
    }
}

// The explorer walks parent → subagent: a subagent row sits under the topic
// that spawned it, a deeper chain keeps indenting, and a topic whose parent is
// gone (closed, or spawned by something no longer listed) stays a root so
// nothing drops out of the pane.
#[test]
fn explorer_rows_nest_subagents_under_their_parent() {
    let mut child = explorer_topic("b");
    child.parent = Some("a".to_string());
    let mut grandchild = explorer_topic("c");
    grandchild.parent = Some("b".to_string());
    let mut orphan = explorer_topic("d");
    orphan.parent = Some("gone".to_string());
    let s = jyc_types::InspectOverview {
        topics: vec![explorer_topic("a"), child, grandchild, orphan],
        ..Default::default()
    };

    let forest = topic_forest(&lineage_of(&s.topics));
    let rows = explorer_rows(&forest);
    let names: Vec<&str> = rows.iter().map(|r| r.node.name.as_str()).collect();
    assert_eq!(names, ["a", "b", "c", "d"], "tree order");
    assert_eq!(rows[0].prefix, "", "a root is not indented");
    assert_eq!(rows[1].prefix, "└─ ");
    assert_eq!(
        rows[2].prefix, "   └─ ",
        "a grandchild keeps the branch open"
    );
    assert_eq!(rows[3].prefix, "", "an orphan is a root");

    // Selection follows the tree rows, not the overview's flat list: the
    // explorer's row index is what `explorer_selected` holds.
    assert_eq!(row_of(&rows, "c", "test"), Some(2));
    assert_eq!(row_of(&rows, "d", "test"), Some(3));
    assert_eq!(row_of(&rows, "nope", "test"), None);
}

// A breadcrumb cycle — hand-edited state only — has no root for the walk to
// start from, so its members are emitted as the leftovers of that walk. They
// must not blank the pane, and `seen` is what stops the recursion the row
// starts from running forever.
#[test]
fn explorer_rows_keep_a_breadcrumb_cycle_visible() {
    let mut a = explorer_topic("a");
    a.parent = Some("b".to_string());
    let mut b = explorer_topic("b");
    b.parent = Some("a".to_string());
    let s = jyc_types::InspectOverview {
        topics: vec![a, b],
        ..Default::default()
    };

    let forest = topic_forest(&lineage_of(&s.topics));
    let rows = explorer_rows(&forest);
    let names: Vec<&str> = rows.iter().map(|r| r.node.name.as_str()).collect();
    assert_eq!(names, ["a", "b"], "both members stay listed, once each");
}

// The explorer's selection lives in row space. A child sorting before its
// parent is where row space and the overview's flat list disagree, so this is
// what pins `explorer_open_selected` — and the clamp in `explorer_move` — to
// the tree rows rather than to `state.topics`.
//
// `#[tokio::test]`: opening a topic goes through `Chat::open`, which spawns the
// websocket client task — the sync `#[test]` above panics with "there is no
// reactor running" the moment `open_addr` is set.
#[tokio::test]
async fn explorer_selection_follows_the_tree_row_not_the_flat_index() {
    let mut child = explorer_topic("b");
    child.parent = Some("z".to_string());
    let s = jyc_types::InspectOverview {
        // Flat order: the child `b` first, then its parent `z`.
        topics: vec![child, explorer_topic("z")],
        ..Default::default()
    };

    let forest = topic_forest(&lineage_of(&s.topics));
    let rows = explorer_rows(&forest);
    let names: Vec<&str> = rows.iter().map(|r| r.node.name.as_str()).collect();
    assert_eq!(names, ["z", "b"], "the parent comes first in the tree");
    assert_eq!(row_of(&rows, "b", "test"), Some(1));

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.open_addr = Some("test-addr".to_string());
    app.chat.token = None;
    app.state = Some(s);
    app.chat.explorer_selected = 0;

    explorer_open_selected(&mut app);

    assert_eq!(app.chat.topic.as_deref(), Some("z"), "row 0 is the parent");
    assert_eq!(
        app.pending_hydrate.as_ref(),
        Some(&("test".to_string(), "z".to_string()))
    );
}

// The pane draws the tree: a nested row carries its branch prefix before the
// status dot, so the child reads as sitting under its parent rather than as
// another top-level topic.
#[test]
fn explorer_renders_a_nested_row_indented() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.explorer_visible = true;
    app.chat.topic = Some("a".to_string());
    app.chat.channel = Some("test".to_string());
    let mut child = explorer_topic("b");
    child.parent = Some("a".to_string());
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![explorer_topic("a"), child],
        ..Default::default()
    });

    let (w, h) = (80, 10);
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    terminal
        .draw(|f| ui_chat_mode(f, f.area(), &mut app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let rows: Vec<String> = (0..h)
        .map(|y| {
            (0..w)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect()
        })
        .collect();
    let pane = rows.join("\n");

    assert!(
        rows.iter().any(|r| r.contains("└─ ● b")),
        "the child is drawn indented under its parent:\n{pane}"
    );
    assert!(
        rows.iter().any(|r| r.contains("● a")),
        "the parent is still drawn:\n{pane}"
    );
}

// Past the prefix cap a level freezes at the `…  ` marker — three columns, as
// wide as the level before it, so a deeper row never starts further left than
// its parent. The explorer pane is 20% wide (about fifteen columns), so an
// indent that grew with every level would soon leave a deep name nothing to
// print in.
#[test]
fn deep_nesting_freezes_the_indent() {
    let mut topics = vec![explorer_topic("r")];
    for i in 0..4usize {
        let mut t = explorer_topic(&format!("n{i}"));
        t.parent = Some(if i == 0 {
            "r".to_string()
        } else {
            format!("n{}", i - 1)
        });
        topics.push(t);
    }
    let s = jyc_types::InspectOverview {
        topics,
        ..Default::default()
    };

    let forest = topic_forest(&lineage_of(&s.topics));
    let rows = explorer_rows(&forest);
    let prefixes: Vec<&str> = rows.iter().map(|r| r.prefix.as_str()).collect();
    assert_eq!(
        prefixes,
        ["", "└─ ", "   └─ ", "…  └─ ", "…  └─ "],
        "the indent freezes rather than shrinking"
    );
}

// A cycle's back-edge must not leave the sibling drawn before it as a non-last
// child with nothing under it. The fixture order is what makes this a test of
// the filter rather than of the shape: the walk starts at the lowest flat
// index, so `zz` has to come first for `aa`'s kids to be `[a, seen zz]` — with
// the two swapped, `last` comes out the same either way and a revert passes.
#[test]
fn a_cycle_back_edge_leaves_no_dangling_branch_mark() {
    let mut zz = explorer_topic("zz");
    zz.parent = Some("aa".to_string());
    let mut aa = explorer_topic("aa");
    aa.parent = Some("zz".to_string());
    let mut a = explorer_topic("a");
    a.parent = Some("aa".to_string());
    let s = jyc_types::InspectOverview {
        topics: vec![zz, aa, a],
        ..Default::default()
    };

    let forest = topic_forest(&lineage_of(&s.topics));
    let rows = explorer_rows(&forest);
    let drawn: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| (r.node.name.as_str(), r.prefix.as_str()))
        .collect();
    assert_eq!(
        drawn,
        [("zz", ""), ("aa", "└─ "), ("a", "   └─ ")],
        "the last sibling still carries the corner mark"
    );
}

// The info pane's tree holds a topic's descendants only (the pane already
// names the topic itself) and closes the last branch of each level with `└─ `,
// so the sibling after a nested child lines up under its parent. The tree
// itself is the one the topic's own detail carries — the core's lineage
// rules built it — and this pins the drawing of it.
#[test]
fn the_pane_draws_the_subagent_tree_with_branch_prefixes() {
    let subagents = vec![
        subagent_node("b", vec![subagent_node("c", vec![])]),
        subagent_node("e", vec![]),
    ];

    let mut rows = Vec::new();
    push_tree_rows(&mut rows, &subagents, "");
    let names: Vec<&str> = rows.iter().map(|r| r.node.name.as_str()).collect();
    assert_eq!(names, ["b", "c", "e"], "depth-first");
    assert_eq!(rows[0].prefix, "├─ ");
    assert_eq!(rows[1].prefix, "│  └─ ");
    assert_eq!(rows[2].prefix, "└─ ");
}

// The rendered pane: the subagent tree sits between the cost row and the files
// section, nested by depth, with the explorer's own status dot.
#[test]
fn info_pane_renders_subagents_as_a_tree() {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    let mut app = App::new(rx, None);
    app.chat.visible = true;
    app.chat.topic = Some("jyc".to_string());
    app.chat.channel = Some("test".to_string());
    app.chat.info_visible = true;

    let mut child = explorer_topic("jyc-1");
    child.parent = Some("jyc".to_string());
    let mut grandchild = explorer_topic("jyc-2");
    grandchild.parent = Some("jyc-1".to_string());
    let mut sibling = explorer_topic("jyc-3");
    sibling.parent = Some("jyc".to_string());
    app.state = Some(jyc_types::InspectOverview {
        topics: vec![explorer_topic("jyc"), child, grandchild, sibling],
        ..Default::default()
    });
    // The pane renders the topic's own detail — the same source `/info` reads
    // — and the subagent tree is part of that detail.
    let mut detail = topic_detail_for("jyc", "test");
    detail.subagents = Some(vec![
        subagent_node("jyc-1", vec![subagent_node("jyc-2", vec![])]),
        subagent_node("jyc-3", vec![]),
    ]);
    // A cost row and a resolved file list, so the tree has the two sections
    // it is placed between.
    detail.cost = Some(jyc_types::TopicCost {
        session: 0.42,
        today: 1.23,
        currency: "USD".to_string(),
    });
    detail.changed_files = Some(vec![]);
    app.topic_detail = Some(detail);
    app.table_state.select(Some(0));

    // Tall enough that the section is not scrolled out of the pane.
    let pane = info_pane_text(&mut app, 40, 40);
    assert!(
        pane.contains("Subagents (3):"),
        "no subagent section:\n{pane}"
    );
    assert!(pane.contains("├─ ● jyc-1"), "first child unmarked:\n{pane}");
    assert!(
        pane.contains("│  └─ ● jyc-2"),
        "grandchild not nested:\n{pane}"
    );
    assert!(pane.contains("└─ ● jyc-3"), "last child wrong:\n{pane}");

    // The tree sits between the cost row and the files section.
    let cost = pane.find("Cost:").expect("the cost row");
    let tree = pane.find("Subagents (3):").expect("the subagent section");
    let files = pane.find("Files:").expect("the files section");
    assert!(
        cost < tree && tree < files,
        "the tree is out of place:\n{pane}"
    );
}

/// The bar the pane draws for a share: `filled` cells of `─`, the rest `·`,
/// `width` cells in all. The width is pinned here on purpose, so a change
/// to the bar's length has to show up in these tests.
fn info_pane_bar(filled: usize, width: usize) -> String {
    "─".repeat(filled) + &"·".repeat(width - filled)
}

/// The width every label of the info pane is padded to — the length of
/// the longest one, "Today (all): ".
const INFO_PANE_LABEL_WIDTH: usize = 13;

/// The label of a pane row, padded to the width every label of the pane
/// shares so that every value starts in the same column. Pinned on
/// purpose: a change to that width breaks this test loudly, so the
/// sketches and the docs are updated with it.
fn info_pane_label(label: &str) -> String {
    format!("{label:<width$}", width = INFO_PANE_LABEL_WIDTH)
}

/// The pane's topic row, as the table layout draws it: the padded label
/// and the topic's name beside it.
fn info_pane_topic_row() -> String {
    format!("{}jyc", info_pane_label("Topic: "))
}

/// The pane's rows, with the border column dropped: what the pane says,
/// one row per line. Each row is trimmed at the end, so a row's own
/// padding does not show.
///
/// The border is dropped by *character*, not by byte: the pane's border
/// is `│`, three bytes wide, so a byte offset of one is not a character
/// boundary and would drop the whole row.
fn info_pane_rows(pane: &str) -> Vec<String> {
    pane.lines()
        .map(|row| {
            row.chars()
                .skip(1)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// The row of the pane that holds the given label's value — the row
/// below the label's own row in the stacked layout.
fn info_pane_value_row<'a>(rows: &'a [String], label: &str) -> &'a str {
    let at = rows
        .iter()
        .position(|r| r == label)
        .unwrap_or_else(|| panic!("no row for the label {label:?}: {rows:?}"));
    rows.get(at + 1)
        .map(String::as_str)
        .unwrap_or_else(|| panic!("no value row under the label {label:?}: {rows:?}"))
}

/// The pane's two percentage rows lead with a bar: the context share and
/// the cache hit rate, each in front of its figures. The figures have a
/// row of their own, under the bar. The pane is what renders them, so
/// this looks at the pane rather than at the helper.
#[test]
fn info_pane_draws_the_percentage_bars() {
    let mut app = info_pane_app(Default::default());
    {
        let detail = app.topic_detail.as_mut().expect("the topic's detail");
        detail.context_input_tokens = Some(47_500);
        detail.max_tokens = Some(100_000);
        detail.total_input_tokens = Some(90_311);
        detail.total_cache_hit_tokens = Some(80_120);
    }

    // Wide enough that neither row wraps.
    let pane = info_pane_text(&mut app, 70, 30);
    let rows = info_pane_rows(&pane);

    // The pane's inner width is 69: the bar takes what the label's room
    // leaves, less the two cells it keeps to the pane's edge — 54 cells.
    // 47% of the window, 88% of the input served from the cache.
    assert!(
        rows.iter()
            .any(|r| { r == &format!("{}{}", info_pane_label("Tokens: "), info_pane_bar(25, 54)) }),
        "the tokens row must lead with its bar:\n{pane}"
    );
    assert!(
        rows.iter().any(|r| {
            r == &format!("{}{}", info_pane_label("Session: "), info_pane_bar(48, 54))
        }),
        "the cache row must lead with its bar:\n{pane}"
    );
    // The figures have a row of their own, in the value's column, under
    // the bar they belong to.
    for figures in ["46.39K / 97.66K (47%)", "78.24K / 88.19K (88%)"] {
        assert!(
            rows.iter()
                .any(|r| r == &format!("{}{figures}", " ".repeat(INFO_PANE_LABEL_WIDTH))),
            "the figures must have a row of their own:\n{pane}"
        );
    }
}

/// The turn's cache row is drawn before the turn's first call, at zero:
/// the turn's counters are zeroed at every turn boundary, so a row that
/// vanished until the first call landed would read as missing data
/// rather than as a zero.
#[test]
fn info_pane_shows_the_turn_cache_row_at_zero() {
    let mut app = info_pane_app(Default::default());

    // Wide enough that the row does not wrap.
    let pane = info_pane_text(&mut app, 70, 30);
    let rows = info_pane_rows(&pane);

    assert!(
        rows.iter()
            .any(|r| { r == &format!("{}{}", info_pane_label("Turn: "), info_pane_bar(0, 54)) }),
        "the turn's cache row must be there, at zero:\n{pane}"
    );
    // Its figures have a row of their own, under the bar.
    assert!(
        rows.iter()
            .any(|r| r == &format!("{}0 / 0 (0%)", " ".repeat(INFO_PANE_LABEL_WIDTH))),
        "the turn's cache row must carry its figures:\n{pane}"
    );
}

/// The pane's cache rows sit under one header: the group names the
/// subject — the cache — and the rows under it name their own scope. The
/// header is a raw line, so it is bold and keeps its own shape instead of
/// being padded into the label column.
#[test]
fn info_pane_groups_the_cache_rows_under_a_header() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = info_pane_app(Default::default());
    {
        let detail = app.topic_detail.as_mut().expect("the topic's detail");
        detail.total_input_tokens = Some(90_311);
        detail.total_cache_hit_tokens = Some(80_120);
    }

    // Wide enough that no row of the group wraps.
    let backend = TestBackend::new(70, 30);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| render_topic_info_pane(frame, frame.area(), &mut app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let pane = (0..30)
        .map(|y| {
            (0..70)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let rows = info_pane_rows(&pane);
    let at = rows
        .iter()
        .position(|r| r == "Cache:")
        .unwrap_or_else(|| panic!("the cache rows must sit under a header:\n{pane}"));
    // The turn's row is always drawn, so the header never dangles over
    // nothing.
    assert!(
        rows[at + 1].starts_with(&info_pane_label("Turn: ")),
        "the header must have the turn's row under it:\n{pane}"
    );
    // The session's row names its own scope.
    assert!(
        rows.iter()
            .any(|r| r.starts_with(&info_pane_label("Session: "))),
        "the session's row must name its scope:\n{pane}"
    );
    // The header is bold: it is a heading, not a value.
    assert!(
        buffer[(1, at as u16)].modifier.contains(Modifier::BOLD),
        "the header must be bold:\n{pane}"
    );
}

/// The bar is as wide as the pane allows, and it stays whole: in the
/// stacked layout the bar has a row of its own, so it can use the
/// pane's full width less the two cells it keeps to the edge, and the
/// figures move to the row under it rather than cutting the bar in half.
#[test]
fn info_pane_bar_fits_a_narrow_pane() {
    let mut app = info_pane_app(Default::default());
    {
        let detail = app.topic_detail.as_mut().expect("the topic's detail");
        detail.context_input_tokens = Some(47_500);
        detail.max_tokens = Some(100_000);
        detail.total_input_tokens = Some(90_311);
        detail.total_cache_hit_tokens = Some(80_120);
    }

    // 30 columns wide: the pane's inner width is 29, too narrow for the
    // table layout, so the bar gets a row of its own — twenty-seven
    // cells, all but the two it keeps to the pane's edge.
    let pane = info_pane_text(&mut app, 30, 30);
    let rows = info_pane_rows(&pane);
    assert!(
        info_pane_value_row(&rows, "Tokens:").starts_with(&info_pane_bar(13, 27)),
        "the bar must stay whole in a narrow pane:\n{pane}"
    );
    assert!(
        info_pane_value_row(&rows, "Session:").starts_with(&info_pane_bar(24, 27)),
        "the bar must stay whole in a narrow pane:\n{pane}"
    );
}

/// A bar needs five cells to carry a share honestly: below that a single
/// cell would stand for a fifth of the share or more, so the row shows
/// its figures alone rather than a bar that misreports them. The stacked
/// layout gives the bar a row of its own, so only a pane of four cells
/// misses one.
#[test]
fn info_pane_draws_no_bar_in_a_too_narrow_pane() {
    let mut app = info_pane_app(Default::default());
    {
        let detail = app.topic_detail.as_mut().expect("the topic's detail");
        detail.context_input_tokens = Some(47_500);
        detail.max_tokens = Some(100_000);
        detail.total_input_tokens = Some(90_311);
        detail.total_cache_hit_tokens = Some(80_120);
    }
    // The status block is hidden: its own separator is a row of `─`, and
    // this test looks for the absence of a bar.
    app.chat.status_visible = false;

    // 5 columns wide: the pane's inner width is four, too narrow for an
    // honest bar, so there is none. The pane is tall enough to hold the
    // whole column: at this width every label wraps over several rows,
    // so the cache hit rate's figure would be cut off by a short one.
    let pane = info_pane_text(&mut app, 5, 80);
    assert!(
        !pane.contains('─'),
        "a pane too narrow for an honest bar must draw none:\n{pane}"
    );
    // The figures are still there — wrapped across the rows of a pane
    // this narrow.
    assert!(
        pane.contains("47%") && pane.contains("88%"),
        "the figures must still be there:\n{pane}"
    );
}

/// A value too long for the pane's value column wraps inside it, and the
/// line that continues it is indented to that column — so the column keeps
/// its values instead of dropping them back to the pane's edge.
#[test]
fn info_pane_indents_a_continued_value_to_its_column() {
    let mut app = info_pane_app(Default::default());
    // A branch name longer than the pane's value column, so it has to
    // wrap. Where the wrap falls depends on the column's width, so the
    // test does not name the words that end up on a row.
    let branch = "feat/issue-42 add a very long branch name for the info pane";
    {
        let detail = app.topic_detail.as_mut().expect("the topic's detail");
        detail.branch = Some(branch.to_string());
    }

    // 46 columns wide: the pane's inner width is 45, wide enough for the
    // table layout.
    let pane = info_pane_text(&mut app, 46, 40);
    let rows = info_pane_rows(&pane);
    // The branch's own row, and the rows that continue it: they carry the
    // rest of the value, each indented to the value's column.
    let at = rows
        .iter()
        .position(|r| r.starts_with(&info_pane_label("Branch: ")))
        .unwrap_or_else(|| panic!("no row for the branch:\n{pane}"));
    let indent = " ".repeat(INFO_PANE_LABEL_WIDTH);
    let continued: Vec<&str> = rows[at + 1..]
        .iter()
        .take_while(|r| r.starts_with(&indent))
        .map(String::as_str)
        .collect();
    assert!(
        !continued.is_empty(),
        "a value too long for its column must wrap, not be cut:\n{pane}"
    );
    for row in &continued {
        assert_eq!(
            row.chars().take_while(|c| *c == ' ').count(),
            INFO_PANE_LABEL_WIDTH,
            "the rest of a value must stay in the value's column:\n{pane}"
        );
    }
    // The value is whole: the rows carry it between them, with the space
    // the break fell on.
    let mut shown: String = rows[at].chars().skip(INFO_PANE_LABEL_WIDTH).collect();
    for row in &continued {
        shown.push(' ');
        shown.push_str(row.trim_start());
    }
    assert_eq!(shown, branch, "the value must not lose a word:\n{pane}");
}

/// The figures of a bar row have a row of their own, in the pane's value
/// column: they line up with the bar above them, so a reader can follow
/// the bar down to the figures it stands for.
#[test]
fn info_pane_puts_the_bar_rows_figures_in_the_value_column() {
    let mut app = info_pane_app(Default::default());
    {
        let detail = app.topic_detail.as_mut().expect("the topic's detail");
        detail.context_input_tokens = Some(47_500);
        detail.max_tokens = Some(100_000);
    }

    // 46 columns wide: the pane's inner width is 45, wide enough for the
    // table layout.
    let pane = info_pane_text(&mut app, 46, 40);
    let rows = info_pane_rows(&pane);
    let row = rows
        .iter()
        .find(|r| r.contains("97.66K (47%)"))
        .unwrap_or_else(|| panic!("no row carries the tokens figures:\n{pane}"));
    // The figures' row starts in the value's column: the same column the
    // bar starts in, so the two line up.
    assert_eq!(
        row.len() - row.trim_start().len(),
        INFO_PANE_LABEL_WIDTH,
        "the figures must stay in the value's column:\n{pane}"
    );
    // The bar has its row to itself: the figures are not beside it.
    let bar_row = rows
        .iter()
        .find(|r| r.starts_with(&info_pane_label("Tokens: ")))
        .unwrap_or_else(|| panic!("no row for the tokens:\n{pane}"));
    assert!(
        !bar_row.contains("97.66K"),
        "the figures must have a row of their own:\n{pane}"
    );
}

/// A value that belongs to the row above it — the figures of a bar row —
/// joins that row's column: in the table layout the value's column, in
/// the stacked one the pane's first column.
#[test]
fn info_pane_puts_a_bars_figures_in_the_value_column() {
    let figures = || InfoRow::Value(Span::raw("46.39K / 97.66K (47%)"));

    // The table layout: the value's column, the one the bar starts in.
    let lines = layout_info_rows(vec![figures()], PaneLayout::Table, 45);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        info_pane_line_text(&lines[0]),
        format!("{}46.39K / 97.66K (47%)", " ".repeat(INFO_PANE_LABEL_WIDTH)),
        "the figures must sit in the value's column"
    );

    // The stacked layout: flush left, where the bar's own row starts.
    let lines = layout_info_rows(vec![figures()], PaneLayout::Stacked, 29);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        info_pane_line_text(&lines[0]),
        "46.39K / 97.66K (47%)",
        "the figures must sit flush left"
    );
}

/// The text of a laid-out line: its spans' contents in order.
fn info_pane_line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The table layout, one row at a time: the value starts in the label's
/// column, and a value too long for the column continues in it — the lines
/// that carry the rest of it are indented to the same column.
#[test]
fn info_pane_table_layout_keeps_a_long_value_in_its_column() {
    let rows = vec![InfoRow::Field(vec![
        label_span("Cost: "),
        Span::raw("467.8899 session - 84.3464 today"),
    ])];
    // 39 cells wide: the label column takes sixteen, the value column the
    // rest.
    let lines = layout_info_rows(rows, PaneLayout::Table, 39);
    assert_eq!(lines.len(), 2, "the value must wrap inside the column");
    assert_eq!(
        info_pane_line_text(&lines[0]).find("467.8899"),
        Some(INFO_PANE_LABEL_WIDTH),
        "the value must start in the label's column"
    );
    assert_eq!(
        info_pane_line_text(&lines[1]).find("84.3464"),
        Some(INFO_PANE_LABEL_WIDTH),
        "the rest of the value must stay in the label's column"
    );
}

/// The stacked layout, one row at a time: the label takes a row of its
/// own, without the padding it carries in the table layout, and the value
/// follows flush left under it.
#[test]
fn info_pane_stacked_layout_drops_the_labels_padding() {
    let rows = vec![InfoRow::Field(vec![
        label_span("Topic: "),
        Span::raw("jyc"),
    ])];
    let lines = layout_info_rows(rows, PaneLayout::Stacked, 29);
    assert_eq!(info_pane_line_text(&lines[0]), "Topic:");
    assert_eq!(info_pane_line_text(&lines[1]), "jyc");
}

/// The table layout: every label is padded to the same width, so every
/// value of the pane starts in the same column and the pane reads as one
/// table rather than a ragged list.
#[test]
fn info_pane_aligns_every_value_in_one_column() {
    let mut app = info_pane_app(Default::default());
    {
        let detail = app.topic_detail.as_mut().expect("the topic's detail");
        detail.model = Some("minimax/Minimax-M3".to_string());
        detail.output_tokens = Some(1_234);
    }

    // 46 columns wide: the pane's inner width is 45, wide enough for the
    // table layout.
    let pane = info_pane_text(&mut app, 46, 40);
    let rows = info_pane_rows(&pane);
    for (label, value) in [
        ("Topic: ", "jyc"),
        ("Pattern: ", "jyc"),
        ("Model: ", "minimax/Minimax-M3"),
        ("Mode: ", "build"),
        ("Status: ", "Idle"),
        ("Output: ", "0 turn · 1.21K"),
    ] {
        let row = rows
            .iter()
            .find(|r| r.starts_with(label))
            .unwrap_or_else(|| panic!("no row for the label {label:?}:\n{pane}"));
        assert_eq!(
            row.find(value),
            Some(INFO_PANE_LABEL_WIDTH),
            "the value of {label:?} must start in the label's column:\n{pane}"
        );
    }
}

/// The stacked layout: a pane too narrow for a value column gives every
/// row a label row of its own, and the value follows flush left under
/// it — both rows start at the pane's first column.
#[test]
fn info_pane_stacks_the_value_under_its_label() {
    let mut app = info_pane_app(Default::default());

    // 30 columns wide: the pane's inner width is 29, too narrow for the
    // table layout.
    let pane = info_pane_text(&mut app, 30, 40);
    let rows = info_pane_rows(&pane);
    for (label, value) in [("Topic:", "jyc"), ("Status:", "Idle")] {
        assert!(
            rows.iter().any(|r| r == label),
            "the label {label:?} must take a row of its own:\n{pane}"
        );
        assert_eq!(
            info_pane_value_row(&rows, label),
            value,
            "the value must sit flush left under its label:\n{pane}"
        );
    }
}

/// The layout switches at one width, and the whole pane switches with
/// it: 29 inner cells still stack, 31 fit the table.
#[test]
fn info_pane_switches_layout_at_the_threshold() {
    let mut app = info_pane_app(Default::default());

    // 30 columns wide: the pane's inner width is 29 — too narrow for the
    // table layout, so the label takes a row of its own.
    let pane = info_pane_text(&mut app, 30, 40);
    let rows = info_pane_rows(&pane);
    assert!(
        rows.iter().any(|r| r == "Topic:"),
        "a narrow pane stacks its rows:\n{pane}"
    );
    assert!(
        !rows.iter().any(|r| r == &info_pane_topic_row()),
        "a narrow pane must not lay its rows out as a table:\n{pane}"
    );

    // 32 columns wide: the inner width is 31 — the table layout, label
    // and value on one row.
    let pane = info_pane_text(&mut app, 32, 40);
    let rows = info_pane_rows(&pane);
    assert!(
        rows.iter().any(|r| r == &info_pane_topic_row()),
        "a wide pane lays its rows out as a table:\n{pane}"
    );
    assert!(
        !rows.iter().any(|r| r == "Topic:"),
        "a wide pane must not stack its rows:\n{pane}"
    );
}
