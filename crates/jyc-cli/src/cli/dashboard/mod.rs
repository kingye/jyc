use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use crossterm::{
    ExecutableCommand,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    prelude::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};
use std::io::stdout;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::Command;

use unicode_width::UnicodeWidthStr;

use jyc_inspect::client::InspectClient;
use jyc_types::{CommandInfo, InspectOverview, ModelInfo, Severity, TopicStatus};

use super::command_popup::*;

mod chat;
mod leader;
mod local_commands;
mod token_render;
mod ws;
use chat::*;
use ws::*;

#[derive(Args, Debug)]
pub struct DashboardArgs {
    /// Inspect server address (also used for WebSocket chat)
    #[arg(long, default_value = "127.0.0.1:9876", global = true)]
    pub addr: String,

    /// Authorization token (defaults to `<workdir>/auth.token`)
    #[arg(long, env = "JYC_DASHBOARD_TOKEN", global = true)]
    pub token: Option<String>,

    /// Subcommand for dashboard operations (defaults to opening the full dashboard)
    #[command(subcommand)]
    pub command: Option<DashboardCommand>,
}

#[derive(Subcommand, Debug)]
pub enum DashboardCommand {
    /// Open a directory as an ad-hoc topic and launch chat mode.
    #[command(name = "open")]
    Open(OpenArgs),
}

/// Arguments for opening a directory as an ad-hoc topic.
///
/// Shared by `jyc dashboard open` and the top-level `jyc open` shortcut.
#[derive(Args, Debug)]
pub struct OpenArgs {
    /// Topic name (defaults to folder name of --path or current directory)
    #[arg(short = 't', long)]
    pub topic: Option<String>,

    /// Websocket channel name (auto-detected if only one exists)
    #[arg(short = 'c', long)]
    pub channel: Option<String>,

    /// Topic working directory (defaults to current directory)
    #[arg(short = 'p', long)]
    pub path: Option<String>,
}

/// Application state for the TUI.
struct App {
    state: Option<InspectOverview>,
    error: Option<String>,
    table_state: TableState,
    should_quit: bool,
    status_message: Option<(String, std::time::Instant)>,
    /// Set by the explorer pane when it switches the chat to a new
    /// topic; the async poll loop picks it up and hydrates the live
    /// buffers so the chat pane shows the new topic's history.
    pending_hydrate: Option<(String, String)>,

    /// Set by the leader `new chat` action; the async poll loop runs the
    /// pattern-select flow (needs InspectClient for `list_patterns`).
    pending_new_chat: bool,

    /// Set by the leader `reload config` action; the async poll loop runs
    /// the reload (needs InspectClient).
    pending_reload_config: bool,

    /// Whether terminal mouse capture is currently enabled. When on, the
    /// chat message area scrolls on wheel events but tmux/terminal-native
    /// text selection is hijacked by the app. When off, tmux select works
    /// but the wheel does nothing. Flipped at runtime by the `toggle
    /// mouse` leader-key popup entry. Default is `true` to match the
    /// behaviour introduced by PR #484 — opt out via leader popup when
    /// working inside tmux.
    mouse_capture_enabled: bool,

    /// Open leader-key popup on the dashboard screen (dashboard + shared
    /// commands). Triggered by `Ctrl+P`.
    leader: Option<leader::Leader>,

    /// Authorization token propagated to the WebSocket upgrade requests.
    token: Option<String>,

    /// WS connection for the currently-selected overview topic.
    /// Live-feeds `live_activity` so the activity pane updates without polling.
    overview_ws_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    overview_ws_rx: tokio::sync::mpsc::UnboundedReceiver<WsEvent>,
    /// (channel, topic) the overview WS is currently scoped to.
    overview_ws_target: Option<(String, String)>,

    /// Chat pane state (WebSocket topic chat for any channel type).
    chat: ChatState,
}

impl App {
    fn new(ws_rx: tokio::sync::mpsc::UnboundedReceiver<WsEvent>, token: Option<String>) -> Self {
        // The overview WS keeps its cmd_tx alive so the spawned task can
        // reconnect on transient errors. cmd_rx is consumed by the task;
        // event_rx is consumed by the app.
        let (overview_ws_cmd_tx, _overview_ws_cmd_rx) =
            tokio::sync::mpsc::unbounded_channel::<String>();
        let (overview_ws_evt_tx, overview_ws_rx) =
            tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let _ = overview_ws_evt_tx; // unused; just for pairing
        Self {
            state: None,
            error: None,
            table_state: TableState::default(),
            should_quit: false,
            status_message: None,
            pending_hydrate: None,
            pending_new_chat: false,
            pending_reload_config: false,
            mouse_capture_enabled: true,
            leader: None,
            token,
            overview_ws_tx: Some(overview_ws_cmd_tx),
            overview_ws_rx,
            overview_ws_target: None,
            chat: ChatState::new(ws_rx),
        }
    }

    fn set_status(&mut self, msg: String) {
        self.status_message = Some((msg, std::time::Instant::now()));
    }

    fn tick_status(&mut self) {
        if let Some((_, at)) = &self.status_message
            && at.elapsed() > Duration::from_secs(5)
        {
            self.status_message = None;
        }
    }

    /// Pure flip — returns the new state without touching the terminal.
    /// Split from the I/O so tests can exercise the toggle without a real
    /// stdout.
    fn flip_mouse_capture(&mut self) -> bool {
        self.mouse_capture_enabled = !self.mouse_capture_enabled;
        self.mouse_capture_enabled
    }

    /// Emit the terminal escape sequence that matches the current
    /// `mouse_capture_enabled` state. Writes to an arbitrary writer so
    /// tests can pass a `Vec<u8>` and assert the emitted bytes; the
    /// thin `apply_mouse_capture` wrapper below topics through stdout.
    /// Crossterm mouse toggles don't go through the ratatui backend, so
    /// we write directly here (matches the `EnableMouseCapture` /
    /// `DisableMouseCapture` usage at startup and shutdown).
    fn apply_mouse_capture_to<W: std::io::Write>(&self, mut out: W) -> std::io::Result<()> {
        use crossterm::ExecutableCommand;
        if self.mouse_capture_enabled {
            out.execute(EnableMouseCapture).map(|_| ())
        } else {
            out.execute(DisableMouseCapture).map(|_| ())
        }
    }

    fn apply_mouse_capture(&self) -> std::io::Result<()> {
        self.apply_mouse_capture_to(std::io::stdout().lock())
    }

    fn next_topic(&mut self) {
        let count = self.state.as_ref().map(|s| s.topics.len()).unwrap_or(0);
        if count == 0 {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => (i + 1) % count,
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    fn prev_topic(&mut self) {
        let count = self.state.as_ref().map(|s| s.topics.len()).unwrap_or(0);
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

    fn handle_ws_event(&mut self, event: WsEvent) {
        match event {
            WsEvent::Connected => {
                self.chat.ws_connected = true;
                // The WS protocol no longer carries `list_patterns` or
                // `subscribe` commands — history is loaded via REST and
                // topic scope comes from the URL. Nothing to do here.
            }
            WsEvent::Disconnected => {
                self.chat.ws_connected = false;
                self.set_status("WebSocket disconnected".to_string());
            }
            WsEvent::Message(text) => {
                self.chat.handle_ws_message(&text);
            }
            WsEvent::Error(err) => {
                self.set_status(format!("WebSocket error: {err}"));
            }
        }
    }
}

/// Auto-spawn `jyc serve` when it's not running, once per dashboard process.
///
/// Writes `serve` logs to `<data_home>/jyc.log` so the user can review
/// diagnostics. Only works for localhost addresses (the default).
async fn ensure_serve_running(addr: &str, workdir: &std::path::Path) -> Result<()> {
    // Only try to spawn once per dashboard session.
    static SPAWNED: AtomicBool = AtomicBool::new(false);

    // Quick check: server already up?
    if TcpStream::connect(addr).await.is_ok() {
        return Ok(());
    }
    if SPAWNED.swap(true, Ordering::SeqCst) {
        // Already tried spawning — server is still down, fail.
        anyhow::bail!("Could not connect to {addr}; jyc serve already attempted to start");
    }

    // Only auto-spawn for localhost addresses.
    let is_local = addr.starts_with("127.0.0.1")
        || addr.starts_with("localhost")
        || addr.starts_with("::1")
        || addr.starts_with("[::1]");
    if !is_local {
        anyhow::bail!("Could not connect to {addr}. Start jyc serve manually.");
    }

    // Determine log file path.
    tokio::fs::create_dir_all(workdir)
        .await
        .with_context(|| format!("Failed to create workdir {}", workdir.display()))?;
    let log_path = workdir.join("jyc.log");

    // Open log file (create / truncate).
    let log_file = std::fs::File::create(&log_path)
        .with_context(|| format!("Failed to create log file {}", log_path.display()))?;
    let log_dup = log_file
        .try_clone()
        .context("Failed to clone log file handle")?;

    // Spawn jyc serve as a background child process. Pass --workdir so the
    // auth token file is written to the same location the dashboard reads.
    // Skip --workdir when it's the platform default (data_home) so the
    // spawned serve uses the standard first-run provisioning path.
    let exe = std::env::current_exe().context("Could not determine jyc binary path")?;
    let default_workdir = jyc_utils::paths::data_home().unwrap_or_default();
    let mut cmd = Command::new(&exe);
    cmd.arg("serve");
    if workdir != default_workdir {
        cmd.arg("--workdir").arg(workdir);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(log_dup)
        .stderr(log_file)
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn {} serve", exe.display()))?;

    // Poll for readiness (up to SERVE_STARTUP_TIMEOUT).
    const SERVE_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
    let deadline = tokio::time::Instant::now() + SERVE_STARTUP_TIMEOUT;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if TcpStream::connect(addr).await.is_ok() {
            tracing::info!("Auto-started jyc serve (pid={})", child.id().unwrap_or(0));
            std::mem::forget(child); // Detach — serve runs until terminated separately.
            return Ok(());
        }
        // Check if child exited early (e.g. first-run provisioning, config error).
        if let Ok(Some(status)) = child.try_wait() {
            let log_content = read_log_tail(&log_path, 20).await;
            tokio::fs::remove_file(&log_path).await.ok();
            if !log_content.is_empty() {
                eprintln!("--- jyc serve log ---\n{log_content}\n--- end of log ---");
            }
            if status.success() {
                anyhow::bail!(
                    "jyc serve started but exited after configuration. \
                     Edit the file and try again."
                );
            }
            anyhow::bail!(
                "jyc serve exited with status {status}. See log: {}",
                log_path.display()
            );
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }

    // Timeout — print diagnostics.
    let log_content = read_log_tail(&log_path, 20).await;
    if !log_content.is_empty() {
        eprintln!("--- jyc serve log tail ---\n{log_content}");
    }
    anyhow::bail!(
        "jyc serve did not start in time. See full log: {}",
        log_path.display()
    );
}

/// Read the last `n` lines from a log file (stripping ANSI codes).
async fn read_log_tail(path: &std::path::Path, n: usize) -> String {
    let mut content = String::new();
    let mut f = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let _ = f.read_to_string(&mut content).await;
    let clean = content.replace("\x1b[", "");
    let lines: Vec<&str> = clean.lines().collect();
    let tail = if lines.len() > n {
        &lines[lines.len() - n..]
    } else {
        &lines
    };
    tail.join("\n")
}

/// How long a lone `Esc` is held while we wait to see whether it is the
/// head of a split mouse escape sequence.
const ESC_FRAGMENT_WINDOW: Duration = Duration::from_millis(20);

/// Outcome of feeding one key through [`MouseFragmentFilter`].
enum Filtered {
    /// Dispatch the key normally.
    Dispatch(KeyEvent),
    /// A held `Esc` turned out to be a real keypress: dispatch it first,
    /// then the current key.
    Replay(KeyEvent, KeyEvent),
    /// The key is part of a mouse escape fragment — drop it.
    Swallow,
}

/// Swallows mouse escape fragments (`[<65;35;12M`) that leak through
/// crossterm as plain character keys.
///
/// crossterm 0.29 assumes more input is pending only when a read fills
/// its whole 1024-byte buffer (`more = read_count == TTY_BUFFER_SIZE`).
/// During a fast wheel burst the pty chunks the byte stream arbitrarily;
/// when a chunk ends exactly at ESC, the parser emits a lone `Esc` key
/// and the remaining `[<65;35;12M` bytes arrive as plain `Char` keys,
/// which the composer would insert as garbage. A real `Esc` press is
/// held for [`ESC_FRAGMENT_WINDOW`] and replayed unless `[` follows
/// immediately — humans cannot type that fast, and pastes arrive as
/// `Event::Paste`, so there are no false positives.
// ponytail: local workaround for crossterm's incomplete-read heuristic
// (more = read_count == TTY_BUFFER_SIZE); delete this filter if crossterm
// ever buffers partial escape sequences across reads.
#[derive(Default)]
struct MouseFragmentFilter {
    /// A lone `Esc` held for the fragment window, with its arrival time.
    pending_esc: Option<(KeyEvent, std::time::Instant)>,
    /// Whether we are inside a fragment body (after `ESC` `[`).
    swallowing: bool,
}

impl MouseFragmentFilter {
    fn feed(&mut self, key: KeyEvent, now: std::time::Instant) -> Filtered {
        if self.swallowing {
            return match key.code {
                // Fragment body: parameters, separators, SGR marker.
                KeyCode::Char(c) if c.is_ascii_digit() || c == ';' || c == '<' => Filtered::Swallow,
                // Final byte: M (press/scroll) or m (release).
                KeyCode::Char('M') | KeyCode::Char('m') => {
                    self.swallowing = false;
                    Filtered::Swallow
                }
                // Not a fragment after all — stop filtering. Characters
                // already swallowed are lost; accepted trade-off inside a
                // 20ms window after Esc.
                _ => {
                    self.swallowing = false;
                    Filtered::Dispatch(key)
                }
            };
        }
        if let Some((esc, at)) = self.pending_esc.take() {
            if now.duration_since(at) <= ESC_FRAGMENT_WINDOW
                && key.code == KeyCode::Char('[')
                && key.modifiers.is_empty()
            {
                self.swallowing = true;
                return Filtered::Swallow;
            }
            return Filtered::Replay(esc, key);
        }
        if key.code == KeyCode::Esc {
            self.pending_esc = Some((key, now));
            return Filtered::Swallow;
        }
        Filtered::Dispatch(key)
    }

    /// Flush a held `Esc` whose fragment window has expired (a real
    /// `Esc` press). Call once per event-loop iteration; the loop's 50ms
    /// poll cadence guarantees it runs.
    fn flush(&mut self, now: std::time::Instant) -> Option<KeyEvent> {
        match self.pending_esc {
            Some((esc, at)) if now.duration_since(at) > ESC_FRAGMENT_WINDOW => {
                self.pending_esc = None;
                Some(esc)
            }
            _ => None,
        }
    }
}

pub async fn run(
    args: &DashboardArgs,
    workdir: &std::path::Path,
    initial_topic: Option<&str>,
    initial_channel: Option<&str>,
) -> Result<()> {
    // Auto-spawn jyc serve FIRST - it writes <workdir>/auth.token on
    // startup, so the token file exists by the time we resolve it.
    ensure_serve_running(&args.addr, workdir)
        .await
        .with_context(|| {
            format!(
                "Failed to connect to {}. Start jyc serve manually.",
                args.addr
            )
        })?;

    // Resolve the auth token: explicit flag/env wins, otherwise read it
    // from `<workdir>/auth.token` (which `jyc serve` writes on startup).
    let token = resolve_dashboard_token(args.token.as_deref(), workdir)?;

    let client = match &token {
        Some(t) => InspectClient::with_token(&args.addr, Some(t.as_str())),
        None => InspectClient::new(&args.addr),
    };

    // Setup terminal
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(EnableBracketedPaste)?;
    // Enable mouse capture so the chat pane can scroll on wheel events.
    // Without this, crossterm swallows mouse events at the terminal level.
    stdout().execute(EnableMouseCapture)?;

    // Restore the terminal on panic. Without this a crash leaves raw mode
    // and mouse capture enabled, and wheel scrolling at the shell prompt
    // sprays mouse escape sequences into readline. Best-effort: ignore
    // errors, then defer to the default hook for the panic message.
    // (Not restored on normal exit — the process ends after `run`.)
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = stdout().execute(DisableBracketedPaste);
        let _ = stdout().execute(DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = stdout().execute(LeaveAlternateScreen);
        default_hook(info);
    }));

    // Terminal and its backend are scoped so they drop *before* we restore
    // the terminal. Otherwise the backend's Drop flushes buffered escape
    // codes after LeaveAlternateScreen, corrupting line alignment.
    let result = {
        let backend = CrosstermBackend::new(stdout());
        let mut terminal = Terminal::new(backend)?;

        let (_, ws_rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(ws_rx, token);
        let mut mouse_frag_filter = MouseFragmentFilter::default();
        let poll_interval = Duration::from_millis(500);
        let mut last_poll = std::time::Instant::now() - poll_interval; // Force immediate poll
        // Overview fetches run off-loop: each poll is spawned and its
        // result arrives on this channel, so the HTTP round-trip never
        // blocks keystroke handling. (The old inline `await` froze input
        // for one RTT twice per second.) At most one poll is in flight,
        // so results cannot arrive out of order.
        let (poll_tx, mut poll_rx) =
            tokio::sync::mpsc::unbounded_channel::<Result<InspectOverview>>();
        let mut poll_in_flight = false;

        // If a topic was requested on the CLI, open chat directly.
        if let Some(topic) = initial_topic {
            let channel = initial_channel.unwrap_or("");
            app.chat
                .open(&args.addr, initial_channel, Some(topic), app.token.clone());
            hydrate_live(&client, &mut app, channel, topic).await;
        }

        loop {
            // Poll for new state (slim overview — no activity/messages/thinking).
            // Spawn the fetch off-loop; handle the result below when it arrives.
            if !poll_in_flight && last_poll.elapsed() >= poll_interval {
                poll_in_flight = true;
                let client = client.clone();
                let tx = poll_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(client.get_overview().await);
                });
            }
            if let Ok(result) = poll_rx.try_recv() {
                poll_in_flight = false;
                last_poll = std::time::Instant::now();
                match result {
                    Ok(overview) => {
                        // Clear awaiting_response once the server confirms the topic
                        // is no longer processing (with a small grace period to avoid
                        // flicker between the local flag and server state).
                        if app.chat.awaiting_response
                            && let Some(ref chat_name) = app.chat.topic
                        {
                            let ct = overview.topics.iter().find(|t| t.name == *chat_name);
                            if let Some(ct) = ct
                                && ct.status != TopicStatus::Processing
                            {
                                app.chat.awaiting_response = false;
                            }
                        }

                        // Append new chat messages from the live buffer to the
                        // dashboard's `chat.messages` vec. The live buffer is
                        // populated by REST hydrate on selection and updated by
                        // WS `chat_message` events.
                        if let Some(channel) = app.chat.channel.as_deref()
                            && let Some(topic) = app.chat.topic.as_deref()
                        {
                            // Clone to release the borrows on
                            // `app.chat.channel` / `.topic` before the
                            // mutable call into `poll_sync_live_chat`.
                            let channel = channel.to_string();
                            let topic = topic.to_string();
                            if app.chat.poll_sync_live_chat(&channel, &topic) {
                                app.chat.scroll = 0;
                            }
                        }

                        app.state = Some(overview);
                        if let Some(ref s) = app.state {
                            // Auto-select the first topic on initial load so the
                            // activity pane is populated immediately via the existing
                            // hydrate + ensure_overview_ws paths. The user can still
                            // navigate to a different topic (↑/↓).
                            if app.table_state.selected().is_none() && !s.topics.is_empty() {
                                app.table_state.select(Some(0));
                            }
                            app.chat.commands = s.commands.clone();
                            app.chat.models = s.models.clone();

                            // Hydrate the live buffers when the table-selected
                            // topic changes (so the overview's activity pane
                            // shows recent entries without requiring the user
                            // to open chat first). Skip if we're in chat mode
                            // — the open() flow already triggered hydrate.
                            if !app.chat.visible {
                                let selected = app
                                    .table_state
                                    .selected()
                                    .and_then(|idx| s.topics.get(idx))
                                    .map(|t| (t.channel.clone(), t.name.clone()));
                                if let Some((channel, topic)) = selected {
                                    let key = (channel.clone(), topic.clone());
                                    let needs_hydrate =
                                        app.chat.last_hydrated_key.as_ref() != Some(&key);
                                    if needs_hydrate {
                                        app.chat.last_hydrated_key = Some(key);
                                    }
                                    let _ = s; // drop the immutable borrow on app.state
                                    if needs_hydrate {
                                        hydrate_live(&client, &mut app, &channel, &topic).await;
                                    }
                                    ensure_overview_ws(&mut app, &args.addr, &channel, &topic);
                                }
                            }
                        }
                        app.error = None;
                    }
                    Err(e) => {
                        app.error = Some(format!("{e:#}"));
                    }
                }
            }

            // Hydrate the new topic's history when the explorer pane
            // switches the chat. Deferred here because the sync key
            // handler can't await on InspectClient.
            if let Some((channel, topic)) = app.pending_hydrate.take() {
                hydrate_live(&client, &mut app, &channel, &topic).await;
            }

            // Leader actions deferred for the same reason.
            if app.pending_new_chat {
                app.pending_new_chat = false;
                start_new_chat(&mut app, &args.addr, &client).await;
            }
            if app.pending_reload_config {
                app.pending_reload_config = false;
                reload_server_config(&mut app, &client, &mut last_poll).await;
            }

            // Check for WebSocket events
            while let Ok(event) = app.chat.ws_rx.try_recv() {
                app.handle_ws_event(event);
            }
            // Overview WS events (live activity feed in overview mode).
            while let Ok(event) = app.overview_ws_rx.try_recv() {
                app.handle_ws_event(event);
            }

            // Clear expired status messages
            app.tick_status();

            // Dispatch a held `Esc` whose fragment window has expired —
            // it was a real keypress, not a split mouse sequence.
            if let Some(esc) = mouse_frag_filter.flush(std::time::Instant::now()) {
                if app.chat.visible {
                    handle_chat_keys(&mut app, esc, &mut terminal);
                } else {
                    handle_normal_keys(&mut app, esc, &client, &mut last_poll, &args.addr).await;
                }
            }

            // Draw
            terminal.draw(|f| ui(f, &mut app))?;

            // Handle input: block up to 50ms for the first event, then
            // drain everything already pending. One draw per burst (above)
            // instead of one per event — otherwise wheel gestures queue up
            // and keep replaying after the user reverses direction.
            if event::poll(Duration::from_millis(50))? {
                // Cap per frame: a continuous event flood (e.g. any-event
                // motion tracking) must not starve redraws.
                for _ in 0..64 {
                    match event::read()? {
                        Event::Paste(data)
                            if app.chat.visible && app.chat.focus == ChatFocus::ChatPane =>
                        {
                            // Bracketed paste delivers the whole pasted chunk as
                            // one event. Forward it to the editor so it never
                            // triggers Enter handling / send.
                            app.chat.editor.insert_str(data);
                        }
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            let (first, second) =
                                match mouse_frag_filter.feed(key, std::time::Instant::now()) {
                                    Filtered::Dispatch(k) => (Some(k), None),
                                    Filtered::Replay(esc, k) => (Some(esc), Some(k)),
                                    Filtered::Swallow => (None, None),
                                };
                            for key in [first, second].into_iter().flatten() {
                                if app.chat.visible {
                                    handle_chat_keys(&mut app, key, &mut terminal);
                                } else {
                                    handle_normal_keys(
                                        &mut app,
                                        key,
                                        &client,
                                        &mut last_poll,
                                        &args.addr,
                                    )
                                    .await;
                                }
                            }
                        }
                        Event::Mouse(mouse) if app.chat.visible => {
                            handle_chat_mouse(&mut app, mouse);
                        }
                        _ => {}
                    }
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }

            if app.should_quit {
                break Ok(());
            }
        }
    }; // terminal + backend dropped here

    // Restore terminal — safe now that no buffered escape codes remain
    stdout().execute(DisableBracketedPaste)?;
    stdout().execute(DisableMouseCapture)?;
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    result
}

/// Open a directory as an ad-hoc websocket topic and launch chat mode.
///
/// Resolves the topic name (from explicit `-t` or the folder name of `-p`),
/// the websocket channel (explicit `-c` or auto-detected when only one
/// exists), and the absolute topic path. Sends a `create_topic` message
/// over the websocket, waits for the inspect server to report the topic,
/// then opens the dashboard with chat already focused on the topic.
///
/// The target directory may be brand new or already contain a `.jyc`
/// subdirectory; in either case the path is registered as the topic's
/// working directory.
pub async fn run_open(
    addr: &str,
    workdir: &std::path::Path,
    topic: Option<&str>,
    channel: Option<&str>,
    path: Option<&str>,
    explicit_token: Option<&str>,
) -> Result<()> {
    // Auto-spawn jyc serve FIRST (writes <workdir>/auth.token on startup).
    ensure_serve_running(addr, workdir)
        .await
        .with_context(|| format!("Failed to connect to {addr}. Start jyc serve manually."))?;

    let token = resolve_dashboard_token(explicit_token, workdir)?;

    // Resolve topic path and name
    let path = resolve_topic_path(path)?;
    let topic = derive_topic_name(&path, topic);

    // If the directory was previously opened as a topic, the topic-name file
    // records the canonical name. Refuse to re-open it under a different name
    // to avoid diverging history and storage paths.
    check_existing_topic_name(&path, &topic)?;

    // Resolve websocket channel using inspect state
    let client = match &token {
        Some(t) => InspectClient::with_token(addr, Some(t.as_str())),
        None => InspectClient::new(addr),
    };
    let channel = resolve_websocket_channel(&client, channel).await?;

    tracing::info!(
        topic = %topic,
        channel = %channel,
        path = %path,
        "Opening directory as ad-hoc topic via dashboard CLI"
    );

    // Register the ad-hoc topic via REST. Replaces the old WebSocket
    // `create_topic` command.
    match client.create_topic(&channel, &topic, &path).await {
        Ok((true, msg)) => {
            tracing::debug!(message = %msg, "Topic created via REST");
        }
        Ok((false, msg)) => {
            anyhow::bail!("create_topic failed: {msg}");
        }
        Err(e) => {
            anyhow::bail!("create_topic error: {e:#}");
        }
    }

    // Wait for the inspect server to report the topic
    wait_for_topic(&client, &topic, &channel).await?;

    // Open dashboard directly in chat mode for the topic
    run(
        &DashboardArgs {
            addr: addr.to_string(),
            token,
            command: None,
        },
        workdir,
        Some(&topic),
        Some(&channel),
    )
    .await
}

/// Resolve the dashboard authorization token from explicit input or workdir.
fn resolve_dashboard_token(
    explicit: Option<&str>,
    workdir: &std::path::Path,
) -> Result<Option<String>> {
    if let Some(token) = explicit {
        return Ok(Some(token.to_string()));
    }
    let path = jyc_utils::auth_token::token_path(workdir);
    match jyc_utils::auth_token::read_token(&path) {
        Ok(token) => Ok(Some(token)),
        Err(e) => {
            // File-not-found is the common case (server not yet started).
            // Other errors (corrupted file, permission denied) are worth logging.
            if path.exists() {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to read authorization token; dashboard will connect without auth"
                );
            }
            Ok(None)
        }
    }
}

/// Resolve the topic path to an absolute filesystem path.
///
/// Expands a leading `~` to `$HOME`. Relative paths are resolved against the
/// current working directory. If the path exists, it is canonicalized; otherwise
/// the absolute path is returned as-is so that new directories can be created
/// later by the storage layer.
fn resolve_topic_path(path: Option<&str>) -> Result<String> {
    let path = path.unwrap_or(".");
    let expanded = if let Some(stripped) = path.strip_prefix("~") {
        dirs_home()
            .ok_or_else(|| anyhow::anyhow!("HOME not set, cannot expand ~"))?
            .join(stripped)
    } else {
        PathBuf::from(path)
    };

    let abs = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    };

    // Canonicalize when possible; otherwise use the absolute path as-is.
    let abs = std::fs::canonicalize(&abs).unwrap_or(abs);
    Ok(abs.to_string_lossy().to_string())
}

/// Resolve HOME directory.
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Derive the topic name from explicit input or the folder name of the path.
fn derive_topic_name(path: &str, topic: Option<&str>) -> String {
    if let Some(name) = topic {
        return name.to_string();
    }
    PathBuf::from(path)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "adhoc".to_string())
}

/// Verify that the directory has not already been registered under a
/// different topic name.
///
/// If `<path>/.jyc/topic-name` exists and contains a non-empty name that
/// differs from `topic`, returns an error to prevent diverging history and
/// storage paths.
fn check_existing_topic_name(path: &str, topic: &str) -> Result<()> {
    let jyc_dir = PathBuf::from(path).join(".jyc");
    // One-time migration for the topic → topic rename.
    jyc_core::topic_path::migrate_topic_name_file(&jyc_dir);
    let topic_name_file = jyc_dir.join("topic-name");
    if topic_name_file.exists() {
        let existing = std::fs::read_to_string(&topic_name_file)
            .with_context(|| format!("failed to read {}", topic_name_file.display()))?;
        let existing = existing.trim();
        if !existing.is_empty() && existing != topic {
            anyhow::bail!(
                "directory '{}' is already registered as topic '{}'; \
                 cannot open as '{}'. Use 'jyc open -t {} -p {}' instead",
                path,
                existing,
                topic,
                existing,
                path
            );
        }
    }
    Ok(())
}

/// Resolve the websocket channel name.
///
/// If the user explicitly provided `-c`, use it. Otherwise query the inspect
/// server and auto-select when exactly one websocket channel exists.
async fn resolve_websocket_channel(
    client: &InspectClient,
    channel: Option<&str>,
) -> Result<String> {
    if let Some(name) = channel {
        return Ok(name.to_string());
    }

    let overview = client.get_overview().await?;
    let ws_channels: Vec<String> = overview
        .channels
        .into_iter()
        .filter(|c| c.channel_type == "websocket")
        .map(|c| c.name)
        .collect();

    match ws_channels.len() {
        0 => anyhow::bail!(
            "No agent / websocket channel configured. Add a [agents.<name>] \
             (preferred) or [channels.<name>] type=\"websocket\" (deprecated) to config.toml."
        ),
        1 => Ok(ws_channels.into_iter().next().unwrap()),
        _ => anyhow::bail!(
            "Multiple websocket channels found: {:?}. Use --channel (-c) to specify one.",
            ws_channels
        ),
    }
}

/// Poll the inspect server until the newly created topic appears in state.
async fn wait_for_topic(client: &InspectClient, topic: &str, channel: &str) -> Result<()> {
    for _ in 0..50 {
        let overview = client.get_overview().await?;
        if overview
            .topics
            .iter()
            .any(|t| t.name == topic && t.channel == channel)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("Timeout waiting for topic {topic} to be created")
}

/// REST hydrate the live activity + chat buffers for the given topic.
///
/// Called after the user opens a chat (Enter on a topic, `c` to start fresh,
/// or `--topic` on the CLI). Subsequent live updates arrive over the
/// WebSocket and are appended to the same buffers; the activity pane and
/// chat progress read exclusively from these buffers.
///
/// Errors are logged but not propagated — the buffers will simply be empty
/// (the activity pane shows "No activity" until the next WS event arrives).
async fn hydrate_live(client: &InspectClient, app: &mut App, channel: &str, topic: &str) {
    // Drop stale WS-fed processing/thinking state for this topic (it may
    // have changed while unwatched); the renderer falls back to the polled
    // overview status until fresh WS events arrive.
    app.chat.clear_live_transient(channel, topic);
    // Activity first (chat pane progress depends on it).
    match client
        .get_topic_activity(channel, topic, None, Some(180))
        .await
    {
        Ok(activity) => {
            // Chat second.
            match client.get_topic_chat(channel, topic, None, Some(100)).await {
                Ok(chat) => {
                    app.chat.seed_live(channel, topic, activity, chat);
                }
                Err(e) => {
                    tracing::warn!(
                        channel = %channel,
                        topic = %topic,
                        error = %e,
                        "failed to hydrate chat history (activity pane may be empty)"
                    );
                    // Still seed activity so the activity pane at least has entries.
                    app.chat.seed_live(channel, topic, activity, Vec::new());
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                channel = %channel,
                topic = %topic,
                error = %e,
                "failed to hydrate activity (activity pane will be empty until WS events arrive)"
            );
        }
    }
}

/// Start a new chat: fetch patterns via REST and open the chat screen in
/// pattern-select mode. Used by the `c` key and the leader `new chat`
/// action (via `pending_new_chat`).
async fn start_new_chat(app: &mut App, addr: &str, client: &InspectClient) {
    let channel = app.state.as_ref().and_then(|o| {
        o.channels
            .iter()
            .find(|c| c.channel_type == "websocket")
            .map(|c| c.name.clone())
    });
    if let Some(channel) = channel {
        app.chat
            .open_pattern_select(addr, &channel, client, app.token.clone())
            .await;
    } else {
        app.set_status("No websocket channel configured".to_string());
    }
}

/// Open the chat screen for the table-selected topic. All channel types
/// use the unified `/ws/<channel>/<topic>` endpoint. Used by the Enter key
/// and the leader `open chat` action.
async fn open_selected_topic_chat(app: &mut App, client: &InspectClient, addr: &str) {
    let topic_info = app.state.as_ref().and_then(|s| {
        app.table_state
            .selected()
            .and_then(|i| s.topics.get(i))
            .map(|t| (t.name.clone(), t.channel.clone()))
    });
    if let Some((name, channel)) = topic_info {
        app.chat
            .open(addr, Some(&channel), Some(&name), app.token.clone());
        // Chat WS takes over live events. Close the overview WS
        // so we don't have two connections to the same topic.
        close_overview_ws(app);
        // REST hydrate the live buffers (activity + chat) so the
        // activity pane and chat progress show recent entries
        // immediately. WS events append to the same buffers.
        hydrate_live(client, app, &channel, &name).await;
    }
}

/// Reload the server configuration. Used by the `R` key and the leader
/// `reload config` action (via `pending_reload_config`).
async fn reload_server_config(
    app: &mut App,
    client: &InspectClient,
    last_poll: &mut std::time::Instant,
) {
    match client.reload_config().await {
        Ok((true, msg)) => {
            app.set_status(format!("Config reloaded: {msg}"));
            *last_poll = std::time::Instant::now() - Duration::from_millis(500);
        }
        Ok((false, msg)) => {
            app.set_status(format!("Reload failed: {msg}"));
        }
        Err(e) => {
            app.set_status(format!("Reload error: {e:#}"));
        }
    }
}

async fn handle_normal_keys(
    app: &mut App,
    key: event::KeyEvent,
    client: &InspectClient,
    last_poll: &mut std::time::Instant,
    addr: &str,
) {
    // ^Q quits the entire dashboard (consistent across all modes)
    if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }

    // Leader open: delegate all keys to it and dispatch the chosen action.
    if let Some(ref mut leader) = app.leader {
        match leader.handle_key(key) {
            leader::LeaderResult::Consumed => {}
            leader::LeaderResult::Closed => app.leader = None,
            leader::LeaderResult::Action(action) => {
                app.leader = None;
                use local_commands::LocalAction;
                match action {
                    LocalAction::OpenChat => open_selected_topic_chat(app, client, addr).await,
                    LocalAction::NewChat => start_new_chat(app, addr, client).await,
                    LocalAction::ReloadConfig => reload_server_config(app, client, last_poll).await,
                    LocalAction::Quit => app.should_quit = true,
                    LocalAction::ToggleMouseCapture => toggle_mouse_capture(app),
                    // Chat-scoped actions are never offered on the dashboard.
                    _ => {}
                }
            }
        }
        return;
    }

    // Ctrl+P opens the leader popup (dashboard + shared commands).
    let is_ctrl_p = key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_p {
        app.leader = Some(leader::Leader::new(local_commands::CommandScope::Dashboard));
        return;
    }

    match key.code {
        KeyCode::Char('c') => {
            // After the user picks a pattern, `select_pattern` opens a
            // scoped WS to `/ws/<channel>/<topic>`.
            start_new_chat(app, addr, client).await;
        }
        KeyCode::Enter => {
            open_selected_topic_chat(app, client, addr).await;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.next_topic();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.prev_topic();
        }
        KeyCode::Char('R') => {
            reload_server_config(app, client, last_poll).await;
        }
        _ => {}
    }
}

/// Flip the terminal mouse-capture mode (used by the `toggle mouse`
/// leader-key action on both dashboard and chat screens). Splits the pure
/// state flip from the terminal I/O so the rollback path can restore the
/// previous state if the escape write fails.
fn toggle_mouse_capture(app: &mut App) {
    let on = app.flip_mouse_capture();
    if let Err(e) = app.apply_mouse_capture() {
        // Roll back the flip so `mouse_capture_enabled` matches the
        // terminal's actual mode.
        app.mouse_capture_enabled = !on;
        app.set_status(format!("Mouse capture toggle failed: {e}"));
    } else {
        app.set_status(if on {
            "Mouse capture: on".to_string()
        } else {
            "Mouse capture: off".to_string()
        });
    }
}

fn ui(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    if app.chat.visible {
        ui_chat_mode(frame, area, app);
    } else {
        ui_normal_mode(frame, area, app);
    }
}

fn ui_normal_mode(frame: &mut Frame, area: Rect, app: &mut App) {
    // Main layout: channels bar | topics table | detail panel | status bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),      // Channels bar
            Constraint::Percentage(40), // Topics table
            Constraint::Percentage(60), // Detail panel + activity log
            Constraint::Length(1),      // Status bar
        ])
        .split(area);

    render_channels(frame, chunks[0], app);
    render_topics(frame, chunks[1], app);
    render_details(frame, chunks[2], app);
    render_status_bar(frame, chunks[3], app);

    // Leader-key popup overlay (dashboard + shared commands).
    if let Some(ref leader) = app.leader {
        leader.render(frame, area);
    }
}

fn render_channels(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().title(" Channels ").borders(Borders::ALL);

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
            let free = ch.max_concurrent.saturating_sub(ch.active_workers);
            let dot_color = if free == 0 {
                Color::Red
            } else if free < ch.max_concurrent {
                Color::Yellow
            } else {
                Color::Green
            };
            parts.push(Span::styled("●", Style::default().fg(dot_color)));
            parts.push(Span::raw(format!(
                " {} ({} {}/{})",
                ch.name, ch.channel_type, free, ch.max_concurrent
            )));
            parts
        })
        .collect();

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let channels_para = Paragraph::new(Line::from(spans));
    frame.render_widget(channels_para, inner);
}

fn render_topics(frame: &mut Frame, area: Rect, app: &mut App) {
    let state = match &app.state {
        Some(s) => s,
        None => {
            let block = Block::default().title(" Topics ").borders(Borders::ALL);
            frame.render_widget(block, area);
            return;
        }
    };

    let header = Row::new(vec![
        Cell::from("Topic"),
        Cell::from("Channel"),
        Cell::from("Pattern"),
        Cell::from("Status"),
        Cell::from("Context"),
        Cell::from("Last Active"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD))
    .height(1);

    let rows: Vec<Row> = state
        .topics
        .iter()
        .map(|t| {
            let status_style = match t.status {
                TopicStatus::Processing => Style::default().fg(Color::Green),
                TopicStatus::Queued => Style::default().fg(Color::Yellow),
                TopicStatus::WaitingForAnswer => Style::default().fg(Color::Cyan),
                TopicStatus::Idle => Style::default().fg(Color::DarkGray),
                TopicStatus::Error => Style::default().fg(Color::Red),
            };

            let tokens = match (t.context_input_tokens, t.max_tokens) {
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
                Cell::from(format_last_active(t.last_active_at.as_deref())),
            ])
        })
        .collect();

    let widths = [
        Constraint::Min(20),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(12),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .title(format!(" Topics ({}) ", state.topics.len()))
                .borders(Borders::ALL),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    frame.render_stateful_widget(table, area, &mut app.table_state);
}

fn render_details(frame: &mut Frame, area: Rect, app: &App) {
    let state = match &app.state {
        Some(s) => s,
        None => {
            let block = Block::default().title(" Details ").borders(Borders::ALL);
            frame.render_widget(block, area);
            return;
        }
    };

    let selected = app.table_state.selected().and_then(|i| state.topics.get(i));

    let selected = match selected {
        Some(t) => t,
        None => {
            let block = Block::default().title(" Details ").borders(Borders::ALL);
            let text = Paragraph::new("Select a topic with ↑/↓").block(block);
            frame.render_widget(text, area);
            return;
        }
    };

    // Split detail area: info (4 lines) + activity log (remaining)
    let detail_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9), // Topic info (Branch row conditionally added)
            Constraint::Min(4),    // Activity log
        ])
        .split(area);

    // Topic info panel, fully enclosed: TOP closes the gap against the topics
    // table above, LEFT/RIGHT frame the sides, BOTTOM closes the pane off from
    // the activity panel below.
    let info_block = Block::default()
        .title(format!(" {} ", selected.name))
        .borders(Borders::ALL);

    let mut info_lines = vec![];

    // Branch is resolved server-side and shipped on TopicSummary.branch.
    // Render only when present — most chat-channel topics (feishu/wecom)
    // have a topic_path that isn't a git repo, so an absent row keeps
    // noise down for them.
    if let Some(branch) = selected.branch.as_deref() {
        info_lines.push(Line::from(vec![
            Span::styled("Branch: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(branch),
        ]));
    }

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

    // Skills line
    if selected.skills.is_empty() {
        info_lines.push(Line::from(vec![
            Span::styled("Skills: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("(none)", Style::default().fg(Color::DarkGray)),
        ]));
    } else {
        info_lines.push(Line::from(vec![
            Span::styled(
                format!("Skills ({}): ", selected.skills.len()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(selected.skills.join(", ")),
        ]));
    }

    let mut status_line = vec![
        Span::styled("Status: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("{}", selected.status),
            match selected.status {
                TopicStatus::Processing => Style::default().fg(Color::Green),
                TopicStatus::Queued => Style::default().fg(Color::Yellow),
                TopicStatus::WaitingForAnswer => Style::default().fg(Color::Cyan),
                TopicStatus::Idle => Style::default().fg(Color::DarkGray),
                TopicStatus::Error => Style::default().fg(Color::Red),
            },
        ),
    ];
    // Live-duration ticker. While the agent loop is alive, append a
    // yellow italic `(12.4s)` suffix to the status chip so the dashboard
    // shows wall-clock elapsed time even during silent LLM/tool work.
    if selected.status == TopicStatus::Processing
        && let Some(ms) = app.chat.live_tick_ms_for(&selected.channel, &selected.name)
    {
        status_line.push(Span::styled(
            format!(" ({})", chat::format_elapsed_ms(ms)),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::ITALIC),
        ));
    }

    // "Tokens: X / Y (Z%)" — shared with the chat info pane via the
    // `token_render` module. Prepend a 2-space gap so it doesn't sit
    // flush against the status chip on the same line.
    if let (Some(_), Some(_)) = (selected.context_input_tokens, selected.max_tokens) {
        status_line.push(Span::raw(token_render::STATUS_SEP));
        token_render::push_tokens_span(&mut status_line, selected);
    }
    if selected.output_tokens.is_some() {
        status_line.push(Span::raw(token_render::STATUS_SEP));
        token_render::push_output_span(&mut status_line, selected);
    }
    if selected.total_input_tokens.is_some() {
        status_line.push(Span::raw(token_render::STATUS_SEP));
        token_render::push_total_input_span(&mut status_line, selected);
    }
    if selected.total_cache_hit_tokens.is_some() {
        status_line.push(Span::raw(token_render::STATUS_SEP));
        token_render::push_cache_hit_span(&mut status_line, selected);
    }
    if selected.total_cache_creation_tokens.is_some() {
        status_line.push(Span::raw(token_render::STATUS_SEP));
        token_render::push_cache_creation_span(&mut status_line, selected);
    }
    if selected.cost.is_some() {
        status_line.push(Span::raw(token_render::STATUS_SEP));
        token_render::push_cost_span(&mut status_line, selected);
    }
    info_lines.push(Line::from(status_line));

    info_lines.push(Line::from(vec![
        Span::styled(
            "Last Active: ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format_last_active(selected.last_active_at.as_deref())),
    ]));

    let info = Paragraph::new(info_lines).block(info_block);
    frame.render_widget(info, detail_chunks[0]);

    // Activity log panel — read from the WS-fed live buffer for this topic.
    // LEFT/RIGHT continue the info pane's side borders; TOP sits under the
    // info pane's BOTTOM border.
    let activity_vec: Vec<jyc_types::ActivityEntry> = app
        .chat
        .live_activity_for(&selected.channel, &selected.name)
        .cloned()
        .collect();
    render_activity_log_inner(
        frame,
        detail_chunks[1],
        &activity_vec,
        0,
        0,
        false,
        Borders::TOP | Borders::LEFT | Borders::RIGHT,
        true,
    );
}

/// Open (or swap to a new) overview WS for the selected topic.
///
/// If `app.overview_ws_target` already matches the new (channel, topic),
/// this is a no-op. Otherwise the previous overview WS is gracefully
/// closed (sends `disconnect` so the spawned task exits), and a new WS
/// task is spawned against `/ws/<channel>/<topic>`.
///
/// `cmd_tx` is kept in `App.overview_ws_tx` so the task can keep
/// reconnecting on transient errors (the `cmd_rx.recv() = None` path
/// only triggers on graceful close, not idle timeouts).
fn ensure_overview_ws(app: &mut App, addr: &str, channel: &str, topic: &str) {
    if let Some((c, t)) = app.overview_ws_target.as_ref()
        && c == channel
        && t == topic
    {
        return;
    }
    // Close the previous overview WS (if any) by sending a disconnect
    // message. The task exits when cmd_rx.recv() returns None.
    if let Some(tx) = app.overview_ws_tx.take() {
        let _ = tx.send(r#"{"type":"disconnect"}"#.to_string());
    }
    let url = format!("ws://{}/ws/{}/{}", addr, channel, topic);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
    // Store the sender so we can send disconnect on close/swap. The task
    // reads cmd_rx and exits cleanly when the channel closes.
    app.overview_ws_tx = Some(cmd_tx);
    app.overview_ws_rx = event_rx;
    app.overview_ws_target = Some((channel.to_string(), topic.to_string()));
    tokio::spawn(ws::ws_client_task(url, cmd_rx, event_tx, app.token.clone()));
}

/// Close the overview WS (used when chat mode opens; chat WS takes over).
fn close_overview_ws(app: &mut App) {
    if let Some(tx) = app.overview_ws_tx.take() {
        let _ = tx.send(r#"{"type":"disconnect"}"#.to_string());
    }
    app.overview_ws_target = None;
    // Drain any pending events so the next selection doesn't get stale data.
    while app.overview_ws_rx.try_recv().is_ok() {}
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    // Shortcuts live in the leader-key popup (Ctrl+P); the status
    // bar only advertises how to reach them.
    let help_text = "[^P]leader [^Q]quit".to_string();

    // Right-aligned chip: the mouse-capture chip is always visible
    // (8 cells, global terminal state) and sits at the rightmost edge.
    let mouse_width: u16 = 8;
    let [left_area, mouse_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(mouse_width)]).areas(area);

    let state = match &app.state {
        Some(s) => s,
        None => {
            let bar = Paragraph::new(format!(" {help_text}"))
                .style(Style::default().bg(Color::DarkGray).fg(Color::White));
            frame.render_widget(bar, left_area);
            render_mouse_chip(frame, mouse_area, app.mouse_capture_enabled);
            return;
        }
    };

    let stats = &state.stats;

    let status_part = if let Some((msg, _)) = &app.status_message {
        Span::styled(msg.as_str(), Style::default().fg(Color::Yellow))
    } else {
        Span::raw(format!(
            "{} active / {} thr │ {} recv │ {} err │ up {} │ jyc ai v{}",
            stats.active_workers,
            stats.total_topics,
            stats.messages_received,
            stats.errors,
            format_duration(state.uptime_secs),
            state.version,
        ))
    };

    let bar = Paragraph::new(Line::from({
        let mut spans = vec![Span::raw(" ")];
        if app.chat.visible {
            if app.chat.ws_connected {
                spans.push(Span::styled("●", Style::default().fg(Color::Green)));
            } else {
                spans.push(Span::styled("●", Style::default().fg(Color::Red)));
            }
            spans.push(Span::raw(" "));
        }
        spans.push(Span::raw(format!("{help_text}  ")));
        spans.push(status_part);
        spans
    }))
    .style(Style::default().bg(Color::DarkGray).fg(Color::White));

    frame.render_widget(bar, left_area);

    render_mouse_chip(frame, mouse_area, app.mouse_capture_enabled);
}

/// Render the right-aligned mouse-capture chip. Same 8-cell padded
/// format as the other status-bar chips (see `render_status_bar`). Peach means
/// capture is on (wheel scrolls, tmux select disabled); overlay0 means
/// off (tmux select works, wheel ignored).
fn render_mouse_chip(frame: &mut Frame, area: Rect, mouse_capture_enabled: bool) {
    let (label, bg, fg) = if mouse_capture_enabled {
        (
            " MOUSE+ ",
            Color::Rgb(250, 179, 135), // Catppuccin Mocha peach
            Color::Rgb(30, 30, 46),    // Catppuccin Mocha base
        )
    } else {
        (
            " MOUSE- ",
            Color::Rgb(108, 112, 134), // Catppuccin Mocha overlay0
            Color::Rgb(205, 214, 244), // Catppuccin Mocha text
        )
    };
    let chip = Paragraph::new(Span::styled(
        label,
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    ))
    .alignment(Alignment::Center);
    frame.render_widget(chip, area);
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

fn format_last_active(value: Option<&str>) -> String {
    let value = match value {
        Some(v) => v,
        None => return "-".to_string(),
    };
    let dt = match chrono::DateTime::parse_from_rfc3339(value) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(_) => return "-".to_string(),
    };
    let now = chrono::Utc::now();
    let diff = now.signed_duration_since(dt);
    if diff.num_minutes() <= 60 {
        let mins = diff.num_minutes();
        return format!("{}m ago", mins.max(0));
    }
    let dt_utc = dt.format("%H:%M").to_string();
    if dt.date_naive() == now.date_naive() {
        return dt_utc;
    }
    dt.format("%b %d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn char_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn assert_swallow(f: Filtered) {
        assert!(matches!(f, Filtered::Swallow), "expected Swallow");
    }

    fn assert_dispatch(f: Filtered, code: KeyCode) {
        match f {
            Filtered::Dispatch(k) => assert_eq!(k.code, code),
            _ => panic!("expected Dispatch({code:?})"),
        }
    }

    fn assert_replay(f: Filtered, first: KeyCode, second: KeyCode) {
        match f {
            Filtered::Replay(a, b) => {
                assert_eq!(a.code, first);
                assert_eq!(b.code, second);
            }
            _ => panic!("expected Replay({first:?}, {second:?})"),
        }
    }

    #[test]
    fn sgr_mouse_fragment_is_swallowed() {
        let mut filter = MouseFragmentFilter::default();
        let t0 = std::time::Instant::now();
        assert_swallow(filter.feed(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), t0));
        for c in "[<65;35;12M".chars() {
            assert_swallow(filter.feed(char_key(c), t0 + Duration::from_millis(1)));
        }
        // Keys after the fragment flow normally again.
        assert_dispatch(
            filter.feed(char_key('a'), t0 + Duration::from_millis(2)),
            KeyCode::Char('a'),
        );
    }

    #[test]
    fn rxvt_mouse_fragment_is_swallowed() {
        let mut filter = MouseFragmentFilter::default();
        let t0 = std::time::Instant::now();
        assert_swallow(filter.feed(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), t0));
        for c in "[65;35;12M".chars() {
            assert_swallow(filter.feed(char_key(c), t0 + Duration::from_millis(1)));
        }
    }

    #[test]
    fn esc_then_letter_replays_both() {
        let mut filter = MouseFragmentFilter::default();
        let t0 = std::time::Instant::now();
        assert_swallow(filter.feed(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), t0));
        assert_replay(
            filter.feed(char_key('a'), t0 + Duration::from_millis(5)),
            KeyCode::Esc,
            KeyCode::Char('a'),
        );
    }

    #[test]
    fn slow_bracket_after_esc_replays_both() {
        // A human typing Esc then '[' cannot beat the fragment window.
        let mut filter = MouseFragmentFilter::default();
        let t0 = std::time::Instant::now();
        assert_swallow(filter.feed(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), t0));
        assert_replay(
            filter.feed(
                char_key('['),
                t0 + ESC_FRAGMENT_WINDOW + Duration::from_millis(10),
            ),
            KeyCode::Esc,
            KeyCode::Char('['),
        );
    }

    #[test]
    fn held_esc_flushes_after_window() {
        let mut filter = MouseFragmentFilter::default();
        let t0 = std::time::Instant::now();
        assert_swallow(filter.feed(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), t0));
        assert!(filter.flush(t0 + Duration::from_millis(10)).is_none());
        let esc = filter
            .flush(t0 + ESC_FRAGMENT_WINDOW + Duration::from_millis(10))
            .expect("stale Esc must flush");
        assert_eq!(esc.code, KeyCode::Esc);
        // Already flushed — nothing pending.
        assert!(
            filter
                .flush(t0 + ESC_FRAGMENT_WINDOW + Duration::from_millis(20))
                .is_none()
        );
    }

    #[test]
    fn deviation_mid_fragment_stops_swallowing() {
        let mut filter = MouseFragmentFilter::default();
        let t0 = std::time::Instant::now();
        assert_swallow(filter.feed(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), t0));
        assert_swallow(filter.feed(char_key('['), t0 + Duration::from_millis(1)));
        assert_swallow(filter.feed(char_key('<'), t0 + Duration::from_millis(1)));
        // 'x' is not fragment syntax — filtering stops and it dispatches.
        assert_dispatch(
            filter.feed(char_key('x'), t0 + Duration::from_millis(1)),
            KeyCode::Char('x'),
        );
        assert_dispatch(
            filter.feed(char_key('5'), t0 + Duration::from_millis(1)),
            KeyCode::Char('5'),
        );
    }

    #[test]
    fn resolve_topic_path_defaults_to_cwd() {
        let resolved = resolve_topic_path(None).expect("should resolve");
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(resolved, cwd);
    }

    #[test]
    fn resolve_topic_path_makes_relative_absolute() {
        let resolved = resolve_topic_path(Some(".")).expect("should resolve");
        assert!(
            PathBuf::from(&resolved).is_absolute(),
            "relative path should be resolved to absolute: {resolved}"
        );
    }

    #[test]
    fn resolve_topic_path_canonicalizes_existing_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sub = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();

        let input = tmp.path().join("a").join(".").join("b");
        let resolved = resolve_topic_path(Some(input.to_str().unwrap())).expect("should resolve");
        assert_eq!(resolved, sub.to_string_lossy().to_string());
    }

    #[test]
    fn derive_topic_name_uses_explicit_value() {
        assert_eq!(derive_topic_name("/any/path", Some("my-topic")), "my-topic");
    }

    #[test]
    fn derive_topic_name_uses_folder_name() {
        assert_eq!(derive_topic_name("/home/user/foo", None), "foo");
    }

    #[test]
    fn derive_topic_name_falls_back_to_adhoc() {
        assert_eq!(derive_topic_name("", None), "adhoc");
    }

    #[test]
    fn check_existing_topic_name_succeeds_when_no_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().to_string_lossy().to_string();
        check_existing_topic_name(&path, "any-topic").expect("should pass when no file exists");
    }

    #[test]
    fn check_existing_topic_name_succeeds_when_matching() {
        let tmp = tempfile::TempDir::new().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(jyc_dir.join("topic-name"), "abc").unwrap();

        let path = tmp.path().to_string_lossy().to_string();
        check_existing_topic_name(&path, "abc").expect("should pass when names match");
    }

    #[test]
    fn check_existing_topic_name_fails_when_mismatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(jyc_dir.join("topic-name"), "existing").unwrap();

        let path = tmp.path().to_string_lossy().to_string();
        let err = check_existing_topic_name(&path, "abc").expect_err("should fail on mismatch");
        let msg = err.to_string();
        assert!(
            msg.contains("existing"),
            "error should mention existing name: {msg}"
        );
        assert!(
            msg.contains("abc"),
            "error should mention requested name: {msg}"
        );
    }

    #[test]
    fn check_existing_topic_name_succeeds_when_file_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(jyc_dir.join("topic-name"), "").unwrap();

        let path = tmp.path().to_string_lossy().to_string();
        check_existing_topic_name(&path, "new-topic").expect("should pass when file is empty");
    }

    fn make_test_app() -> App {
        let (_, ws_rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        App::new(ws_rx, None)
    }

    #[tokio::test]
    async fn ensure_overview_ws_noop_when_target_unchanged() {
        let mut app = make_test_app();
        app.overview_ws_target = Some(("chan".to_string(), "thr".to_string()));
        let original_rx_ptr = std::ptr::addr_of!(app.overview_ws_rx);
        // No new task should be spawned; rx is not replaced.
        ensure_overview_ws(&mut app, "127.0.0.1:9876", "chan", "thr");
        assert_eq!(
            app.overview_ws_target,
            Some(("chan".to_string(), "thr".to_string()))
        );
        assert!(std::ptr::eq(
            original_rx_ptr,
            std::ptr::addr_of!(app.overview_ws_rx)
        ));
    }

    #[tokio::test]
    async fn ensure_overview_ws_swaps_when_target_changes() {
        let mut app = make_test_app();
        // Seed: pretend a previous WS exists for topic A.
        app.overview_ws_target = Some(("chan".to_string(), "old".to_string()));
        // Call for a different target.
        ensure_overview_ws(&mut app, "127.0.0.1:9876", "chan", "new");
        assert_eq!(
            app.overview_ws_target,
            Some(("chan".to_string(), "new".to_string()))
        );
        // A disconnect message should have been sent on the old cmd_tx.
        // (Can't easily test the spawn itself, but the target updated and
        //  the old cmd_tx is consumed.)
    }

    #[tokio::test]
    async fn close_overview_ws_drains_and_clears_target() {
        let mut app = make_test_app();
        // Seed: pretend a WS exists and has an event in the rx queue.
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        app.overview_ws_tx = Some(cmd_tx);
        app.overview_ws_target = Some(("chan".to_string(), "thr".to_string()));
        app.overview_ws_rx = {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
            // The tx side is dropped, so any send would fail. rx still alive
            // so we can push events into the queue.
            drop(tx);
            rx
        };
        close_overview_ws(&mut app);
        assert!(app.overview_ws_target.is_none());
        assert!(app.overview_ws_tx.is_none());
    }

    // --- auto-select first topic on initial load ---

    fn make_overview_with_topics(names: &[&str]) -> jyc_types::InspectOverview {
        use jyc_types::{ChannelInfo, InspectOverview, TopicStatus, TopicSummary};
        InspectOverview {
            uptime_secs: 0,
            version: "test".to_string(),
            channels: vec![ChannelInfo {
                name: "chan".to_string(),
                channel_type: "websocket".to_string(),
                active_workers: 0,
                max_concurrent: 0,
            }],
            topics: names
                .iter()
                .map(|n| TopicSummary {
                    name: (*n).to_string(),
                    channel: "chan".to_string(),
                    pattern: None,
                    status: TopicStatus::Idle,
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
            stats: Default::default(),
            commands: vec![],
            models: vec![],
        }
    }

    #[tokio::test]
    async fn auto_select_first_topic_when_no_selection() {
        let mut app = make_test_app();
        app.state = Some(make_overview_with_topics(&["alpha", "beta"]));
        assert!(app.table_state.selected().is_none());

        // Simulate the auto-select block from the poll loop.
        if let Some(ref s) = app.state
            && app.table_state.selected().is_none()
            && !s.topics.is_empty()
        {
            app.table_state.select(Some(0));
        }

        assert_eq!(app.table_state.selected(), Some(0));
    }

    #[tokio::test]
    async fn auto_select_noop_when_already_selected() {
        let mut app = make_test_app();
        app.state = Some(make_overview_with_topics(&["alpha", "beta"]));
        app.table_state.select(Some(1));

        // Simulate the auto-select block from the poll loop.
        if let Some(ref s) = app.state
            && app.table_state.selected().is_none()
            && !s.topics.is_empty()
        {
            app.table_state.select(Some(0));
        }

        // Selection unchanged - user already navigated to row 1.
        assert_eq!(app.table_state.selected(), Some(1));
    }

    #[tokio::test]
    async fn auto_select_noop_when_no_topics() {
        let mut app = make_test_app();
        app.state = Some(make_overview_with_topics(&[]));
        assert!(app.table_state.selected().is_none());

        // Simulate the auto-select block.
        if let Some(ref s) = app.state
            && app.table_state.selected().is_none()
            && !s.topics.is_empty()
        {
            app.table_state.select(Some(0));
        }

        // No topics -> no auto-select.
        assert!(app.table_state.selected().is_none());
    }

    /// Regression: the dashboard **overview** Details panel fully encloses the
    /// topic info pane (`Borders::ALL`) and frames the activity log in
    /// `Borders::TOP | Borders::LEFT | Borders::RIGHT`, giving a continuous
    /// left/right edge with no gap at the top or bottom of the info pane.
    /// Drives `render_details` end-to-end (rather than calling the inner
    /// renderers directly) so the assertions pin the *call-site* wiring: if
    /// either caller drops a border flag, the corresponding
    /// `│` / `┌` / `┐` / `└` / `┘` cells disappear.
    #[test]
    fn overview_details_panes_have_continuous_side_borders() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = make_test_app();
        app.state = Some(make_overview_with_topics(&["alpha"]));
        app.table_state.select(Some(0));

        let width: u16 = 40;
        let height: u16 = 20;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_details(frame, frame.area(), &app))
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        // detail_chunks: info pane occupies y = 0..9, activity pane y = 9..
        // Info pane has Borders::ALL:
        //   y = 0     → top corners ┌ / ┐
        //   y = 1..8  → side borders │ / │
        //   y = 8     → bottom corners └ / ┘
        assert_eq!(buffer[(0, 0)].symbol(), "┌", "info pane top-left corner");
        assert_eq!(
            buffer[(width - 1, 0)].symbol(),
            "┐",
            "info pane top-right corner"
        );
        let info_bottom: u16 = 8;
        assert_eq!(
            buffer[(0, info_bottom)].symbol(),
            "└",
            "info pane bottom-left corner"
        );
        assert_eq!(
            buffer[(width - 1, info_bottom)].symbol(),
            "┘",
            "info pane bottom-right corner"
        );
        // Activity pane has TOP | LEFT | RIGHT (no BOTTOM):
        //   y = 9              → top corners ┌ / ┐
        //   y = 10 .. height   → side borders │ / │
        let activity_top: u16 = 9;
        assert_eq!(
            buffer[(0, activity_top)].symbol(),
            "┌",
            "activity pane top-left corner"
        );
        assert_eq!(
            buffer[(width - 1, activity_top)].symbol(),
            "┐",
            "activity pane top-right corner"
        );
        // Every other row in both panes carries continuous side borders.
        for y in (1..height).filter(|y| *y != info_bottom && *y != activity_top) {
            let left = buffer[(0, y)].symbol();
            let right = buffer[(width - 1, y)].symbol();
            assert_eq!(left, "│", "row {y} left edge should be `│`, got {left:?}");
            assert_eq!(
                right, "│",
                "row {y} right edge should be `│`, got {right:?}"
            );
        }
    }

    /// Regression: the dashboard/overview activity pane keeps its `── Activity`
    /// title in the top border. PR #669 removed the title from the shared
    /// `render_activity_log_inner`, stripping it from the dashboard too. The
    /// title must stay on the dashboard (only the chat screen drops it).
    #[test]
    fn overview_activity_pane_keeps_title() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = make_test_app();
        app.state = Some(make_overview_with_topics(&["alpha"]));
        app.table_state.select(Some(0));

        let width: u16 = 40;
        let height: u16 = 20;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_details(frame, frame.area(), &app))
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        // Activity pane top border is at y = 9 (info pane occupies 0..9).
        let activity_top: u16 = 9;
        let title_row: String = (0..width)
            .map(|x| buffer[(x, activity_top)].symbol().to_string())
            .collect();
        assert!(
            title_row.contains("── Activity"),
            "dashboard activity pane should keep its `── Activity` title, got: {title_row:?}"
        );
    }
}
