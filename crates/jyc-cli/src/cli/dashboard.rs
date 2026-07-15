use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use crossterm::{
    ExecutableCommand,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    layout::{Constraint, Direction, Layout, Rect},
    prelude::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;

use unicode_width::UnicodeWidthStr;

use jyc_inspect::client::InspectClient;
use jyc_types::{InspectState, Severity, ThreadStatus};

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
    Open {
        /// Thread name (defaults to folder name of --path or current directory)
        #[arg(short = 't', long)]
        thread: Option<String>,

        /// Websocket channel name (auto-detected if only one exists)
        #[arg(short = 'c', long)]
        channel: Option<String>,

        /// Thread working directory (defaults to current directory)
        #[arg(short = 'p', long)]
        path: Option<String>,
    },
}

/// Phase of the chat pane UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatPhase {
    /// User is selecting a pattern to chat with.
    PatternSelect,
    /// User is actively chatting in a thread.
    Chatting,
}

/// Which pane has focus in chat mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatFocus {
    /// The chat conversation pane.
    ChatPane,
    /// The activity log pane.
    ActivityPane,
}

/// A single message in the chat conversation.
#[derive(Debug, Clone)]
struct ChatMessage {
    sender: String,
    text: String,
    timestamp: Option<String>,
}

/// Events from the WebSocket client task.
#[derive(Debug)]
enum WsEvent {
    Connected,
    Disconnected,
    Message(String),
    Error(String),
}

/// Application state for the TUI.
struct App {
    state: Option<InspectState>,
    error: Option<String>,
    table_state: TableState,
    should_quit: bool,
    status_message: Option<(String, std::time::Instant)>,
    pending_reset: Option<(String, std::time::Instant)>,

    // Chat pane state
    chat_visible: bool,
    chat_phase: ChatPhase,
    chat_patterns: Vec<String>,
    chat_pattern_selected: usize,
    chat_thread: Option<String>,
    chat_messages: Vec<ChatMessage>,
    chat_input: String,
    chat_cursor: usize,
    chat_focus: ChatFocus,
    chat_scroll: usize,
    activity_scroll: usize,
    /// Horizontal scroll offset for the activity pane (left-right).
    activity_hscroll: usize,
    /// Set locally when user sends a message, cleared when the poll confirms
    /// the thread is processing or has completed. Bridges the gap between
    /// sending a message and the inspect server reporting Processing status.
    chat_awaiting_response: bool,
    /// Activity pane split state: 0=80/20, 1=100/0, 2=20/80, 3=0/100
    activity_split: u8,
    ws_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    ws_rx: tokio::sync::mpsc::UnboundedReceiver<WsEvent>,
    ws_connected: bool,
    /// Timestamp of the last Enter press, used for paste debounce.
    /// When Enter is pressed within 300ms of the previous Enter, it's
    /// treated as a paste event and inserts a newline instead of sending.
    last_enter_at: Option<std::time::Instant>,
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
            chat_visible: false,
            chat_phase: ChatPhase::PatternSelect,
            chat_patterns: vec![],
            chat_pattern_selected: 0,
            chat_thread: None,
            chat_messages: vec![],
            chat_input: String::new(),
            chat_cursor: 0,
            chat_focus: ChatFocus::ChatPane,
            chat_scroll: 0,
            activity_scroll: 0,
            activity_hscroll: 0,
            chat_awaiting_response: false,
            activity_split: 0,
            ws_tx: None,
            ws_rx,
            ws_connected: false,
            last_enter_at: None,
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

    // ── Chat pane helpers ──────────────────────────────────────────────

    fn open_chat(&mut self, addr: &str, channel: Option<&str>, initial_thread: Option<&str>) {
        self.chat_visible = true;
        self.chat_phase = if initial_thread.is_some() {
            ChatPhase::Chatting
        } else {
            ChatPhase::PatternSelect
        };
        self.chat_patterns.clear();
        self.chat_pattern_selected = 0;
        self.chat_thread = initial_thread.map(|s| s.to_string());
        self.chat_messages.clear();
        self.chat_input.clear();
        self.chat_cursor = 0;
        self.chat_focus = ChatFocus::ChatPane;
        self.chat_scroll = 0;
        self.activity_scroll = 0;
        self.activity_hscroll = 0;
        self.activity_split = 0;
        self.ws_connected = false;
        self.last_enter_at = None;

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        self.ws_tx = Some(cmd_tx);
        // Replace the old receiver with the new one
        self.ws_rx = event_rx;

        let url = match channel {
            Some(ch) => format!("ws://{}/ws/{}", addr, ch),
            None => format!("ws://{}/ws", addr),
        };
        tokio::spawn(ws_client_task(url, cmd_rx, event_tx));
    }

    fn close_chat(&mut self) {
        self.chat_visible = false;
        self.chat_phase = ChatPhase::PatternSelect;
        self.ws_connected = false;
        if let Some(tx) = self.ws_tx.take() {
            // Best-effort disconnect signal
            let _ = tx.send("{\"type\":\"disconnect\"}".to_string());
        }
    }

    fn select_pattern(&mut self, pattern: String) {
        self.chat_phase = ChatPhase::Chatting;
        self.chat_thread = Some(pattern.clone());
        self.chat_input.clear();
        self.chat_cursor = 0;
        self.chat_scroll = 0;
        self.chat_messages.clear();
        self.chat_messages.clear();

        let subscribe_msg = serde_json::json!({
            "type": "subscribe",
            "thread": pattern,
        })
        .to_string();
        if let Some(tx) = &self.ws_tx {
            let _ = tx.send(subscribe_msg);
        }
    }

    fn go_to_pattern_select(&mut self) {
        self.chat_phase = ChatPhase::PatternSelect;
        self.chat_thread = None;
        self.chat_input.clear();
        self.chat_cursor = 0;
        self.chat_scroll = 0;
    }

    fn toggle_focus(&mut self) {
        self.chat_focus = match self.chat_focus {
            ChatFocus::ChatPane => ChatFocus::ActivityPane,
            ChatFocus::ActivityPane => ChatFocus::ChatPane,
        };
    }

    fn scroll_up(&mut self) {
        match self.chat_focus {
            ChatFocus::ChatPane => self.chat_scroll += 1,
            ChatFocus::ActivityPane => self.activity_scroll += 1,
        }
    }

    fn scroll_down(&mut self) {
        match self.chat_focus {
            ChatFocus::ChatPane => self.chat_scroll = self.chat_scroll.saturating_sub(1),
            ChatFocus::ActivityPane => {
                self.activity_scroll = self.activity_scroll.saturating_sub(1)
            }
        }
    }

    fn page_size(&self) -> usize {
        let base = crossterm::terminal::size()
            .map(|(_, h)| h.saturating_sub(7) as usize)
            .unwrap_or(10);
        match self.chat_focus {
            ChatFocus::ChatPane => {
                let term_width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
                let input_lines = count_wrapped_lines(&self.chat_input, term_width).clamp(1, 5);
                base.saturating_sub(input_lines).max(1)
            }
            ChatFocus::ActivityPane => base.max(1),
        }
    }

    fn page_up(&mut self) {
        let page = self.page_size();
        match self.chat_focus {
            ChatFocus::ChatPane => self.chat_scroll = self.chat_scroll.saturating_add(page),
            ChatFocus::ActivityPane => {
                self.activity_scroll = self.activity_scroll.saturating_add(page)
            }
        }
    }

    fn page_down(&mut self) {
        let page = self.page_size();
        match self.chat_focus {
            ChatFocus::ChatPane => self.chat_scroll = self.chat_scroll.saturating_sub(page),
            ChatFocus::ActivityPane => {
                self.activity_scroll = self.activity_scroll.saturating_sub(page)
            }
        }
    }

    fn send_chat_message(&mut self) {
        let text = self.chat_input.trim().to_string();
        if text.is_empty() {
            return;
        }
        let thread = match &self.chat_thread {
            Some(t) => t.clone(),
            None => return,
        };

        // Echo user message locally
        self.chat_messages.push(ChatMessage {
            sender: "user".to_string(),
            text: text.clone(),
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
        });
        self.chat_input.clear();
        self.chat_cursor = 0;
        self.chat_scroll = 0;
        self.chat_awaiting_response = true;

        let msg = serde_json::json!({
            "type": "message",
            "thread": thread,
            "text": text,
        })
        .to_string();
        if let Some(tx) = &self.ws_tx {
            let _ = tx.send(msg);
        }
    }

    /// Send a raw command message via WebSocket without echoing to the chat
    /// view or clearing the input. Used for quick keyboard shortcuts like
    /// Ctrl+C (cancel) and Shift+Tab (mode switch).
    fn send_raw_message(&mut self, text: &str) {
        let thread = match &self.chat_thread {
            Some(t) => t.clone(),
            None => return,
        };
        let msg = serde_json::json!({
            "type": "message",
            "thread": thread,
            "text": text,
        })
        .to_string();
        if let Some(tx) = &self.ws_tx {
            let _ = tx.send(msg);
        }
    }

    fn handle_ws_event(&mut self, event: WsEvent) {
        match event {
            WsEvent::Connected => {
                self.ws_connected = true;
                // Request pattern list on connect
                let list_msg = serde_json::json!({"type": "list_patterns"}).to_string();
                if let Some(tx) = &self.ws_tx {
                    let _ = tx.send(list_msg);
                }

                // Auto-re-subscribe to the previously selected thread, if any
                if let Some(ref thread) = self.chat_thread {
                    let subscribe_msg = serde_json::json!({
                        "type": "subscribe",
                        "thread": thread,
                    })
                    .to_string();
                    if let Some(tx) = &self.ws_tx {
                        let _ = tx.send(subscribe_msg);
                    }
                    self.set_status(format!("Reconnected to {thread}"));
                }
            }
            WsEvent::Disconnected => {
                self.ws_connected = false;
                self.set_status("WebSocket disconnected".to_string());
            }
            WsEvent::Message(text) => {
                self.handle_ws_message(&text);
            }
            WsEvent::Error(err) => {
                self.set_status(format!("WebSocket error: {err}"));
            }
        }
    }

    fn handle_ws_message(&mut self, text: &str) {
        let parsed: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };

        match parsed.get("type").and_then(|v| v.as_str()) {
            Some("patterns") => {
                if let Some(patterns) = parsed.get("patterns").and_then(|v| v.as_array()) {
                    self.chat_patterns = patterns
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    self.chat_pattern_selected = 0;
                }
            }
            Some("history") => {
                if let (Some(thread), Some(messages)) = (
                    parsed.get("thread").and_then(|v| v.as_str()),
                    parsed.get("messages").and_then(|v| v.as_array()),
                ) && self.chat_thread.as_deref() == Some(thread)
                {
                    self.chat_messages = messages
                        .iter()
                        .filter_map(|m| {
                            Some(ChatMessage {
                                sender: m.get("sender")?.as_str()?.to_string(),
                                text: m.get("text")?.as_str()?.to_string(),
                                timestamp: m
                                    .get("timestamp")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string()),
                            })
                        })
                        .collect();
                }
            }
            Some("reply") => {
                if let (Some(thread), Some(text)) = (
                    parsed.get("thread").and_then(|v| v.as_str()),
                    parsed.get("text").and_then(|v| v.as_str()),
                ) {
                    // Only append if it matches our subscribed thread
                    if self.chat_thread.as_deref() == Some(thread) {
                        self.chat_messages.push(ChatMessage {
                            sender: "ai".to_string(),
                            text: text.to_string(),
                            timestamp: Some(chrono::Utc::now().to_rfc3339()),
                        });
                        self.chat_scroll = 0;
                        self.chat_awaiting_response = false;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Runs a WebSocket client in a background task with auto-reconnect.
/// Exponential backoff from 1s to 30s max.
async fn ws_client_task(
    url: String,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    event_tx: tokio::sync::mpsc::UnboundedSender<WsEvent>,
) {
    use futures_util::{SinkExt, StreamExt};

    let mut backoff = 1u64; // seconds

    'reconnect: loop {
        // Attempt connection
        let (ws_stream, _) = match tokio_tungstenite::connect_async(&url).await {
            Ok(v) => v,
            Err(e) => {
                let _ = event_tx.send(WsEvent::Error(format!("Connect failed: {e}")));
                // Wait for backoff before retrying, but check for clean shutdown
                let delay = std::cmp::min(backoff, 30);
                backoff = std::cmp::min(backoff * 2, 30);
                let sleep = tokio::time::sleep(tokio::time::Duration::from_secs(delay));
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        _ = &mut sleep => break,
                        cmd = cmd_rx.recv() => {
                            // Clean shutdown requested (user closed chat)
                            if cmd.is_none() {
                                break 'reconnect;
                            }
                        }
                    }
                }
                continue 'reconnect;
            }
        };

        // Reset backoff on successful connection
        backoff = 1;
        let _ = event_tx.send(WsEvent::Connected);

        let (mut write, mut read) = ws_stream.split();

        // Main message loop
        let connection_lost = loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            let _ = event_tx.send(WsEvent::Message(text));
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => {
                            break true;
                        }
                        Some(Err(e)) => {
                            let _ = event_tx.send(WsEvent::Error(format!("Read error: {e}")));
                            break true;
                        }
                        None => {
                            break true;
                        }
                        _ => {}
                    }
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(text) => {
                            if let Err(e) = write.send(
                                tokio_tungstenite::tungstenite::Message::Text(text)
                            ).await {
                                let _ = event_tx.send(WsEvent::Error(format!("Send error: {e}")));
                                break true;
                            }
                        }
                        None => break false, // Clean shutdown — cmd channel closed
                    }
                }
            }
        };

        if connection_lost {
            let _ = event_tx.send(WsEvent::Disconnected);
            // Backoff before reconnecting
            let delay = std::cmp::min(backoff, 30);
            backoff = std::cmp::min(backoff * 2, 30);
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
            // Continue reconnection loop
        } else {
            break; // Clean shutdown
        }
    }
}

pub async fn run(
    args: &DashboardArgs,
    initial_thread: Option<&str>,
    initial_channel: Option<&str>,
) -> Result<()> {
    let mut client = InspectClient::new(&args.addr);

    // Setup terminal
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;

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
            app.open_chat(&args.addr, initial_channel, Some(thread));
        }

        loop {
            // Poll for new state
            if last_poll.elapsed() >= poll_interval {
                match client.get_state().await {
                    Ok(state) => {
                        // Clear chat_awaiting_response once the server confirms the thread
                        // is no longer processing (with a small grace period to avoid
                        // flicker between the local flag and server state).
                        if app.chat_awaiting_response
                            && let Some(ref chat_name) = app.chat_thread
                        {
                            let ct = state.threads.iter().find(|t| t.name == *chat_name);
                            if let Some(ct) = ct
                                && ct.status != ThreadStatus::Processing
                            {
                                app.chat_awaiting_response = false;
                            }
                        }
                        app.state = Some(state);
                        app.error = None;
                    }
                    Err(e) => {
                        app.error = Some(format!("{e:#}"));
                    }
                }
                last_poll = std::time::Instant::now();
            }

            // Check for WebSocket events
            while let Ok(event) = app.ws_rx.try_recv() {
                app.handle_ws_event(event);
            }

            // Clear expired status messages
            app.tick_status();

            // Draw
            terminal.draw(|f| ui(f, &mut app))?;

            // Handle input (non-blocking, 50ms timeout)
            if event::poll(Duration::from_millis(50))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                if app.chat_visible {
                    handle_chat_keys(&mut app, key);
                } else {
                    handle_normal_keys(&mut app, key, &mut client, &mut last_poll, &args.addr)
                        .await;
                }
            }

            if app.should_quit {
                break Ok(());
            }
        }
    }; // terminal + backend dropped here

    // Restore terminal — safe now that no buffered escape codes remain
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
                 cannot open as '{}'. Use 'jyc dashboard open -t {} -p {}' instead",
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
    let (mut ws_stream, _) = tokio_tungstenite::connect_async(&url)
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

/// Format elapsed time from an RFC 3339 timestamp to now.
/// Returns a string like "15s" or "2m" or "" if parsing fails.
fn format_elapsed(timestamp: &Option<String>) -> String {
    let ts = match timestamp {
        Some(t) => t,
        None => return String::new(),
    };
    let parsed = match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(_) => return String::new(),
    };
    let elapsed = chrono::Utc::now().signed_duration_since(parsed);
    let secs = elapsed.num_seconds();
    if secs < 0 {
        return String::new();
    }
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m", secs / 60)
    }
}

/// Format a message timestamp for the chat group header (╭─ line).
/// Shows "HH:MM" for today, "MM-DD HH:MM" for other dates.
fn format_msg_time(ts: &Option<String>) -> String {
    let ts = match ts {
        Some(t) => t,
        None => return String::new(),
    };
    let parsed = match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(dt) => dt.with_timezone(&chrono::Local),
        Err(_) => return String::new(),
    };
    let now = chrono::Local::now();
    if parsed.date_naive() == now.date_naive() {
        parsed.format("%H:%M").to_string()
    } else {
        parsed.format("%m-%d %H:%M").to_string()
    }
}

/// Format elapsed time between two RFC 3339 timestamps for the chat group
/// footer (╰─ line). Falls back to now if `end` is None.
fn format_group_elapsed(start: &Option<String>, end: &Option<String>) -> String {
    let start_ts = match start {
        Some(t) => t,
        None => return String::new(),
    };
    let start_dt = match chrono::DateTime::parse_from_rfc3339(start_ts) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(_) => return String::new(),
    };
    let end_dt = match end {
        Some(t) => match chrono::DateTime::parse_from_rfc3339(t) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(_) => return String::new(),
        },
        None => chrono::Utc::now(),
    };
    let elapsed = end_dt.signed_duration_since(start_dt);
    let secs = elapsed.num_seconds();
    if secs <= 0 {
        return String::new();
    }
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m", secs / 60)
    }
}

/// Count the number of visual lines when `text` is wrapped within
/// `available_width`, accounting for the 4-character prefix ("▶ > " or
/// "    ") that each logical line starts with.
fn count_wrapped_lines(text: &str, available_width: u16) -> usize {
    let width = available_width as usize;
    let prefix_width = 4usize;
    let first_row_content = width.saturating_sub(prefix_width).max(1);

    text.split('\n')
        .map(|line| {
            let w = line.width();
            if w <= first_row_content {
                1
            } else {
                1 + (w - first_row_content).div_ceil(width)
            }
        })
        .sum::<usize>()
        .max(1)
}

/// Move the cursor up or down one line within a multi-line string.
///
/// `down = true` moves down, `false` moves up. If the cursor is on the
/// first/last line, it moves to the start/end of that line respectively.
fn move_cursor_vertically(input: &str, cursor: &mut usize, down: bool) {
    // Find the start of the current line
    let line_start = input[..*cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let line_end = input[*cursor..]
        .find('\n')
        .map(|p| *cursor + p)
        .unwrap_or(input.len());
    let col = *cursor - line_start;

    if down {
        // Find the next line
        if line_end < input.len() {
            let next_start = line_end + 1;
            let next_end = input[next_start..]
                .find('\n')
                .map(|p| next_start + p)
                .unwrap_or(input.len());
            *cursor = next_start + col.min(next_end - next_start);
        } else {
            // Already on last line, move to end
            *cursor = input.len();
        }
    } else {
        // Find the previous line
        if line_start > 0 {
            let prev_end = line_start - 1; // position of the \n
            let prev_start = input[..prev_end].rfind('\n').map(|p| p + 1).unwrap_or(0);
            *cursor = prev_start + col.min(prev_end - prev_start);
        } else {
            // Already on first line, move to start
            *cursor = 0;
        }
    }
}

fn handle_chat_keys(app: &mut App, key: event::KeyEvent) {
    // Ctrl+Q quits the entire dashboard (consistent across all modes)
    let is_ctrl_q = key.code == KeyCode::Char('q') && key.modifiers.contains(KeyModifiers::CONTROL);

    if is_ctrl_q {
        app.should_quit = true;
        return;
    }

    // Ctrl+W cycles activity pane split ratio
    let is_ctrl_w = key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_w && app.chat_phase == ChatPhase::Chatting {
        app.activity_split = (app.activity_split + 1) % 4;
        return;
    }

    // Ctrl+C sends /cancel to stop the current AI processing
    let is_ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
    if is_ctrl_c && app.chat_phase == ChatPhase::Chatting {
        app.send_raw_message("/cancel");
        app.set_status("⏹ Cancelled".to_string());
        app.chat_awaiting_response = false;
        return;
    }

    // Shift+Tab toggles between plan and build mode
    let is_shift_tab = key.code == KeyCode::BackTab;
    if is_shift_tab && app.chat_phase == ChatPhase::Chatting {
        let current_mode = app
            .state
            .as_ref()
            .and_then(|s| {
                let chat_name = app.chat_thread.as_deref()?;
                s.threads.iter().find(|t| t.name == chat_name)
            })
            .and_then(|t| t.mode.clone())
            .unwrap_or_else(|| "build".to_string());

        if current_mode == "plan" {
            app.send_raw_message("/build");
            app.set_status("Switched to build mode".to_string());
        } else {
            app.send_raw_message("/plan");
            app.set_status("Switched to plan mode".to_string());
        }
        app.chat_awaiting_response = true;
        return;
    }

    match app.chat_phase {
        ChatPhase::PatternSelect => match key.code {
            KeyCode::Esc => {
                app.close_chat();
            }
            KeyCode::Up => {
                if app.chat_pattern_selected > 0 {
                    app.chat_pattern_selected -= 1;
                }
            }
            KeyCode::Down => {
                if app.chat_pattern_selected + 1 < app.chat_patterns.len() {
                    app.chat_pattern_selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(pattern) = app.chat_patterns.get(app.chat_pattern_selected) {
                    let pattern = pattern.clone();
                    app.select_pattern(pattern);
                }
            }
            _ => {}
        },
        ChatPhase::Chatting => match key.code {
            KeyCode::Enter if app.chat_focus == ChatFocus::ChatPane => {
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT)
                {
                    // Insert newline for multi-line input
                    app.chat_input.insert(app.chat_cursor, '\n');
                    app.chat_cursor += 1;
                } else if !app.chat_input.trim().is_empty() {
                    let now = std::time::Instant::now();
                    let is_fast_enter = app
                        .last_enter_at
                        .map(|t| now.duration_since(t) < std::time::Duration::from_millis(300))
                        .unwrap_or(false);
                    app.last_enter_at = Some(now);
                    if is_fast_enter {
                        // Rapid consecutive Enter (e.g. pasting multi-line text).
                        // Insert newline instead of sending to prevent each
                        // pasted line from being sent as a separate message.
                        app.chat_input.insert(app.chat_cursor, '\n');
                        app.chat_cursor += 1;
                    } else {
                        app.send_chat_message();
                    }
                }
            }
            KeyCode::Esc => {
                app.go_to_pattern_select();
            }
            KeyCode::Tab => {
                app.toggle_focus();
            }
            KeyCode::Up => {
                if app.chat_focus == ChatFocus::ChatPane && app.chat_input.contains('\n') {
                    // Move cursor up one line within multi-line input
                    move_cursor_vertically(&app.chat_input, &mut app.chat_cursor, false);
                } else {
                    app.scroll_up();
                }
            }
            KeyCode::Down => {
                if app.chat_focus == ChatFocus::ChatPane && app.chat_input.contains('\n') {
                    // Move cursor down one line within multi-line input
                    move_cursor_vertically(&app.chat_input, &mut app.chat_cursor, true);
                } else {
                    app.scroll_down();
                }
            }
            KeyCode::PageUp => {
                app.page_up();
            }
            KeyCode::PageDown => {
                app.page_down();
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.page_up();
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.page_down();
            }
            KeyCode::Left if app.chat_focus == ChatFocus::ChatPane => {
                app.chat_cursor = app
                    .chat_input
                    .floor_char_boundary(app.chat_cursor.saturating_sub(1));
            }
            KeyCode::Left if app.chat_focus == ChatFocus::ActivityPane => {
                app.activity_hscroll = app.activity_hscroll.saturating_sub(1);
            }
            KeyCode::Right
                if app.chat_focus == ChatFocus::ChatPane
                    && app.chat_cursor < app.chat_input.len() =>
            {
                app.chat_cursor = app.chat_input.ceil_char_boundary(app.chat_cursor + 1);
            }
            KeyCode::Right if app.chat_focus == ChatFocus::ActivityPane => {
                app.activity_hscroll = app.activity_hscroll.saturating_add(1);
            }
            _ => {
                if app.chat_focus == ChatFocus::ChatPane {
                    match key.code {
                        KeyCode::Char(c) => {
                            app.chat_input.insert(app.chat_cursor, c);
                            app.chat_cursor += c.len_utf8();
                        }
                        KeyCode::Backspace if app.chat_cursor > 0 => {
                            let prev = app.chat_input.floor_char_boundary(app.chat_cursor - 1);
                            app.chat_input.remove(prev);
                            app.chat_cursor = prev;
                        }
                        _ => {}
                    }
                }
            }
        },
    }
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
        KeyCode::Esc => {
            app.should_quit = true;
        }
        KeyCode::Char('c') => {
            app.open_chat(addr, None, None);
        }
        KeyCode::Enter => {
            // Enter chat directly when a websocket thread is selected
            let thread_info = app.state.as_ref().and_then(|s| {
                app.table_state
                    .selected()
                    .and_then(|i| s.threads.get(i))
                    .and_then(|t| {
                        let is_ws = s
                            .channels
                            .iter()
                            .find(|c| c.name == t.channel)
                            .is_some_and(|c| c.channel_type == "websocket");
                        if is_ws {
                            Some((t.name.clone(), t.channel.clone()))
                        } else {
                            None
                        }
                    })
            });
            if let Some((name, channel)) = thread_info {
                app.open_chat(addr, Some(&channel), Some(&name));
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

    if app.chat_visible {
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

fn ui_chat_mode(frame: &mut Frame, area: Rect, app: &mut App) {
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Channels bar
            Constraint::Length(1), // Compact info bar
            Constraint::Min(0),    // Content (chat + optional activity)
            Constraint::Length(1), // Status bar
        ])
        .split(area);

    render_channels(frame, main_chunks[0], app);
    render_compact_info(frame, main_chunks[1], app);

    match app.chat_phase {
        ChatPhase::PatternSelect => {
            render_pattern_select(frame, main_chunks[2], app);
        }
        ChatPhase::Chatting => {
            match app.activity_split {
                1 => {
                    // 100/0 — full chat, no activity pane
                    render_chat_conversation(frame, main_chunks[2], app);
                }
                2 => {
                    // 20/80 — activity dominant
                    let content = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(20), Constraint::Percentage(80)])
                        .split(main_chunks[2]);
                    render_chat_conversation(frame, content[0], app);
                    render_activity_log(frame, content[1], app);
                }
                3 => {
                    // 0/100 — full activity
                    render_activity_log(frame, main_chunks[2], app);
                }
                _ => {
                    // 0 — 80/20 (default)
                    let content = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(80), Constraint::Percentage(20)])
                        .split(main_chunks[2]);
                    render_chat_conversation(frame, content[0], app);
                    render_activity_log(frame, content[1], app);
                }
            }
        }
    }

    render_status_bar(frame, main_chunks[3], app);
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

fn render_compact_info(frame: &mut Frame, area: Rect, app: &App) {
    let state = match &app.state {
        Some(s) => s,
        None => {
            let text = Paragraph::new("");
            frame.render_widget(text, area);
            return;
        }
    };

    let selected = if app.chat_visible && app.chat_phase == ChatPhase::Chatting {
        app.chat_thread
            .as_ref()
            .and_then(|chat_name| state.threads.iter().find(|t| t.name == *chat_name))
    } else {
        app.table_state
            .selected()
            .and_then(|i| state.threads.get(i))
    };

    let text = if let Some(t) = selected {
        let mut spans = vec![
            Span::styled("Thread: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&t.name),
            Span::raw(" | "),
            Span::styled("Channel: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&t.channel),
            Span::raw(" | "),
            Span::styled("Pattern: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(t.pattern.as_deref().unwrap_or("-")),
        ];
        if let Some(ref model) = t.model {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                "Model: ",
                Style::default().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(model));
        }
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(
            "Mode: ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(t.mode.as_deref().unwrap_or("build")));
        if let (Some(cur), Some(max)) = (t.input_tokens, t.max_tokens) {
            let pct = if max > 0 {
                cur.checked_mul(100)
                    .and_then(|v| v.checked_div(max))
                    .unwrap_or(0)
            } else {
                0
            };
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                "Tokens: ",
                Style::default().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(format!("{cur} / {max} ({pct}%)")));
        }
        if t.status == ThreadStatus::Processing {
            spans.push(Span::raw(" | "));
            spans.push(Span::styled(
                "⏳ AI thinking...",
                Style::default().fg(Color::Yellow),
            ));
        }
        Line::from(spans)
    } else {
        Line::from("Select a thread with ↑/↓")
    };

    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, area);
}

fn render_pattern_select(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(" Select Pattern ")
        .borders(Borders::ALL);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.chat_patterns.is_empty() {
        let text = Paragraph::new(Span::styled(
            "  No patterns available",
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(text, inner);
        return;
    }

    let lines: Vec<Line> = app
        .chat_patterns
        .iter()
        .enumerate()
        .map(|(i, pattern)| {
            if i == app.chat_pattern_selected {
                Line::from(vec![Span::styled(
                    format!("> {pattern}"),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )])
            } else {
                Line::from(vec![Span::raw("  "), Span::raw(pattern)])
            }
        })
        .collect();

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: true });
    frame.render_widget(paragraph, inner);
}

fn render_chat_conversation(frame: &mut Frame, area: Rect, app: &mut App) {
    let title = format!(" Chat: {} ", app.chat_thread.as_deref().unwrap_or("-"));
    let mut block = Block::default().title(title).borders(Borders::ALL);
    if app.chat_focus == ChatFocus::ChatPane {
        block = block.border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split: scrollable messages (top) + dynamic input area (bottom)
    // Input area grows with content (up to 5 lines) for multi-line editing.
    // Count visual lines including wrapped lines (not just explicit newlines).
    let input_line_count = count_wrapped_lines(&app.chat_input, inner.width).clamp(1, 5) as u16;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(input_line_count)])
        .split(inner);

    // --- Messages area (markdown-rendered with colored bars) ---
    let renderer = ratatui_markdown::markdown::MarkdownRenderer::new(chunks[0].width as usize);
    let theme = ratatui_markdown::theme::ThemeConfig::default();

    let mut all_lines: Vec<Line> = Vec::new();

    let dim_style = Style::default().fg(Color::DarkGray);
    let mut box_open = false;
    let mut group_start_ts: Option<String> = None;

    for (idx, msg) in app.chat_messages.iter().enumerate() {
        let is_user = msg.sender == "user";
        let prefix = if is_user { "**You:** " } else { "**AI:** " };

        let prev_sender = if idx > 0 {
            Some(app.chat_messages[idx - 1].sender.as_str())
        } else {
            None
        };

        // Close previous group box on AI → user transition
        if is_user && prev_sender == Some("ai") && box_open {
            all_lines.push(Line::from(vec![Span::styled("│", dim_style)]));
            let last_ts = app
                .chat_messages
                .get(idx - 1)
                .and_then(|m| m.timestamp.clone());
            let elapsed = format_group_elapsed(&group_start_ts, &last_ts);
            let width = chunks[0].width as usize;
            let close_spans = if elapsed.is_empty() {
                let dashes = "─".repeat(width.saturating_sub(1));
                vec![Span::styled(format!("╰{dashes}"), dim_style)]
            } else {
                // ╰─── 12s ──
                let dash_count = width.saturating_sub(2 + elapsed.len() + 4); // ╰ + " " + elapsed + " " + "──"
                vec![
                    Span::styled(format!("╰{} ", "─".repeat(dash_count)), dim_style),
                    Span::styled(elapsed, dim_style),
                    Span::styled(" ──", dim_style),
                ]
            };
            all_lines.push(Line::from(close_spans));
            all_lines.push(Line::from(""));
            box_open = false;
            group_start_ts = None;
        }

        // Open new group box at the start of a user turn
        if is_user && !box_open {
            group_start_ts = msg.timestamp.clone();
            let time_str = format_msg_time(&msg.timestamp);
            let width = chunks[0].width as usize;
            let open_spans = if time_str.is_empty() {
                let dashes = "─".repeat(width.saturating_sub(2));
                vec![Span::styled(format!("╭─{}", dashes), dim_style)]
            } else {
                let used = 3 + time_str.len() + 1; // "╭─ " + time_str + " "
                let dash_count = width.saturating_sub(used);
                vec![
                    Span::styled("╭─ ", dim_style),
                    Span::styled(time_str, dim_style),
                    Span::styled(format!(" {}", "─".repeat(dash_count)), dim_style),
                ]
            };
            all_lines.push(Line::from(open_spans));
            box_open = true;
        }

        // Separator line between user and AI within a group
        if !is_user && prev_sender == Some("user") {
            let width = chunks[0].width as usize;
            let sep = format!("├{}", "─".repeat(width.saturating_sub(1)));
            let sep_style = Style::default().fg(Color::DarkGray);
            all_lines.push(Line::from(vec![Span::styled(sep, sep_style)]));
        }

        // Render message: all bars use the same dim style
        let bar_style = dim_style;
        let md_text = format!("{prefix}{}\n", msg.text);
        let blocks = renderer.parse(&md_text);
        let msg_lines = renderer.render(&blocks, &theme);

        for line in msg_lines {
            let bar_span = Span::styled("│ ", bar_style);
            let spans: Vec<Span> = std::iter::once(bar_span).chain(line).collect();
            all_lines.push(Line::from(spans));
        }
    }

    // Close any open box at the end
    if box_open {
        all_lines.push(Line::from(vec![Span::styled("│", dim_style)]));
        let last_ts = app.chat_messages.last().and_then(|m| m.timestamp.clone());
        let elapsed = format_group_elapsed(&group_start_ts, &last_ts);
        let width = chunks[0].width as usize;
        let close_spans = if elapsed.is_empty() {
            let dashes = "─".repeat(width.saturating_sub(1));
            vec![Span::styled(format!("╰{dashes}"), dim_style)]
        } else {
            // ╰─── 12s ──
            let dash_count = width.saturating_sub(2 + elapsed.len() + 4);
            vec![
                Span::styled(format!("╰{} ", "─".repeat(dash_count)), dim_style),
                Span::styled(elapsed, dim_style),
                Span::styled(" ──", dim_style),
            ]
        };
        all_lines.push(Line::from(close_spans));
        all_lines.push(Line::from(""));
    }

    // Show progress indicator
    // Determine if the thread is processing: either the inspect server
    // reports Processing status, or we've sent a message and haven't yet
    // seen the server confirm completion (covers the first-message gap
    // where the poll hasn't caught up yet).
    let server_processing = app
        .state
        .as_ref()
        .and_then(|s| {
            let chat_name = app.chat_thread.as_deref()?;
            s.threads.iter().find(|t| t.name == chat_name)
        })
        .is_some_and(|ct| ct.status == ThreadStatus::Processing);

    // Show progress if the server reports processing OR we've sent a message
    // locally and are still waiting for the server state to catch up.
    let show_progress = server_processing || app.chat_awaiting_response;

    if show_progress {
        // Try to get activity entries from the server
        let activity_entries: Vec<_> = app
            .state
            .as_ref()
            .and_then(|s| {
                let chat_name = app.chat_thread.as_deref()?;
                s.threads.iter().find(|t| t.name == chat_name)
            })
            .filter(|ct| ct.status == ThreadStatus::Processing)
            .map(|ct| ct.activity.iter().rev().take(2).collect::<Vec<_>>())
            .unwrap_or_default();

        if activity_entries.is_empty() {
            all_lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "⏳ AI is thinking...",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        } else {
            let total = activity_entries.len();
            for (idx, a) in activity_entries.iter().rev().enumerate() {
                let is_last = idx == total - 1;
                let elapsed = if is_last {
                    format_elapsed(&a.timestamp)
                } else {
                    String::new()
                };
                let style = Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::ITALIC);

                // Split multi-line entries (e.g. edit diff) into separate lines.
                // Try parsing as JSON first — edit events store full data as JSON.
                let rendered_lines: Vec<String> = if let Ok(json) =
                    serde_json::from_str::<serde_json::Value>(&a.text)
                    && json.get("type").and_then(|t| t.as_str()) == Some("edit")
                {
                    let file_path = json
                        .get("file_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let line_no = json.get("line_no").and_then(|v| v.as_u64());
                    let old_str = json
                        .get("old_string")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let new_str = json
                        .get("new_string")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let location = match line_no {
                        Some(n) => format!("{file_path}:{n}"),
                        None => file_path.to_string(),
                    };
                    let mut out = Vec::new();
                    // Header line
                    if is_last {
                        if elapsed.is_empty() {
                            out.push(format!("⏳ {location}"));
                        } else {
                            out.push(format!("⏳ {location} {elapsed}"));
                        }
                    } else {
                        out.push(format!("   {location}"));
                    }
                    // Old lines
                    for line in old_str.split('\n') {
                        out.push(format!("  -{line}"));
                    }
                    // New lines
                    for line in new_str.split('\n') {
                        out.push(format!("  +{line}"));
                    }
                    out
                } else if let Ok(json) = serde_json::from_str::<serde_json::Value>(&a.text)
                    && json.get("type").and_then(|t| t.as_str()) == Some("write")
                {
                    let file_path = json
                        .get("file_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let content = json.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    let mut out = Vec::new();
                    // Header line
                    if is_last {
                        if elapsed.is_empty() {
                            out.push(format!("⏳ {file_path}"));
                        } else {
                            out.push(format!("⏳ {file_path} {elapsed}"));
                        }
                    } else {
                        out.push(format!("   {file_path}"));
                    }
                    // Content lines (truncated to avoid flooding the pane)
                    let content_lines: Vec<&str> = content.split('\n').collect();
                    let max_lines = 20;
                    for line in content_lines.iter().take(max_lines) {
                        out.push(format!("  +{line}"));
                    }
                    if content_lines.len() > max_lines {
                        out.push(format!(
                            "  … ({} more lines)",
                            content_lines.len() - max_lines
                        ));
                    }
                    out
                } else {
                    // Plain text — split by newlines for display
                    let lines: Vec<&str> = a.text.split('\n').collect();
                    lines
                        .iter()
                        .enumerate()
                        .map(|(line_idx, line)| {
                            if line_idx == 0 && is_last {
                                if elapsed.is_empty() {
                                    format!("⏳ {line}")
                                } else {
                                    format!("⏳ {line} {elapsed}")
                                }
                            } else {
                                // Pad with 3 spaces to visually align with "⏳ "
                                format!("   {line}")
                            }
                        })
                        .collect()
                };

                for label in rendered_lines {
                    let label_style = if label.starts_with("  -") {
                        Style::default().fg(Color::Gray)
                    } else {
                        style
                    };
                    all_lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(label, label_style),
                    ]));
                }
            }
        }
    }

    let inner_height = chunks[0].height as usize;
    let max_skip = all_lines.len().saturating_sub(inner_height);
    app.chat_scroll = app.chat_scroll.min(max_skip);
    let skip = max_skip.saturating_sub(app.chat_scroll);
    let visible_lines: Vec<Line> = all_lines.into_iter().skip(skip).collect();

    let messages_para = Paragraph::new(visible_lines);
    frame.render_widget(messages_para, chunks[0]);

    // --- Input area (multi-line, at bottom) ---
    let focus_indicator = if app.chat_focus == ChatFocus::ChatPane {
        "▶ "
    } else {
        "  "
    };
    let prompt_span = Span::styled("> ", Style::default().fg(Color::Yellow));

    // Build input lines from chat_input, splitting on '\n'.
    // The cursor block is inserted at the cursor position.
    let before_cursor = &app.chat_input[..app.chat_cursor];
    let after_cursor = &app.chat_input[app.chat_cursor..];

    let mut input_lines: Vec<Line> = Vec::new();
    let before_lines: Vec<&str> = before_cursor.split('\n').collect();
    let after_lines: Vec<&str> = after_cursor.split('\n').collect();

    // The cursor is at the boundary between before_lines (last) and after_lines (first).
    // before_cursor ends at the end of before_lines' last element.
    // after_cursor starts at the beginning of after_lines' first element.
    let n_before = before_lines.len();
    let n_after = after_lines.len();
    let total_lines = n_before + n_after - 1; // they share the cursor line

    for (i, _) in (0..total_lines).enumerate() {
        let mut spans: Vec<Span> = Vec::new();
        if i == 0 {
            spans.push(Span::raw(focus_indicator));
            spans.push(prompt_span.clone());
        } else {
            spans.push(Span::raw("    ")); // indent to align after "▶ > "
        }

        if i < n_before - 1 {
            // Full line before cursor line
            spans.push(Span::raw(before_lines[i]));
        } else if i == n_before - 1 && n_after == 1 {
            // Cursor line: before + cursor + after (single line)
            spans.push(Span::raw(before_lines[i]));
            spans.push(Span::styled("▌", Style::default().fg(Color::Yellow)));
            spans.push(Span::raw(after_lines[0]));
        } else if i == n_before - 1 {
            // Cursor line: before + cursor (after has more lines)
            spans.push(Span::raw(before_lines[i]));
            spans.push(Span::styled("▌", Style::default().fg(Color::Yellow)));
        } else {
            // After cursor lines
            let after_idx = i - n_before + 1;
            spans.push(Span::raw(after_lines[after_idx]));
        }

        input_lines.push(Line::from(spans));
    }

    // When input exceeds the visible area, scroll to keep the cursor visible.
    // Calculate using visual (wrapped) lines rather than logical lines.
    let width = inner.width;
    let visible_input_lines = input_line_count as usize;
    let total_visual_lines = count_wrapped_lines(&app.chat_input, width);

    // Determine the cursor's visual line (0-indexed)
    // Visual lines from logical lines before the cursor's line
    let visual_before: usize = before_lines[..before_lines.len() - 1]
        .iter()
        .map(|l| {
            let lw = (*l).width();
            let w = width as usize;
            let first_row = w.saturating_sub(4).max(1);
            if lw <= first_row {
                1
            } else {
                1 + (lw - first_row).div_ceil(w)
            }
        })
        .sum();
    // Cursor's visual row within its logical line
    let w = width as usize;
    let first_row_content = w.saturating_sub(4).max(1);
    let cursor_col_w = before_lines.last().unwrap_or(&"").width();
    let cursor_row_within = if cursor_col_w > first_row_content {
        1 + (cursor_col_w - first_row_content) / w
    } else {
        0
    };
    let cursor_visual_line = visual_before + cursor_row_within;

    let input_scroll = if total_visual_lines > visible_input_lines {
        if cursor_visual_line < visible_input_lines {
            0
        } else {
            cursor_visual_line - visible_input_lines + 1
        }
    } else {
        0
    };

    let input_para = Paragraph::new(input_lines)
        .wrap(Wrap { trim: true })
        .scroll((input_scroll as u16, 0));
    frame.render_widget(input_para, chunks[1]);
}

fn render_activity_log(frame: &mut Frame, area: Rect, app: &mut App) {
    let state = match &app.state {
        Some(s) => s,
        None => {
            let block = Block::default().title(" Activity ").borders(Borders::ALL);
            frame.render_widget(block, area);
            return;
        }
    };

    let selected = if app.chat_visible && app.chat_phase == ChatPhase::Chatting {
        app.chat_thread
            .as_ref()
            .and_then(|chat_name| state.threads.iter().find(|t| t.name == *chat_name))
    } else {
        app.table_state
            .selected()
            .and_then(|i| state.threads.get(i))
    };

    let selected = match selected {
        Some(t) => t,
        None => {
            let block = Block::default().title(" Activity ").borders(Borders::ALL);
            frame.render_widget(block, area);
            return;
        }
    };

    let focused = app.chat_visible && app.chat_focus == ChatFocus::ActivityPane;
    let inner_height = area.height.saturating_sub(2) as usize; // subtract borders
    let max_skip = selected.activity.len().saturating_sub(inner_height);
    app.activity_scroll = app.activity_scroll.min(max_skip);
    render_activity_log_inner(
        frame,
        area,
        selected,
        app.activity_scroll,
        app.activity_hscroll,
        focused,
    );
}

fn render_activity_log_inner(
    frame: &mut Frame,
    area: Rect,
    selected: &jyc_types::ThreadInfo,
    scroll_offset: usize,
    hscroll: usize,
    focused: bool,
) {
    let mut block = Block::default().title(" Activity ").borders(Borders::ALL);
    if focused {
        block = block.border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    }

    if selected.activity.is_empty() {
        let text = Paragraph::new(Span::styled(
            "  No activity",
            Style::default().fg(Color::DarkGray),
        ))
        .block(block);
        frame.render_widget(text, area);
        return;
    }

    let inner_height = area.height.saturating_sub(2) as usize; // subtract borders
    let max_skip = selected.activity.len().saturating_sub(inner_height);
    let skip = max_skip.saturating_sub(scroll_offset);

    let activity_lines: Vec<Line> = selected
        .activity
        .iter()
        .skip(skip)
        .map(|entry| {
            let time_str = entry
                .timestamp
                .as_deref()
                .and_then(|ts| {
                    chrono::DateTime::parse_from_rfc3339(ts)
                        .ok()
                        .map(|dt| dt.format("%H:%M:%S").to_string())
                })
                .unwrap_or_else(|| "-".to_string());
            let text_style = match entry.severity {
                Severity::Error => Style::default().fg(Color::Red),
                Severity::Warning => Style::default().fg(Color::Yellow),
                Severity::Info => Style::default(),
            };
            Line::from(vec![
                Span::styled(
                    format!("  {time_str} "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(&entry.text, text_style),
            ])
        })
        .collect();

    let text = Paragraph::new(activity_lines)
        .block(block)
        .scroll((0, hscroll as u16));
    frame.render_widget(text, area);
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let help_text = if app.chat_visible {
        match app.chat_phase {
            ChatPhase::PatternSelect => "[↑↓]select [Enter]choose [Esc]back [^Q]quit",
            ChatPhase::Chatting => {
                "[Tab]focus [↑↓]scroll [PgUp/PgDn ^F/^B]page [←→]cursor [^C]cancel [⇧Tab]mode [^W]split [Esc]back [^Q]quit"
            }
        }
    } else {
        "[^Q]quit [↑↓]select [Enter]chat [r]refresh [R]reload [s]reset [c]new"
    };

    let state = match &app.state {
        Some(s) => s,
        None => {
            let bar = Paragraph::new(format!(" {help_text}"))
                .style(Style::default().bg(Color::DarkGray).fg(Color::White));
            frame.render_widget(bar, area);
            return;
        }
    };

    let uptime = format_duration(state.uptime_secs);
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
            uptime,
            state.version,
        ))
    };

    let bar = Paragraph::new(Line::from({
        let mut spans = vec![Span::raw(" ")];
        if app.chat_visible {
            if app.ws_connected {
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
    fn select_pattern_clears_chat_messages() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(rx);

        // Simulate messages from a previous thread
        app.chat_messages.push(ChatMessage {
            sender: "user".to_string(),
            text: "hello from thread A".to_string(),
            timestamp: None,
        });
        app.chat_messages.push(ChatMessage {
            sender: "ai".to_string(),
            text: "reply from thread A".to_string(),
            timestamp: None,
        });
        assert_eq!(app.chat_messages.len(), 2);

        // Switch to a new thread
        app.select_pattern("thread-b".to_string());

        // Messages must be cleared so stale content doesn't leak across threads
        assert!(app.chat_messages.is_empty());
        assert_eq!(app.chat_thread.as_deref(), Some("thread-b"));
    }

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
