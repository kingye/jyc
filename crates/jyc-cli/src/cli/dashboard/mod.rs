use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use crossterm::{
    ExecutableCommand,
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use edtui::{EditorEventHandler, EditorMode, EditorState, EditorTheme, EditorView, Lines};
use ratatui::{
    Frame, Terminal,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    prelude::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Widget, Wrap},
};
use std::io::{Stdout, stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::Command;

use unicode_width::UnicodeWidthStr;

use jyc_inspect::client::InspectClient;
use jyc_types::{CommandInfo, InspectState, ModelInfo, Severity, ThreadStatus};

use super::command_popup::*;

mod chat;
mod ws;
mod ws_auth;
use chat::*;
use ws::*;

#[derive(Args, Debug)]
pub struct DashboardArgs {
    /// Inspect server address (also used for WebSocket chat)
    #[arg(long, default_value = "127.0.0.1:9876", global = true)]
    pub addr: String,

    /// Subcommand for dashboard operations (defaults to opening the full dashboard)
    #[command(subcommand)]
    pub command: Option<DashboardCommand>,
}

#[derive(Subcommand, Debug)]
pub enum DashboardCommand {
    /// Open a directory as an ad-hoc thread and launch chat mode.
    #[command(name = "open")]
    Open(OpenArgs),
}

/// Arguments for opening a directory as an ad-hoc thread.
///
/// Shared by `jyc dashboard open` and the top-level `jyc open` shortcut.
#[derive(Args, Debug)]
pub struct OpenArgs {
    /// Thread name (defaults to folder name of --path or current directory)
    #[arg(short = 't', long)]
    pub thread: Option<String>,

    /// Websocket channel name (auto-detected if only one exists)
    #[arg(short = 'c', long)]
    pub channel: Option<String>,

    /// Thread working directory (defaults to current directory)
    #[arg(short = 'p', long)]
    pub path: Option<String>,
}

/// Application state for the TUI.
struct App {
    state: Option<InspectState>,
    error: Option<String>,
    table_state: TableState,
    should_quit: bool,
    status_message: Option<(String, std::time::Instant)>,
    pending_reset: Option<(String, std::time::Instant)>,

    /// Chat pane state (WebSocket thread chat and non-WebSocket detail mode).
    chat: ChatState,
}

impl App {
    fn new(ws_rx: tokio::sync::mpsc::UnboundedReceiver<WsEvent>) -> Self {
        Self {
            state: None,
            error: None,
            table_state: TableState::default(),
            should_quit: false,
            status_message: None,
            pending_reset: None,
            chat: ChatState::new(ws_rx),
        }
    }

    fn set_status(&mut self, msg: String) {
        self.status_message = Some((msg, std::time::Instant::now()));
    }

    fn clear_pending_reset(&mut self) {
        self.pending_reset = None;
    }

    fn tick_status(&mut self) {
        if let Some((_, at)) = &self.status_message
            && at.elapsed() > Duration::from_secs(5)
        {
            self.status_message = None;
        }
        if let Some((_, at)) = &self.pending_reset
            && at.elapsed() > Duration::from_secs(3)
        {
            self.pending_reset = None;
        }
    }

    fn next_thread(&mut self) {
        let count = self.state.as_ref().map(|s| s.threads.len()).unwrap_or(0);
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
        let count = self.state.as_ref().map(|s| s.threads.len()).unwrap_or(0);
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
                // Request pattern list on connect
                let list_msg = serde_json::json!({"type": "list_patterns"}).to_string();
                if let Some(tx) = &self.chat.ws_tx {
                    let _ = tx.send(list_msg);
                }

                // Auto-re-subscribe to the previously selected thread, if any
                if let Some(ref thread) = self.chat.thread {
                    let subscribe_msg = serde_json::json!({
                        "type": "subscribe",
                        "thread": thread,
                    })
                    .to_string();
                    if let Some(tx) = &self.chat.ws_tx {
                        let _ = tx.send(subscribe_msg);
                    }
                    self.set_status(format!("Reconnected to {thread}"));
                }
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
async fn ensure_serve_running(addr: &str) -> Result<()> {
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
    let log_dir = jyc_utils::paths::data_home().ok_or_else(|| {
        anyhow::anyhow!("Could not determine platform data directory for log file")
    })?;
    tokio::fs::create_dir_all(&log_dir)
        .await
        .with_context(|| format!("Failed to create log directory {}", log_dir.display()))?;
    let log_path = log_dir.join("jyc.log");

    // Open log file (create / truncate).
    let log_file = std::fs::File::create(&log_path)
        .with_context(|| format!("Failed to create log file {}", log_path.display()))?;
    let log_dup = log_file
        .try_clone()
        .context("Failed to clone log file handle")?;

    // Spawn jyc serve as a background child process.
    let exe = std::env::current_exe().context("Could not determine jyc binary path")?;
    let mut child = Command::new(&exe)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(log_dup)
        .stderr(log_file)
        .kill_on_drop(true)
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

pub async fn run(
    args: &DashboardArgs,
    initial_thread: Option<&str>,
    initial_channel: Option<&str>,
) -> Result<()> {
    // Auto-spawn jyc serve if it's not running.
    ensure_serve_running(&args.addr).await.with_context(|| {
        format!(
            "Failed to connect to {}. Start jyc serve manually.",
            args.addr
        )
    })?;

    let mut client = InspectClient::new(&args.addr);

    // Setup terminal
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    stdout().execute(EnableBracketedPaste)?;

    // Terminal and its backend are scoped so they drop *before* we restore
    // the terminal. Otherwise the backend's Drop flushes buffered escape
    // codes after LeaveAlternateScreen, corrupting line alignment.
    let result = {
        let backend = CrosstermBackend::new(stdout());
        let mut terminal = Terminal::new(backend)?;

        let (_, ws_rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(ws_rx);
        let poll_interval = Duration::from_millis(500);
        let mut last_poll = std::time::Instant::now() - poll_interval; // Force immediate poll

        // If a thread was requested on the CLI, open chat directly.
        if let Some(thread) = initial_thread {
            app.chat.open(&args.addr, initial_channel, Some(thread));
        }

        loop {
            // Poll for new state
            if last_poll.elapsed() >= poll_interval {
                match client.get_state().await {
                    Ok(state) => {
                        // Clear awaiting_response once the server confirms the thread
                        // is no longer processing (with a small grace period to avoid
                        // flicker between the local flag and server state).
                        if app.chat.awaiting_response
                            && let Some(ref chat_name) = app.chat.thread
                        {
                            let ct = state.threads.iter().find(|t| t.name == *chat_name);
                            if let Some(ct) = ct
                                && ct.status != ThreadStatus::Processing
                            {
                                app.chat.awaiting_response = false;
                            }
                        }

                        // Detail mode: extract live chat messages from recent_messages
                        if app.chat.is_detail_mode()
                            && let Some(ref chat_name) = app.chat.thread
                            && let Some(ct) = state.threads.iter().find(|t| t.name == *chat_name)
                        {
                            let mut new_msg = false;
                            for msg in &ct.recent_messages {
                                // Skip messages we already have (dedup by text+timestamp)
                                let already =
                                    app.chat.messages.iter().any(|m| {
                                        m.text == msg.text && m.timestamp == msg.timestamp
                                    });
                                if !already {
                                    app.chat.messages.push(ChatMessage {
                                        sender: msg.sender.clone(),
                                        text: msg.text.clone(),
                                        timestamp: msg.timestamp.clone(),
                                    });
                                    new_msg = true;
                                }
                            }
                            // Auto-scroll to bottom only when new messages arrive
                            if new_msg {
                                app.chat.scroll = 0;
                            }
                        }

                        app.state = Some(state);
                        if let Some(ref s) = app.state {
                            app.chat.commands = s.commands.clone();
                            app.chat.models = s.models.clone();
                        }
                        app.error = None;
                    }
                    Err(e) => {
                        app.error = Some(format!("{e:#}"));
                    }
                }
                last_poll = std::time::Instant::now();
            }

            // Process pending message injection (detail mode)
            if let Some((thread, text)) = app.chat.pending_inject.take()
                && let Some(ref channel) = app.chat.detail_channel
            {
                match client.inject_message(channel, &thread, &text).await {
                    Ok((true, msg)) => {
                        tracing::debug!("Message injected: {msg}");
                    }
                    Ok((false, msg)) => {
                        app.set_status(format!("Inject failed: {msg}"));
                        app.chat.awaiting_response = false;
                    }
                    Err(e) => {
                        app.set_status(format!("Inject error: {e:#}"));
                        app.chat.awaiting_response = false;
                    }
                }
            }

            // Check for WebSocket events
            while let Ok(event) = app.chat.ws_rx.try_recv() {
                app.handle_ws_event(event);
            }

            // Clear expired status messages
            app.tick_status();

            // Draw
            terminal.draw(|f| ui(f, &mut app))?;

            // Handle input (non-blocking, 50ms timeout)
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Paste(data)
                        if app.chat.visible && app.chat.focus == ChatFocus::ChatPane =>
                    {
                        // Bracketed paste delivers the whole pasted chunk as
                        // one event. Forward it to the editor so it never
                        // triggers Enter handling / send.
                        app.chat.handler.on_paste_event(data, &mut app.chat.editor);
                    }
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if app.chat.visible {
                            handle_chat_keys(&mut app, key, &mut terminal);
                        } else {
                            handle_normal_keys(
                                &mut app,
                                key,
                                &mut client,
                                &mut last_poll,
                                &args.addr,
                            )
                            .await;
                        }
                    }
                    _ => {}
                }
            }

            if app.should_quit {
                break Ok(());
            }
        }
    }; // terminal + backend dropped here

    // Restore terminal — safe now that no buffered escape codes remain
    stdout().execute(DisableBracketedPaste)?;
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    result
}

/// Open a directory as an ad-hoc websocket thread and launch chat mode.
///
/// Resolves the thread name (from explicit `-t` or the folder name of `-p`),
/// the websocket channel (explicit `-c` or auto-detected when only one
/// exists), and the absolute thread path. Sends a `create_thread` message
/// over the websocket, waits for the inspect server to report the thread,
/// then opens the dashboard with chat already focused on the thread.
///
/// The target directory may be brand new or already contain a `.jyc`
/// subdirectory; in either case the path is registered as the thread's
/// working directory.
pub async fn run_open(
    addr: &str,
    thread: Option<&str>,
    channel: Option<&str>,
    path: Option<&str>,
) -> Result<()> {
    // Auto-spawn jyc serve if it's not running.
    ensure_serve_running(addr)
        .await
        .with_context(|| format!("Failed to connect to {addr}. Start jyc serve manually."))?;

    // Resolve thread path and name
    let path = resolve_thread_path(path)?;
    let thread = derive_thread_name(&path, thread);

    // If the directory was previously opened as a thread, the thread-name file
    // records the canonical name. Refuse to re-open it under a different name
    // to avoid diverging history and storage paths.
    check_existing_thread_name(&path, &thread)?;

    // Resolve websocket channel using inspect state
    let mut client = InspectClient::new(addr);
    let channel = resolve_websocket_channel(&mut client, channel).await?;

    tracing::info!(
        thread = %thread,
        channel = %channel,
        path = %path,
        "Opening directory as ad-hoc thread via dashboard CLI"
    );

    // Send create_thread over websocket to the target channel
    create_thread_via_websocket(addr, &channel, &thread, &path).await?;

    // Wait for the inspect server to report the thread
    wait_for_thread(&mut client, &thread, &channel).await?;

    // Open dashboard directly in chat mode for the thread
    run(
        &DashboardArgs {
            addr: addr.to_string(),
            command: None,
        },
        Some(&thread),
        Some(&channel),
    )
    .await
}

/// Resolve the thread path to an absolute filesystem path.
///
/// Expands a leading `~` to `$HOME`. Relative paths are resolved against the
/// current working directory. If the path exists, it is canonicalized; otherwise
/// the absolute path is returned as-is so that new directories can be created
/// later by the storage layer.
fn resolve_thread_path(path: Option<&str>) -> Result<String> {
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

/// Derive the thread name from explicit input or the folder name of the path.
fn derive_thread_name(path: &str, thread: Option<&str>) -> String {
    if let Some(name) = thread {
        return name.to_string();
    }
    PathBuf::from(path)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "adhoc".to_string())
}

/// Verify that the directory has not already been registered under a
/// different thread name.
///
/// If `<path>/.jyc/thread-name` exists and contains a non-empty name that
/// differs from `thread`, returns an error to prevent diverging history and
/// storage paths.
fn check_existing_thread_name(path: &str, thread: &str) -> Result<()> {
    let thread_name_file = PathBuf::from(path).join(".jyc").join("thread-name");
    if thread_name_file.exists() {
        let existing = std::fs::read_to_string(&thread_name_file)
            .with_context(|| format!("failed to read {}", thread_name_file.display()))?;
        let existing = existing.trim();
        if !existing.is_empty() && existing != thread {
            anyhow::bail!(
                "directory '{}' is already registered as thread '{}'; \
                 cannot open as '{}'. Use 'jyc open -t {} -p {}' instead",
                path,
                existing,
                thread,
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
    client: &mut InspectClient,
    channel: Option<&str>,
) -> Result<String> {
    if let Some(name) = channel {
        return Ok(name.to_string());
    }

    let state = client.get_state().await?;
    let ws_channels: Vec<String> = state
        .channels
        .into_iter()
        .filter(|c| c.channel_type == "websocket")
        .map(|c| c.name)
        .collect();

    match ws_channels.len() {
        0 => anyhow::bail!(
            "No websocket channel configured. Add a [channels.<name>] with type = \"websocket\" to config.toml."
        ),
        1 => Ok(ws_channels.into_iter().next().unwrap()),
        _ => anyhow::bail!(
            "Multiple websocket channels found: {:?}. Use --channel (-c) to specify one.",
            ws_channels
        ),
    }
}

/// Send a `create_thread` message over a short-lived websocket connection.
async fn create_thread_via_websocket(
    addr: &str,
    channel: &str,
    thread: &str,
    path: &str,
) -> Result<()> {
    let url = format!("ws://{}/ws/{}", addr, channel);
    let request = ws_auth::build_authenticated_ws_request(&url);
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .with_context(|| format!("failed to connect to websocket at {url}"))?;

    let msg = serde_json::json!({
        "type": "create_thread",
        "thread": thread,
        "path": path,
    });
    use futures_util::SinkExt;
    ws_stream
        .send(tokio_tungstenite::tungstenite::Message::Text(
            msg.to_string(),
        ))
        .await
        .context("failed to send create_thread message")?;

    // Graceful close; best-effort only.
    let _ = ws_stream.close(None).await;
    Ok(())
}

/// Poll the inspect server until the newly created thread appears in state.
async fn wait_for_thread(client: &mut InspectClient, thread: &str, channel: &str) -> Result<()> {
    for _ in 0..50 {
        let state = client.get_state().await?;
        if state
            .threads
            .iter()
            .any(|t| t.name == thread && t.channel == channel)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("Timeout waiting for thread {thread} to be created")
}

async fn handle_normal_keys(
    app: &mut App,
    key: event::KeyEvent,
    client: &mut InspectClient,
    last_poll: &mut std::time::Instant,
    addr: &str,
) {
    // ^Q quits the entire dashboard (consistent across all modes)
    if key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }

    match key.code {
        KeyCode::Char('c') => {
            app.chat.open(addr, None, None);
        }
        KeyCode::Enter => {
            // Enter chat for websocket threads, detail mode for non-websocket threads
            let thread_info = app.state.as_ref().and_then(|s| {
                app.table_state
                    .selected()
                    .and_then(|i| s.threads.get(i))
                    .map(|t| {
                        let is_ws = s
                            .channels
                            .iter()
                            .find(|c| c.name == t.channel)
                            .is_some_and(|c| c.channel_type == "websocket");
                        (t.name.clone(), t.channel.clone(), is_ws)
                    })
            });
            if let Some((name, channel, is_ws)) = thread_info {
                if is_ws {
                    app.chat.open(addr, Some(&channel), Some(&name));
                } else {
                    app.chat
                        .open_thread_detail(&channel, &name, app.state.as_ref());
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.next_thread();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.prev_thread();
        }
        KeyCode::Char('r') => {
            // Force refresh
            *last_poll = std::time::Instant::now() - Duration::from_millis(500);
        }
        KeyCode::Char('R') => {
            // Reload config
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
        KeyCode::Char('s') => {
            if let Some((ref thread_name, at)) = app.pending_reset {
                if at.elapsed() <= Duration::from_secs(3) {
                    let name = thread_name.clone();
                    app.clear_pending_reset();
                    match client.reset_session(&name).await {
                        Ok((true, msg)) => {
                            app.set_status(format!("Session reset: {msg}"));
                            *last_poll = std::time::Instant::now() - Duration::from_millis(500);
                        }
                        Ok((false, msg)) => {
                            app.set_status(format!("Reset failed: {msg}"));
                        }
                        Err(e) => {
                            app.set_status(format!("Reset error: {e:#}"));
                        }
                    }
                } else {
                    app.clear_pending_reset();
                }
            } else {
                let thread_name = app.state.as_ref().and_then(|s| {
                    app.table_state
                        .selected()
                        .and_then(|i| s.threads.get(i).map(|t| t.name.clone()))
                });
                match thread_name {
                    Some(name) => {
                        app.pending_reset = Some((name.clone(), std::time::Instant::now()));
                        app.set_status(format!(
                            "Press `s` again to confirm reset session for {name}"
                        ));
                    }
                    None => {
                        app.set_status("No thread selected".to_string());
                    }
                }
            }
        }
        _ => {
            app.clear_pending_reset();
        }
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
    // Main layout: channels bar | threads table | detail panel | status bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),      // Channels bar
            Constraint::Percentage(40), // Threads table
            Constraint::Percentage(60), // Detail panel + activity log
            Constraint::Length(1),      // Status bar
        ])
        .split(area);

    render_channels(frame, chunks[0], app);
    render_threads(frame, chunks[1], app);
    render_details(frame, chunks[2], app);
    render_status_bar(frame, chunks[3], app);
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

fn render_threads(frame: &mut Frame, area: Rect, app: &mut App) {
    let state = match &app.state {
        Some(s) => s,
        None => {
            let block = Block::default().title(" Threads ").borders(Borders::ALL);
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
        Cell::from("Last Active"),
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
                ThreadStatus::Error => Style::default().fg(Color::Red),
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
                .title(format!(" Threads ({}) ", state.threads.len()))
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

    let selected = app
        .table_state
        .selected()
        .and_then(|i| state.threads.get(i));

    let selected = match selected {
        Some(t) => t,
        None => {
            let block = Block::default().title(" Details ").borders(Borders::ALL);
            let text = Paragraph::new("Select a thread with ↑/↓").block(block);
            frame.render_widget(text, area);
            return;
        }
    };

    // Split detail area: info (4 lines) + activity log (remaining)
    let detail_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8), // Thread info
            Constraint::Min(4),    // Activity log
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
                ThreadStatus::Processing => Style::default().fg(Color::Green),
                ThreadStatus::Queued => Style::default().fg(Color::Yellow),
                ThreadStatus::WaitingForAnswer => Style::default().fg(Color::Cyan),
                ThreadStatus::Idle => Style::default().fg(Color::DarkGray),
                ThreadStatus::Error => Style::default().fg(Color::Red),
            },
        ),
    ];

    if let (Some(cur), Some(max)) = (selected.input_tokens, selected.max_tokens) {
        let pct = if max > 0 {
            cur.checked_mul(100)
                .and_then(|v| v.checked_div(max))
                .unwrap_or(0)
        } else {
            0
        };
        status_line.push(Span::raw("  "));
        status_line.push(Span::styled(
            "Tokens: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        status_line.push(Span::raw(format!("{cur} / {max} ({pct}%)")));
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

    // Activity log panel
    render_activity_log_inner(frame, detail_chunks[1], selected, 0, 0, false);
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let help_text = if app.chat.visible {
        match app.chat.phase {
            ChatPhase::PatternSelect => {
                "[↑↓/jk]select [Enter]choose [Esc]back [^Q]quit".to_string()
            }
            ChatPhase::Chatting => {
                "[Tab]focus [↑↓/jk]scroll [gg/G]top/bottom [PgUp/PgDn ^F/^B]page [←→]cursor [^W]split [Esc]back [^Q]quit".to_string()
            }
        }
    } else {
        "[^Q]quit [↑↓]select [Enter]chat [r]refresh [R]reload [s]reset [c]new".to_string()
    };

    // Right-aligned vim mode chip while chatting. 8 cells = padded label width.
    let mode_width: u16 = if app.chat.visible && app.chat.phase == ChatPhase::Chatting {
        8
    } else {
        0
    };
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(mode_width)]).areas(area);

    let state = match &app.state {
        Some(s) => s,
        None => {
            let bar = Paragraph::new(format!(" {help_text}"))
                .style(Style::default().bg(Color::DarkGray).fg(Color::White));
            frame.render_widget(bar, left_area);
            return;
        }
    };

    let stats = &state.stats;

    let status_part = if let Some((msg, _)) = &app.status_message {
        Span::styled(msg.as_str(), Style::default().fg(Color::Yellow))
    } else {
        Span::raw(format!(
            "{} active / {} thr │ {} recv │ {} err │ up {} │ v{}",
            stats.active_workers,
            stats.total_threads,
            stats.messages_received,
            stats.errors,
            format_duration(state.uptime_secs),
            state.version,
        ))
    };

    let bar = Paragraph::new(Line::from({
        let mut spans = vec![Span::raw(" ")];
        if app.chat.visible && !app.chat.is_detail_mode() {
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

    // Right-align the vim mode chip (Catppuccin Mocha palette).
    if mode_width > 0 {
        let (label, bg, fg) = match app.chat.editor.mode {
            EditorMode::Normal => (
                " NORMAL ",
                Color::Rgb(137, 180, 250),
                Color::Rgb(30, 30, 46),
            ),
            EditorMode::Insert => (
                " INSERT ",
                Color::Rgb(166, 227, 161),
                Color::Rgb(30, 30, 46),
            ),
            EditorMode::Visual => (
                " VISUAL ",
                Color::Rgb(203, 166, 247),
                Color::Rgb(30, 30, 46),
            ),
            _ => (
                " NORMAL ",
                Color::Rgb(137, 180, 250),
                Color::Rgb(30, 30, 46),
            ),
        };
        let mode_para = Paragraph::new(Span::styled(
            label,
            Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Center);
        frame.render_widget(mode_para, right_area);
    }
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

    #[test]
    fn resolve_thread_path_defaults_to_cwd() {
        let resolved = resolve_thread_path(None).expect("should resolve");
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(resolved, cwd);
    }

    #[test]
    fn resolve_thread_path_makes_relative_absolute() {
        let resolved = resolve_thread_path(Some(".")).expect("should resolve");
        assert!(
            PathBuf::from(&resolved).is_absolute(),
            "relative path should be resolved to absolute: {resolved}"
        );
    }

    #[test]
    fn resolve_thread_path_canonicalizes_existing_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sub = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();

        let input = tmp.path().join("a").join(".").join("b");
        let resolved = resolve_thread_path(Some(input.to_str().unwrap())).expect("should resolve");
        assert_eq!(resolved, sub.to_string_lossy().to_string());
    }

    #[test]
    fn derive_thread_name_uses_explicit_value() {
        assert_eq!(
            derive_thread_name("/any/path", Some("my-thread")),
            "my-thread"
        );
    }

    #[test]
    fn derive_thread_name_uses_folder_name() {
        assert_eq!(derive_thread_name("/home/user/foo", None), "foo");
    }

    #[test]
    fn derive_thread_name_falls_back_to_adhoc() {
        assert_eq!(derive_thread_name("", None), "adhoc");
    }

    #[test]
    fn check_existing_thread_name_succeeds_when_no_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().to_string_lossy().to_string();
        check_existing_thread_name(&path, "any-thread").expect("should pass when no file exists");
    }

    #[test]
    fn check_existing_thread_name_succeeds_when_matching() {
        let tmp = tempfile::TempDir::new().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(jyc_dir.join("thread-name"), "abc").unwrap();

        let path = tmp.path().to_string_lossy().to_string();
        check_existing_thread_name(&path, "abc").expect("should pass when names match");
    }

    #[test]
    fn check_existing_thread_name_fails_when_mismatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(jyc_dir.join("thread-name"), "existing").unwrap();

        let path = tmp.path().to_string_lossy().to_string();
        let err = check_existing_thread_name(&path, "abc").expect_err("should fail on mismatch");
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
    fn check_existing_thread_name_succeeds_when_file_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(jyc_dir.join("thread-name"), "").unwrap();

        let path = tmp.path().to_string_lossy().to_string();
        check_existing_thread_name(&path, "new-thread").expect("should pass when file is empty");
    }
}
