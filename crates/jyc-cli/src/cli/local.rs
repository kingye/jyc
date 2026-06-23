//! Local TUI channel CLI command.
//!
//! Starts an interactive terminal chat session with JYC.
//! Each `jyc local` instance is an independent process with its own
//! workspace and agent context.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use clap::Args;
use crossterm::{
    ExecutableCommand,
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    layout::{Constraint, Direction, Layout},
    prelude::CrosstermBackend,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use jyc_channels::local::LocalMatcher;
use jyc_channels::local::inbound::LocalInboundAdapter;
use jyc_channels::local::outbound::LocalOutboundAdapter;
use jyc_core::message_router::MessageRouter;
use jyc_core::message_storage::MessageStorage;
use jyc_core::thread_manager::ThreadManager;
use jyc_types::{InboundAdapter, InboundAdapterOptions, OutboundAdapter, load_config};

#[derive(Args, Debug)]
pub struct LocalArgs {
    /// Channel name (must match a `type = "local"` channel in config.toml)
    #[arg(short, long, default_value = "local")]
    pub name: String,
}

/// Run the local TUI channel.
pub async fn run(args: &LocalArgs, workdir: &Path) -> Result<()> {
    // 1. Load config
    let config_path = workdir.join("config.toml");
    tracing::info!(config = %config_path.display(), "Loading configuration");

    let config = load_config(&config_path)?;
    let config = Arc::new(ArcSwap::from_pointee(config));
    let config_snapshot = config.load();

    // 2. Find the local channel
    let channel_config = config_snapshot
        .channels
        .get(&args.name)
        .with_context(|| format!("channel '{}' not found in config", args.name))?;

    anyhow::ensure!(
        channel_config.channel_type == "local",
        "channel '{}' is not type 'local' (got '{}')",
        args.name,
        channel_config.channel_type
    );

    let channel_name = args.name.clone();
    let patterns = channel_config.patterns.clone().unwrap_or_default();

    // 3. Setup workspace and storage
    let workspace_dir = jyc_core::thread_path::resolve_workspace(workdir, &channel_name);
    let storage = Arc::new(MessageStorage::new(&workspace_dir));

    // 4. Create outbound adapter
    let outbound = Arc::new(LocalOutboundAdapter::new());
    outbound
        .connect()
        .await
        .with_context(|| format!("channel '{channel_name}': outbound connection failed"))?;
    tracing::info!(channel = %channel_name, "Local outbound connected");

    // 5. Create agent
    let agent_result = crate::cli::agent_builder::build_agent_service(
        &config_snapshot.agent,
        channel_config,
        workdir,
        outbound.clone(),
        patterns.clone(),
        config_snapshot.mcps.clone(),
        None, // local channel has no inbound attachment config
        &channel_name,
    )?;
    let agent = agent_result.agent;

    // 6. Create thread manager and router
    let cancel = CancellationToken::new();
    let thread_manager = Arc::new(ThreadManager::new_with_options(
        config_snapshot.general.max_concurrent_threads,
        config_snapshot.general.max_queue_size_per_thread,
        storage.clone(),
        outbound.clone(),
        agent,
        cancel.clone(),
        true,
        workdir.join("templates"),
        config.clone(),
        channel_name.clone(),
        "local".to_string(),
        workspace_dir.clone(),
        jyc_core::metrics::MetricsHandle::noop(), // no metrics in local mode
    ));

    let router = Arc::new(MessageRouter::new(thread_manager.clone(), storage.clone()));

    // 7. Setup shutdown signal
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("Received Ctrl+C, shutting down...");
        cancel_clone.cancel();
    });

    // 8. Create inbound adapter with TUI spawner
    let run_tui: jyc_channels::local::inbound::TuiSpawner = Box::new(
        move |input_tx: tokio::sync::mpsc::UnboundedSender<String>,
              output_rx: tokio::sync::mpsc::UnboundedReceiver<String>| {
            tokio::task::spawn_blocking(move || run_tui(input_tx, output_rx))
        },
    );

    let adapter = LocalInboundAdapter::new(channel_name.clone(), outbound.output_tx_arc(), run_tui);

    let patterns_for_callback = patterns.clone();
    let router_for_callback = router.clone();
    let channel_name_for_callback = channel_name.clone();
    let tm_for_callback = thread_manager.clone();

    let options = InboundAdapterOptions {
        on_message: Box::new(move |message| {
            let router = router_for_callback.clone();
            let patterns = patterns_for_callback.clone();
            let channel_name = channel_name_for_callback.clone();
            tokio::spawn(async move {
                router
                    .route(&LocalMatcher::new(channel_name), message, &patterns)
                    .await;
            });
            Ok(())
        }),
        on_thread_close: Some(Box::new(move |thread_name: String| {
            let tm = tm_for_callback.clone();
            tokio::spawn(async move {
                if let Err(e) = tm.close_thread(&thread_name).await {
                    tracing::error!(error = %e, thread = %thread_name, "Failed to close thread");
                }
            });
            Ok(())
        })),
        on_error: Box::new(|error| {
            tracing::error!(error = %error, "Local inbound error");
        }),
        attachment_config: None,
    };

    tracing::info!(channel = %channel_name, "Starting local TUI channel");

    // 9. Start the adapter (blocks until TUI exits or Ctrl+C)
    let adapter_result = adapter
        .start(options, cancel.clone())
        .instrument(tracing::info_span!("in", ch = %channel_name))
        .await;

    // 10. Shutdown
    thread_manager.shutdown().await;

    adapter_result
}

// ─── TUI Implementation ───────────────────────────────────────────

struct LocalApp {
    conversation: Vec<(String, String)>, // (role, text)
    input_buffer: String,
    scroll_offset: usize,
    should_quit: bool,
}

impl LocalApp {
    fn new() -> Self {
        Self {
            conversation: vec![],
            input_buffer: String::new(),
            scroll_offset: 0,
            should_quit: false,
        }
    }

    fn add_user_message(&mut self, text: String) {
        self.conversation.push(("You".to_string(), text));
        self.scroll_offset = self.conversation.len().saturating_sub(1);
    }

    fn add_agent_message(&mut self, text: String) {
        self.conversation.push(("Agent".to_string(), text));
        self.scroll_offset = self.conversation.len().saturating_sub(1);
    }

    fn send_input(&mut self, input_tx: &tokio::sync::mpsc::UnboundedSender<String>) {
        let text = self.input_buffer.trim().to_string();
        if text.is_empty() {
            // Silently ignore empty/whitespace-only input; keep buffer so
            // the user sees nothing was sent.
            return;
        }
        self.add_user_message(text.clone());
        if let Err(e) = input_tx.send(text) {
            tracing::warn!(error = %e, "Failed to send input to adapter");
        }
        self.input_buffer.clear();
    }
}

fn run_tui(
    input_tx: tokio::sync::mpsc::UnboundedSender<String>,
    mut output_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    // Terminal setup
    enable_raw_mode()?;
    std::io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = LocalApp::new();
    let poll_interval = std::time::Duration::from_millis(50);

    let result = loop {
        // Check for AI replies
        while let Ok(text) = output_rx.try_recv() {
            app.add_agent_message(text);
        }

        // Draw
        if let Err(e) = terminal.draw(|f| draw_ui(f, &mut app)) {
            break Err(anyhow::anyhow!("TUI draw error: {e}"));
        }

        // Handle input
        if event::poll(poll_interval).unwrap_or(false)
            && let Ok(Event::Key(key)) = event::read()
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    app.should_quit = true;
                }
                KeyCode::Char('d')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    app.send_input(&input_tx);
                }
                KeyCode::Char(c) => {
                    app.input_buffer.push(c);
                }
                KeyCode::Enter => {
                    app.input_buffer.push('\n');
                }
                KeyCode::Backspace => {
                    app.input_buffer.pop();
                }
                KeyCode::Up => {
                    app.scroll_offset = app.scroll_offset.saturating_sub(1);
                }
                KeyCode::Down if app.scroll_offset + 1 < app.conversation.len() => {
                    app.scroll_offset += 1;
                }
                _ => {}
            }
        }

        if app.should_quit {
            break Ok(());
        }
    };

    // Restore terminal
    let _ = disable_raw_mode();
    let _ = std::io::stdout().execute(LeaveAlternateScreen);

    result
}

fn draw_ui(frame: &mut Frame, app: &mut LocalApp) {
    let area = frame.area();

    // Main layout: conversation (top 80%) | input (bottom 20%)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(80), Constraint::Percentage(20)])
        .split(area);

    // Conversation area
    let conv_block = Block::default()
        .title(" Conversation ")
        .borders(Borders::ALL);
    let inner = conv_block.inner(chunks[0]);

    let mut conv_lines: Vec<Line> = vec![];
    for (role, text) in &app.conversation {
        let role_style = if role == "You" {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::Green)
        };
        for (idx, line_text) in text.lines().enumerate() {
            let prefix = if idx == 0 {
                format!("[{}] ", role)
            } else {
                "     ".to_string() // indent continuation lines
            };
            conv_lines.push(Line::from(vec![
                Span::styled(prefix, role_style),
                Span::raw(line_text.to_string()),
            ]));
        }
        // If text ends with a newline, add an empty continuation line
        if text.ends_with('\n') {
            conv_lines.push(Line::from(vec![
                Span::styled("     ".to_string(), role_style),
                Span::raw("".to_string()),
            ]));
        }
    }

    // Auto-scroll to bottom unless user scrolled up
    let visible_lines = inner.height.saturating_sub(2) as usize;
    let start = if app.scroll_offset + visible_lines >= conv_lines.len() {
        conv_lines.len().saturating_sub(visible_lines)
    } else {
        app.scroll_offset
    };
    let displayed: Vec<Line> = conv_lines.into_iter().skip(start).collect();

    let conv_para = Paragraph::new(displayed)
        .block(conv_block)
        .wrap(Wrap { trim: true });
    frame.render_widget(conv_para, chunks[0]);

    // Input area
    let input_block = Block::default()
        .title(" Input (Ctrl+D=send, Ctrl+C=quit) ")
        .borders(Borders::ALL);
    let input_para = Paragraph::new(app.input_buffer.as_str()).block(input_block);
    frame.render_widget(input_para, chunks[1]);
}
