use anyhow::Result;
use clap::Args;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    prelude::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame, Terminal,
};
use std::io::stdout;
use std::time::Duration;

use crate::inspect::client::InspectClient;
use crate::inspect::types::{InspectState, ThreadStatus};

#[derive(Args, Debug)]
pub struct DashboardArgs {
    /// Inspect server address
    #[arg(long, default_value = "127.0.0.1:9876")]
    pub addr: String,
}

/// Application state for the TUI.
struct App {
    state: Option<InspectState>,
    error: Option<String>,
    table_state: TableState,
    should_quit: bool,
}

impl App {
    fn new() -> Self {
        Self {
            state: None,
            error: None,
            table_state: TableState::default(),
            should_quit: false,
        }
    }

    fn next_thread(&mut self) {
        let count = self
            .state
            .as_ref()
            .map(|s| s.threads.len())
            .unwrap_or(0);
        if count == 0 {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => (i + 1) % count,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    fn prev_thread(&mut self) {
        let count = self
            .state
            .as_ref()
            .map(|s| s.threads.len())
            .unwrap_or(0);
        if count == 0 {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i == 0 {
                    count - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }
}

pub async fn run(args: &DashboardArgs) -> Result<()> {
    let mut client = InspectClient::new(&args.addr);

    // Setup terminal
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let poll_interval = Duration::from_millis(500);
    let mut last_poll = std::time::Instant::now() - poll_interval; // Force immediate poll

    let result = loop {
        // Poll for new state
        if last_poll.elapsed() >= poll_interval {
            match client.get_state().await {
                Ok(state) => {
                    app.state = Some(state);
                    app.error = None;
                }
                Err(e) => {
                    app.error = Some(format!("{e:#}"));
                }
            }
            last_poll = std::time::Instant::now();
        }

        // Draw
        terminal.draw(|f| ui(f, &mut app))?;

        // Handle input (non-blocking, 50ms timeout)
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            app.should_quit = true;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            app.next_thread();
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            app.prev_thread();
                        }
                        KeyCode::Char('r') => {
                            // Force refresh
                            last_poll =
                                std::time::Instant::now() - poll_interval;
                        }
                        _ => {}
                    }
                }
            }
        }

        if app.should_quit {
            break Ok(());
        }
    };

    // Restore terminal
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    result
}

fn ui(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Main layout: channels bar | threads table | detail panel | status bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Channels bar
            Constraint::Percentage(40), // Threads table
            Constraint::Percentage(60), // Detail panel + activity log
            Constraint::Length(1),  // Status bar
        ])
        .split(area);

    render_channels(frame, chunks[0], app);
    render_threads(frame, chunks[1], app);
    render_details(frame, chunks[2], app);
    render_status_bar(frame, chunks[3], app);
}

fn render_channels(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(" Channels ")
        .borders(Borders::ALL);

    if let Some(ref error) = app.error {
        let text = Paragraph::new(Line::from(vec![
            Span::styled("Not connected: ", Style::default().fg(Color::Red)),
            Span::raw(error.as_str()),
        ]))
        .block(block);
        frame.render_widget(text, area);
        return;
    }

    let state = match &app.state {
        Some(s) => s,
        None => {
            let text = Paragraph::new("Connecting...").block(block);
            frame.render_widget(text, area);
            return;
        }
    };

    let spans: Vec<Span> = state
        .channels
        .iter()
        .enumerate()
        .flat_map(|(i, ch)| {
            let mut parts = vec![];
            if i > 0 {
                parts.push(Span::raw("  "));
            }
            parts.push(Span::styled(
                "●",
                Style::default().fg(Color::Green),
            ));
            parts.push(Span::raw(format!(" {} ({})", ch.name, ch.channel_type)));
            parts
        })
        .collect();

    let text = Paragraph::new(Line::from(spans)).block(block);
    frame.render_widget(text, area);
}

fn render_threads(frame: &mut Frame, area: Rect, app: &mut App) {
    let state = match &app.state {
        Some(s) => s,
        None => {
            let block = Block::default()
                .title(" Threads ")
                .borders(Borders::ALL);
            frame.render_widget(block, area);
            return;
        }
    };

    let header = Row::new(vec![
        Cell::from("Thread"),
        Cell::from("Channel"),
        Cell::from("Pattern"),
        Cell::from("Status"),
        Cell::from("Tokens"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD))
    .height(1);

    let rows: Vec<Row> = state
        .threads
        .iter()
        .map(|t| {
            let status_style = match t.status {
                ThreadStatus::Processing => Style::default().fg(Color::Green),
                ThreadStatus::Queued => Style::default().fg(Color::Yellow),
                ThreadStatus::WaitingForAnswer => Style::default().fg(Color::Cyan),
                ThreadStatus::Idle => Style::default().fg(Color::DarkGray),
            };

            let tokens = match (t.input_tokens, t.max_tokens) {
                (Some(cur), Some(max)) => format!("{}K/{}K", cur / 1000, max / 1000),
                (Some(cur), None) => format!("{}K", cur / 1000),
                _ => "-".to_string(),
            };

            Row::new(vec![
                Cell::from(t.name.clone()),
                Cell::from(t.channel.clone()),
                Cell::from(t.pattern.clone().unwrap_or("-".into())),
                Cell::from(Span::styled(format!("{}", t.status), status_style)),
                Cell::from(tokens),
            ])
        })
        .collect();

    let widths = [
        Constraint::Min(20),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(12),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .title(format!(" Threads ({}) ", state.threads.len()))
                .borders(Borders::ALL),
        )
        .row_highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED),
        );

    frame.render_stateful_widget(table, area, &mut app.table_state);
}

fn render_details(frame: &mut Frame, area: Rect, app: &App) {
    let state = match &app.state {
        Some(s) => s,
        None => {
            let block = Block::default()
                .title(" Details ")
                .borders(Borders::ALL);
            frame.render_widget(block, area);
            return;
        }
    };

    let selected = app
        .table_state
        .selected()
        .and_then(|i| state.threads.get(i));

    let selected = match selected {
        Some(t) => t,
        None => {
            let block = Block::default()
                .title(" Details ")
                .borders(Borders::ALL);
            let text = Paragraph::new("Select a thread with ↑/↓").block(block);
            frame.render_widget(text, area);
            return;
        }
    };

    // Split detail area: info (4 lines) + activity log (remaining)
    let detail_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // Thread info
            Constraint::Min(4),   // Activity log
        ])
        .split(area);

    // Thread info panel
    let info_block = Block::default()
        .title(format!(" {} ", selected.name))
        .borders(Borders::ALL);

    let mut info_lines = vec![];

    info_lines.push(Line::from(vec![
        Span::styled("Channel: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(&selected.channel),
        Span::raw("  "),
        Span::styled("Pattern: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(selected.pattern.as_deref().unwrap_or("-")),
    ]));

    info_lines.push(Line::from(vec![
        Span::styled("Model: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(selected.model.as_deref().unwrap_or("(default)")),
        Span::raw("  "),
        Span::styled("Mode: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(selected.mode.as_deref().unwrap_or("build")),
    ]));

    let mut status_line = vec![
        Span::styled("Status: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{}", selected.status),
            match selected.status {
                ThreadStatus::Processing => Style::default().fg(Color::Green),
                ThreadStatus::Queued => Style::default().fg(Color::Yellow),
                ThreadStatus::WaitingForAnswer => Style::default().fg(Color::Cyan),
                ThreadStatus::Idle => Style::default().fg(Color::DarkGray),
            },
        ),
    ];

    if let (Some(cur), Some(max)) = (selected.input_tokens, selected.max_tokens) {
        let pct = if max > 0 { cur * 100 / max } else { 0 };
        status_line.push(Span::raw("  "));
        status_line.push(Span::styled("Tokens: ", Style::default().add_modifier(Modifier::BOLD)));
        status_line.push(Span::raw(format!("{cur} / {max} ({pct}%)")));
    }
    info_lines.push(Line::from(status_line));

    let info = Paragraph::new(info_lines).block(info_block);
    frame.render_widget(info, detail_chunks[0]);

    // Activity log panel
    let activity_block = Block::default()
        .title(" Activity ")
        .borders(Borders::ALL);

    if selected.activity.is_empty() {
        let text = Paragraph::new(Span::styled(
            "  No activity",
            Style::default().fg(Color::DarkGray),
        ))
        .block(activity_block);
        frame.render_widget(text, detail_chunks[1]);
    } else {
        let activity_lines: Vec<Line> = selected
            .activity
            .iter()
            .map(|entry| {
                Line::from(vec![
                    Span::styled(
                        format!("  {} ", entry.time),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(&entry.text),
                ])
            })
            .collect();

        let text = Paragraph::new(activity_lines).block(activity_block);
        frame.render_widget(text, detail_chunks[1]);
    }
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let state = match &app.state {
        Some(s) => s,
        None => {
            let bar = Paragraph::new(" [q] quit  [↑↓] select  [r] refresh")
                .style(Style::default().bg(Color::DarkGray).fg(Color::White));
            frame.render_widget(bar, area);
            return;
        }
    };

    let uptime = format_duration(state.uptime_secs);
    let stats = &state.stats;

    let bar = Paragraph::new(Line::from(vec![
        Span::raw(format!(
            " {} active / {} threads │ {} recv │ {} err │ up {} │ v{} ",
            stats.active_workers,
            stats.total_threads,
            stats.messages_received,
            stats.errors,
            uptime,
            state.version,
        )),
        Span::raw("  "),
        Span::styled(
            "[q] quit  [↑↓] select  [r] refresh",
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .style(Style::default().bg(Color::DarkGray).fg(Color::White));

    frame.render_widget(bar, area);
}

fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else {
        format!("{mins}m")
    }
}
