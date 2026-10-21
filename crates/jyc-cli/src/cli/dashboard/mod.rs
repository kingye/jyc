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
use jyc_core::duration::{DurationStyle, format_duration_secs};
use ratatui::{
    Frame, Terminal,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Padding, Paragraph, Row, Table, TableState, Wrap},
};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};
use std::cell::RefCell;
use std::io::stdout;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::Command;

use unicode_width::UnicodeWidthStr;

use jyc_inspect::client::InspectClient;
use jyc_types::{CommandInfo, InspectOverview, Severity, TopicStatus};

use super::command_popup::*;

mod chat;
mod hyperlink;
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

    /// Authorization token (defaults to `<data dir>/auth.token`)
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
    /// Detail for the one topic the screen shows, fetched by the poll loop
    /// from `GET /api/topics/{channel}/{topic}/info`.
    ///
    /// The topic's own figures — model, mode, branch, tokens, cost, latency,
    /// tasks, changed files, status — come from here, and from nowhere else:
    /// the overview poll carries the list's own fields (identity, status,
    /// context, last activity) and no more. The detail endpoint is also what
    /// `/info` reads, so the pane and the command report the same numbers.
    topic_detail: Option<jyc_types::TopicInfo>,
    error: Option<String>,
    table_state: TableState,
    should_quit: bool,
    status_message: Option<(String, std::time::Instant)>,
    /// Set by the explorer pane when it switches the chat to a new
    /// topic; the async poll loop picks it up and hydrates the live
    /// buffers so the chat pane shows the new topic's history.
    pending_hydrate: Option<(String, String)>,

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

    /// One-shot flag: the render loop calls `terminal.clear()` before the
    /// next draw, rebuilding ratatui's diff baseline. Set when the real
    /// screen may have changed out-of-band — e.g. while mouse capture was
    /// off, the wheel scrolls the terminal's own scrollback (or tmux
    /// copy-mode), moving content without ratatui knowing; the following
    /// diff repaint would ghost-mix old and new frames.
    needs_full_redraw: bool,

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

    /// Text-pane rectangles eligible for OSC 8 link detection, refreshed by
    /// the renderer every frame and shared with the terminal backend.
    link_regions: hyperlink::LinkRegions,
}

/// The topic the current screen shows, as `(channel, name)`.
///
/// The one screen rule — the chat screen answers for the topic its visible
/// chat is bound to, looked up through the shared
/// [`jyc_types::find_topic`] by name (the channel narrows it when the chat
/// knows one); otherwise it is the row under the dashboard cursor. Both the
/// renderers (`chat::selected_topic_detail`) and the detail fetch go through
/// this, so they cannot disagree about which topic is on screen.
/// Which pane owns the keys. One enum for both screens: the chat screen's
/// panes, plus [`Focus::TopicsTable`] for the overview's topic table. The
/// state lives in [`ChatState::focus`] — the panes it names are the chat
/// module's renderers on both screens, so the table is the only overview-only
/// entry and the two screens cannot disagree about what "focused" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The chat input field.
    ChatPane,
    /// The scrollable message area above the input field.
    MessageArea,
    /// The right-hand topic info pane (topic metadata + changed files).
    InfoPane,
    /// The activity log pane.
    ActivityPane,
    /// The left-side topic explorer pane.
    ExplorerPane,
    /// The overview's topic table.
    TopicsTable,
}

fn shown_topic_key(app: &App) -> Option<(String, String)> {
    let state = app.state.as_ref()?;
    let summary = if app.chat.visible {
        let name = app.chat.topic.as_deref()?;
        jyc_types::find_topic(&state.topics, name, app.chat.channel.as_deref())?
    } else {
        state.topics.get(app.table_state.selected()?)?
    };
    Some((summary.channel.clone(), summary.name.clone()))
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
            topic_detail: None,
            error: None,
            table_state: TableState::default(),
            should_quit: false,
            status_message: None,
            pending_hydrate: None,
            pending_reload_config: false,
            mouse_capture_enabled: true,
            needs_full_redraw: false,
            leader: None,
            token,
            overview_ws_tx: Some(overview_ws_cmd_tx),
            overview_ws_rx,
            overview_ws_target: None,
            chat: ChatState::new(ws_rx),
            link_regions: Rc::new(RefCell::new(Vec::new())),
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

    /// Compute `chat.commands` from the chat topic (the topic the user is
    /// typing into) and store it on `chat.commands` for the `/` popup.
    /// Called once when the `/` popup is about to open — the popup is the
    /// only reader, so we don't refresh on every poll cycle or every ↑/↓
    /// (which would just churn the log without changing what the user sees).
    ///
    /// `topic.commands` is the single source — the inspect server
    /// computes it server-side and the TUI just consumes it. Older
    /// servers (pre-PR-708) don't populate this field; we fall back to
    /// the built-in list so the popup is never silently empty in a
    /// mixed-version deployment.
    fn refresh_chat_commands(&mut self) {
        let chat_name = self.chat.topic.as_deref();
        let chat_channel = self.chat.channel.as_deref();
        let topic = chat_name.and_then(|name| {
            self.state
                .as_ref()
                .and_then(|s| jyc_types::find_topic(&s.topics, name, chat_channel))
        });
        let topic_commands = topic
            .map(|t| t.commands.clone())
            .filter(|cmds| !cmds.is_empty());
        let used_fallback = topic_commands.is_none();
        let mut final_commands = topic_commands.unwrap_or_else(jyc_core::command::all_commands);
        tracing::info!(
            chat_topic = ?self.chat.topic,
            server_topic_name = ?topic.map(|t| t.name.clone()),
            server_topic_pattern = ?topic.and_then(|t| t.pattern.clone()),
            server_commands = ?topic.map(|t| t.commands.iter().map(|c| c.name.clone()).collect::<Vec<_>>()),
            used_fallback,
            popup_commands = ?final_commands.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            "tui refresh_chat_commands"
        );
        // Mixed-version compat: a server older than `CommandInfo::args`
        // sends commands with no argument values, which would leave `/model`
        // without a picker. Refill it from the model list the overview
        // already carries — the same source the server's table reads.
        if let Some(state) = self.state.as_ref()
            && let Some(cmd) = final_commands
                .iter_mut()
                .find(|c| c.name == "/model" && c.args.is_empty())
        {
            cmd.args = state
                .models
                .iter()
                .map(|m| jyc_types::CommandArg {
                    value: m.name.clone(),
                    ..Default::default()
                })
                .collect();
        }
        self.chat.commands = final_commands;
    }

    fn handle_ws_event(&mut self, event: WsEvent) {
        match event {
            WsEvent::Connected => {
                self.chat.ws_connected = true;
                // After (re)connect, the server-side inspect_broadcast is
                // a fresh channel — the dedup state from the previous
                // session would drop every incoming event until events
                // catch up. Clear it and re-hydrate the current topic.
                self.chat.last_seen_id.clear();
                if let (Some(channel), Some(topic)) =
                    (self.chat.channel.clone(), self.chat.topic.clone())
                {
                    self.pending_hydrate = Some((channel, topic));
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

/// Arguments for the auto-started `jyc serve`.
///
/// Kept separate from the spawn so the shape is testable (spawning needs a
/// live address). `--data-dir` is skipped when it is the platform default
/// (`data_dir`) so the child uses the standard first-run provisioning path,
/// and `--pipe` is forwarded so bare `jyc --pipe` also starts the pipe.
fn serve_spawn_args(
    data_dir: &std::path::Path,
    default_data_dir: &std::path::Path,
    log_path: &std::path::Path,
    pipe: bool,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec!["serve".into()];
    if data_dir != default_data_dir {
        args.push("--data-dir".into());
        args.push(data_dir.into());
    }
    args.push("--log-file".into());
    args.push(log_path.into());
    if pipe {
        args.push("--pipe".into());
    }
    args
}

/// Auto-spawn `jyc serve` when it's not running, once per dashboard process.
///
/// The spawned `serve` writes its tracing logs to `<data dir>/jyc.log`
/// (same file as `/deploy`-started servers). Only works for localhost
/// addresses (the default).
async fn ensure_serve_running(addr: &str, data_dir: &std::path::Path, pipe: bool) -> Result<()> {
    // Only try to spawn once per dashboard session.
    static SPAWNED: AtomicBool = AtomicBool::new(false);

    // Quick check: server already up?
    if TcpStream::connect(addr).await.is_ok() {
        if pipe {
            // Someone else started this hub (systemd, compose, another
            // terminal) and `--pipe` cannot be retrofitted, so its
            // pipe-only channels still need a separate jyc-pipe.
            tracing::warn!(
                "A jyc serve is already running at {addr} and was not started with \
                 `--pipe`; its pipe-only channels need a separate jyc-pipe process"
            );
        }
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

    // Spawn jyc serve as a background child process. Pass --data-dir so the
    // auth token file is written to the same location the dashboard reads.
    // Skip --data-dir when it's the platform default (data_dir) so the
    // spawned serve uses the standard first-run provisioning path.
    // Pass --log-file so the auto-spawned server writes to the same log
    // file (`<data dir>/jyc.log`) as `/deploy`-started servers — no
    // separate plain `jyc.log` from stderr capture.
    tokio::fs::create_dir_all(data_dir)
        .await
        .with_context(|| format!("Failed to create data_dir {}", data_dir.display()))?;
    let exe = std::env::current_exe().context("Could not determine jyc binary path")?;
    let default_data_dir = jyc_utils::paths::data_dir().unwrap_or_default();
    let log_path = data_dir.join("jyc.log");
    let mut cmd = Command::new(&exe);
    cmd.args(serve_spawn_args(
        data_dir,
        &default_data_dir,
        &log_path,
        pipe,
    ));
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
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
    data_dir: &std::path::Path,
    initial_topic: Option<&str>,
    initial_channel: Option<&str>,
    pipe: bool,
) -> Result<()> {
    // Auto-spawn jyc serve FIRST - it writes <data dir>/auth.token on
    // startup, so the token file exists by the time we resolve it.
    ensure_serve_running(&args.addr, data_dir, pipe)
        .await
        .with_context(|| {
            format!(
                "Failed to connect to {}. Start jyc serve manually.",
                args.addr
            )
        })?;

    // Resolve the auth token: explicit flag/env wins, otherwise read it
    // from `<data dir>/auth.token` (which `jyc serve` writes on startup).
    let token = resolve_dashboard_token(args.token.as_deref(), data_dir)?;

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
        let link_regions: hyperlink::LinkRegions = Rc::new(RefCell::new(Vec::new()));
        let backend = hyperlink::HyperlinkBackend::new(stdout(), link_regions.clone());
        let mut terminal = Terminal::new(backend)?;

        let (_, ws_rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        let mut app = App::new(ws_rx, token);
        app.link_regions = link_regions;
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

        // Detail fetches ride the same way, for the one topic the screen shows:
        // `branch` and `changed_files` are the rows the overview no longer
        // carries (the `git` probe per topic per poll was the expensive part),
        // so they are refreshed on their own slower clock. A topic closed and
        // recreated under the same name maps to the same key — identity *is*
        // the name — so that case shows the old detail for at most one
        // refresh, and nothing can tell the two apart sooner.
        const DETAIL_REFRESH: Duration = Duration::from_secs(4);
        let (detail_tx, mut detail_rx) = tokio::sync::mpsc::unbounded_channel::<(
            (String, String),
            Result<jyc_types::TopicInfo>,
        )>();
        let mut detail_in_flight = false;
        let mut detail_key: Option<(String, String)> = None;
        let mut last_detail = std::time::Instant::now() - DETAIL_REFRESH;

        // If a topic was requested on the CLI, open chat directly.
        if let Some(topic) = initial_topic {
            let channel = initial_channel.unwrap_or("");
            app.chat
                .open(&args.addr, initial_channel, topic, app.token.clone());
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
            // Detail for the topic on screen: fetched when the shown topic
            // changes, and re-fetched on a slower clock so its branch and
            // changed files stay roughly current.
            let shown = shown_topic_key(&app);
            let needs_detail = !detail_in_flight
                && shown.is_some()
                && (shown != detail_key || last_detail.elapsed() >= DETAIL_REFRESH);
            if needs_detail && let Some(key) = shown {
                detail_in_flight = true;
                let client = client.clone();
                let tx = detail_tx.clone();
                tokio::spawn(async move {
                    let result = client.get_topic_info(&key.0, &key.1).await;
                    let _ = tx.send((key, result));
                });
            }
            if let Ok((key, result)) = detail_rx.try_recv() {
                detail_in_flight = false;
                detail_key = Some(key.clone());
                last_detail = std::time::Instant::now();
                match result {
                    Ok(info) => {
                        // Drop a detail the screen has already moved away from.
                        if shown_topic_key(&app) == Some(key) {
                            app.topic_detail = Some(info);
                        }
                    }
                    Err(e) => {
                        // Drop the detail rather than let a failing fetch keep
                        // an arbitrarily old branch and file list on screen.
                        if shown_topic_key(&app) == Some(key) {
                            app.topic_detail = None;
                        }
                        tracing::debug!(error = %e, "failed to fetch topic detail");
                    }
                }
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
                            let ct = jyc_types::find_topic(
                                &overview.topics,
                                chat_name,
                                app.chat.channel.as_deref(),
                            );
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
                        // Auto-select the first topic on initial load so the
                        // activity pane is populated immediately via the existing
                        // hydrate + ensure_overview_ws paths. The user can still
                        // navigate to a different topic (↑/↓).
                        if app.table_state.selected().is_none()
                            && !app.state.as_ref().is_some_and(|s| s.topics.is_empty())
                        {
                            app.table_state.select(Some(0));
                        }
                        // chat.commands is refreshed on demand when the `/`
                        // popup opens (see refresh_chat_commands); no need
                        // to re-sync here just because the topic list may
                        // have changed.

                        // Hydrate the live buffers when the table-selected
                        // topic changes (so the overview's activity pane
                        // shows recent entries without requiring the user
                        // to open chat first). Skip if we're in chat mode
                        // — the open() flow already triggered hydrate.
                        if !app.chat.visible {
                            let selected = app
                                .table_state
                                .selected()
                                .and_then(|idx| app.state.as_ref().and_then(|s| s.topics.get(idx)))
                                .map(|t| (t.channel.clone(), t.name.clone()));
                            if let Some((channel, topic)) = selected {
                                let key = (channel.clone(), topic.clone());
                                let needs_hydrate =
                                    app.chat.last_hydrated_key.as_ref() != Some(&key);
                                if needs_hydrate {
                                    app.chat.last_hydrated_key = Some(key);
                                }
                                if needs_hydrate {
                                    hydrate_live(&client, &mut app, &channel, &topic).await;
                                }
                                ensure_overview_ws(&mut app, &args.addr, &channel, &topic);
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

            // Flush a yank's clipboard write, if any. Here rather than in the key
            // handlers: those run inside `terminal.draw`, and OSC 52 has to write
            // to the real stdout.
            if let Some(text) = app.chat.pending_clipboard.take() {
                chat::clipboard::apply(&text);
            }

            // Draw
            if app.needs_full_redraw {
                app.needs_full_redraw = false;
                terminal.clear()?;
            }
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
                            if app.chat.visible && app.chat.focus == Focus::ChatPane =>
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
    data_dir: &std::path::Path,
    topic: Option<&str>,
    channel: Option<&str>,
    path: Option<&str>,
    explicit_token: Option<&str>,
    pipe: bool,
) -> Result<()> {
    // Auto-spawn jyc serve FIRST (writes <data dir>/auth.token on startup).
    ensure_serve_running(addr, data_dir, pipe)
        .await
        .with_context(|| format!("Failed to connect to {addr}. Start jyc serve manually."))?;

    let token = resolve_dashboard_token(explicit_token, data_dir)?;

    // Resolve topic path and name
    let path = resolve_topic_path(path)?;
    let topic = derive_topic_name(&path, topic);

    // If the directory was previously opened as a topic, the topic-name file
    // records the canonical name. Refuse to re-open it under a different name
    // to avoid diverging history and storage paths.
    check_existing_topic_name(&path, &topic, data_dir)?;

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
        data_dir,
        Some(&topic),
        Some(&channel),
        pipe,
    )
    .await
}

/// Resolve the dashboard authorization token from explicit input or data_dir.
fn resolve_dashboard_token(
    explicit: Option<&str>,
    data_dir: &std::path::Path,
) -> Result<Option<String>> {
    if let Some(token) = explicit {
        return Ok(Some(token.to_string()));
    }
    let path = jyc_utils::auth_token::token_path(data_dir);
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
/// storage paths. Also honors the adopted state dir (path-derived name) —
/// serve-side adoption at pin creation is authoritative; this is the CLI's
/// pre-flight guard over both candidate locations.
fn check_existing_topic_name(path: &str, topic: &str, data_dir: &std::path::Path) -> Result<()> {
    let p = std::path::Path::new(path);
    let derived =
        jyc_core::topic_path::state_dir_for(&jyc_core::topic_path::agents_dir(data_dir), p, None);
    for state_dir in [p.join(".jyc"), derived] {
        if !state_dir.is_dir() {
            continue;
        }
        let topic_name_file = state_dir.join("topic-name");
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
        if jyc_types::has_topic(&overview.topics, topic, Some(channel)) {
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
            .open(addr, Some(&channel), &name, app.token.clone());
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

    // `gg`: a second consecutive `g` jumps to the top, any other key resets
    // the sequence — the same state machine the chat runs, stepped on every
    // key so a half-typed `g` never survives, whichever pane owns the keys.
    let gg_jump = app.chat.gg_step(key.code == KeyCode::Char('g'));

    match app.chat.focus {
        // Keys the table answers. The table is the overview's cursor, so
        // Enter opens the highlighted topic and `j`/`k` move the cursor.
        Focus::TopicsTable => match key.code {
            KeyCode::Tab => cycle_overview_focus(app),
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
        },
        // The info and activity panes are the chat module's panes, so their
        // keys are the chat's: `scroll_pane_key` is the one key map and reads
        // `app.chat.focus`, the focus both screens share.
        _ => match key.code {
            KeyCode::Tab => cycle_overview_focus(app),
            KeyCode::PageUp => app.chat.page_up(),
            KeyCode::PageDown => app.chat.page_down(),
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.chat.page_up()
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.chat.page_down()
            }
            _ => {
                app.chat.scroll_pane_key(key, gg_jump);
            }
        },
    }
}

/// Move the overview's pane focus to the next pane: the topics table, then
/// the info pane, then the activity pane. All three are always on screen
/// here, so the cycle visits all three unconditionally — unlike the chat
/// screen's, which skips whatever `Ctrl+P` `i` / `a` has closed. The panes
/// themselves and the focus state are still the shared ones.
fn cycle_overview_focus(app: &mut App) {
    app.chat.focus = match app.chat.focus {
        Focus::TopicsTable => Focus::InfoPane,
        Focus::InfoPane => Focus::ActivityPane,
        // The chat screen's own panes: the overview never cycles into them,
        // and leaving the overview puts focus back on its table.
        Focus::ActivityPane | Focus::ChatPane | Focus::MessageArea | Focus::ExplorerPane => {
            Focus::TopicsTable
        }
    };
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
        if on {
            // While capture was off, scrolling went to the terminal's own
            // scrollback — the real screen no longer matches ratatui's
            // previous buffer. Repaint everything once (same pattern as
            // the editor-suspend resume, which also `terminal.clear()`s).
            app.needs_full_redraw = true;
        }
        app.set_status(if on {
            "Mouse capture: on".to_string()
        } else {
            "Mouse capture: off".to_string()
        });
    }
}

fn ui(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Link regions are re-registered by the active screen's renderer.
    app.link_regions.borrow_mut().clear();

    if app.chat.visible {
        ui_chat_mode(frame, area, app);
    } else {
        ui_normal_mode(frame, area, app);
    }
}

fn ui_normal_mode(frame: &mut Frame, area: Rect, app: &mut App) {
    // Main layout, the chat screen's shape without its channel row: the topics
    // table beside the topic info pane, and the activity pane across the
    // bottom. Both panes are the chat module's own renderers (see
    // `chat::render_topic_info_pane` and `chat::render_activity_log`), so the
    // overview shows the same panes instead of a second set of lookalikes.
    //
    // The channels bar is gone: it listed the channels *this* process runs,
    // which on a hub is the single synthesized `agents` channel — the real
    // channels live in the pipe process, and their state is not the hub's to
    // claim. What the bar still had to say (that the overview is unreachable,
    // or that its first poll has not landed) is the one line it may still take,
    // at the top and only when there is something to report.
    //
    // No bottom status bar: version, server stats and transient messages
    // render in the info pane's status sub-pane (see `chat_status_block`),
    // the same block the chat screen pins to the bottom of that column.
    //
    // Both panes are always here and the screen has no way to close them:
    // the visibility flags behind `Ctrl+P` `i` / `a` / `s` belong to the chat
    // screen, which is the only screen that offers those commands, so the
    // overview does not read them. What the two screens share is the renderers
    // and the focus, not the toggles.
    let line = status_line(app);
    let mut constraints = Vec::with_capacity(3);
    if line.is_some() {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(0));
    constraints.push(Constraint::Percentage(20));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    if let Some(ref line) = line {
        render_status_line(frame, chunks[0], line.clone());
    }
    let body = &chunks[usize::from(line.is_some())..];

    let middle =
        Layout::horizontal([Constraint::Percentage(80), Constraint::Percentage(20)]).split(body[0]);
    // One blank row above the table, the same top padding the info pane
    // carries, so neither column's content hugs the screen top.
    let topics = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(middle[0]);
    render_topics(frame, topics[1], app);
    render_topic_info_pane(frame, middle[1], app);
    render_activity_log(frame, body[1], app);

    // Leader-key popup overlay (dashboard + shared commands).
    if let Some(ref leader) = app.leader {
        leader.render(frame, area);
    }
}

/// Resting colour of every pane border, on both the chat and the overview
/// screen. The explorer's right border and the chat status block already drew
/// with it; the panes that left the style unset inherited the terminal default,
/// which made the separators on one screen disagree.
pub(super) const PANE_BORDER: Color = Color::DarkGray;

/// Border colour of the pane that currently owns the keys.
pub(super) const PANE_BORDER_FOCUSED: Color = Color::Cyan;

/// Border style for a pane: [`PANE_BORDER_FOCUSED`] while it has focus,
/// [`PANE_BORDER`] otherwise. The only place pane border colours are decided —
/// a pane that skipped it inherited the terminal default and drifted from its
/// neighbours.
pub(super) fn pane_border_style(focused: bool) -> Style {
    Style::default().fg(if focused {
        PANE_BORDER_FOCUSED
    } else {
        PANE_BORDER
    })
}

/// What the one-line status strip says, or `None` when there is nothing to
/// report — in which case the topics table takes the whole body.
///
/// The reachability half of what the channels bar used to say, kept without the
/// bar: an unreachable server and a not-yet-loaded poll are both reasons the
/// table below is empty or stale, and neither shows up anywhere else here.
fn status_line(app: &App) -> Option<Line<'static>> {
    if let Some(ref error) = app.error {
        return Some(Line::from(vec![
            Span::styled("Not connected: ", Style::default().fg(Color::Red)),
            Span::raw(error.clone()),
        ]));
    }
    if app.state.is_none() {
        return Some(Line::from("Connecting..."));
    }
    None
}

/// Draw the status strip: one line, no border, so it reads as a message rather
/// than as another pane.
fn render_status_line(frame: &mut Frame, area: Rect, line: Line<'static>) {
    frame.render_widget(Paragraph::new(line), area);
}

/// Draw the overview's topic table.
///
/// The table carries no border and no title: the info pane's left border is the
/// divider on its right and the activity pane's top border the one below, so a
/// frame of its own would only double them up. Its first row is the blank
/// padding the info pane's own content also starts below, and the rule under
/// the header is drawn in the same resting border colour as the pane borders.
fn render_topics(frame: &mut Frame, area: Rect, app: &mut App) {
    // Nothing loaded yet — the panes around it still draw.
    let Some(state) = &app.state else {
        return;
    };

    // `bottom_margin(1)` leaves one blank row under the header: the table
    // renders it and `render_topics` then draws the rule into it below.
    let header = Row::new(vec![
        Cell::from("Agent"),
        Cell::from("Topic"),
        Cell::from("Pattern"),
        Cell::from("Status"),
        Cell::from("Context"),
        Cell::from("Last Active"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD))
    .height(1)
    .bottom_margin(1);

    let rows: Vec<Row> = state
        .topics
        .iter()
        .map(|t| {
            let status_style = match t.status {
                TopicStatus::Processing => Style::default().fg(Color::Green),
                TopicStatus::Queued => Style::default().fg(Color::Yellow),
                TopicStatus::Idle => Style::default().fg(Color::DarkGray),
                TopicStatus::Error => Style::default().fg(Color::Red),
            };

            // The context size: the last call's input plus its output,
            // against the context window. The counts are formatted the
            // same way the info pane and `/info` format them.
            let tokens = match (t.context_input_tokens, t.max_tokens) {
                (Some(cur), Some(max)) => {
                    let used = jyc_types::context_tokens(Some(cur), t.context_output_tokens)
                        .unwrap_or(cur);
                    format!(
                        "{}/{}",
                        jyc_types::format_token_count(used),
                        jyc_types::format_token_count(max)
                    )
                }
                (Some(cur), None) => jyc_types::format_token_count(cur),
                _ => "-".to_string(),
            };

            Row::new(vec![
                Cell::from(t.agent.clone().unwrap_or_else(|| "-".into())),
                Cell::from(t.name.clone()),
                Cell::from(t.pattern.clone().unwrap_or("-".into())),
                Cell::from(Span::styled(format!("{}", t.status), status_style)),
                Cell::from(tokens),
                Cell::from(format_last_active(t.last_active_at.as_deref())),
            ])
        })
        .collect();

    // Agent and Topic share the space the Channel column gave up: names are
    // what the table is read for, and the two are the same value for an
    // agent-keyed topic. The rest keep their fixed widths.
    //
    // The Context column is wide enough for the longest figure the shared
    // formatter can print — `999.99K/999.99K`, fifteen cells — so a context
    // is never cut off mid-number. Status needs only its longest value
    // (`Processing`) and Last Active only its header, so the three cells
    // come from there and the table keeps the width it had.
    let widths = [
        Constraint::Min(20),
        Constraint::Length(16),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(15),
        Constraint::Length(11),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().add_modifier(Modifier::DIM))
        .highlight_symbol("→ ");

    frame.render_stateful_widget(table, area, &mut app.table_state);

    // Horizontal rule under the header, in the row `bottom_margin(1)` left
    // blank. The header is pinned at the top of the table area and is one
    // row tall, so the rule's row is always `area.y + 1`; the intersection
    // drops it entirely when the area is too short to hold it.
    let rule_area = Rect::new(area.x, area.y + 1, area.width, 1).intersection(area);
    if !rule_area.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "─".repeat(rule_area.width as usize),
                Style::default().fg(PANE_BORDER),
            )),
            rule_area,
        );
    }
}

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

/// Pack whole chips into rows of at most `width` cells.
///
/// Each chip carries the separator that joins it to the one before it (empty for
/// the first), so `2 active / 5 thread` keeps its slash while ` · ` separates the
/// rest — a chip that has to start a new row drops its separator rather than
/// leading a row with it.
///
/// A chip is never split: one wider than `width` gets a row to itself (and is
/// then clipped like any over-long line). Greedy — a short chip fills the current
/// row before the next one starts.
fn pack_chips(chips: &[(&str, String)], width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for (sep, chip) in chips {
        match rows.last_mut() {
            Some(row)
                if row.chars().count() + sep.chars().count() + chip.chars().count() <= width =>
            {
                row.push_str(sep);
                row.push_str(chip);
            }
            _ => rows.push(chip.clone()),
        }
    }
    rows
}

/// Build the chat-screen status block: the first lines of the topic info
/// pane, replacing the bottom status bar on the chat screen. The version on
/// its own line, then the server stats (gray) packed into as many rows as
/// `width` needs — a chip is never cut mid-word, only re-rowed, so a narrow
/// info column stays readable instead of showing `2 active / 5 t`. The
/// version line is the one row that is not packed: too narrow for it, it
/// clips rather than taking a second row. A transient status message swaps
/// the whole block for one yellow line; it auto-expires after a few seconds.
/// Empty when there is no server state to report (the chat header already
/// shows the connecting state).
fn chat_status_block(app: &App, width: u16) -> Vec<Line<'_>> {
    if let Some((msg, _)) = &app.status_message {
        return vec![Line::from(Span::styled(
            msg.as_str(),
            Style::default().fg(Color::Yellow),
        ))];
    }
    let Some(state) = &app.state else {
        return Vec::new();
    };
    let stats = &state.stats;
    let rows = pack_chips(
        &[
            ("", format!("{} active", stats.active_workers)),
            (" / ", format!("{} thread", stats.total_topics)),
            (" · ", format!("{} recv", stats.messages_received)),
            (" · ", format!("{} err", stats.errors)),
            (
                " · ",
                format!(
                    "up {}",
                    format_duration_secs(state.uptime_secs, DurationStyle::Coarse)
                ),
            ),
        ],
        width as usize,
    );
    let mut lines = vec![Line::from(format!("JYC AI v{}", state.version))];
    lines.extend(
        rows.into_iter()
            .map(|row| Line::from(Span::styled(row, Style::default().fg(Color::DarkGray)))),
    );
    lines
}

fn format_last_active(value: Option<&str>) -> String {
    let value = match value {
        Some(v) => v,
        None => return "-".to_string(),
    };
    let dt = match chrono::DateTime::parse_from_rfc3339(value) {
        Ok(dt) => dt.with_timezone(&chrono::Local),
        Err(_) => return "-".to_string(),
    };
    let now = chrono::Local::now();
    let mins = now.signed_duration_since(dt).num_minutes();
    if (0..=60).contains(&mins) {
        return format!("{mins}m ago");
    }
    let dt_local = dt.format("%H:%M").to_string();
    if dt.date_naive() == now.date_naive() {
        return dt_local;
    }
    dt.format("%b %d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The auto-started `jyc serve` keeps its provisioning shape and only
    /// forwards `--pipe` when the dashboard was asked for it.
    #[test]
    fn auto_spawned_serve_args_forward_pipe() {
        let default_data_dir = std::path::Path::new("/home/u/.local/share/jyc");
        let log_path = default_data_dir.join("jyc.log");
        assert_eq!(
            serve_spawn_args(default_data_dir, default_data_dir, &log_path, false),
            ["serve", "--log-file", "/home/u/.local/share/jyc/jyc.log"]
        );
        assert_eq!(
            serve_spawn_args(default_data_dir, default_data_dir, &log_path, true),
            [
                "serve",
                "--log-file",
                "/home/u/.local/share/jyc/jyc.log",
                "--pipe"
            ]
        );
        assert_eq!(
            serve_spawn_args(
                std::path::Path::new("/srv/jyc"),
                default_data_dir,
                &log_path,
                true
            ),
            [
                "serve",
                "--data-dir",
                "/srv/jyc",
                "--log-file",
                "/home/u/.local/share/jyc/jyc.log",
                "--pipe"
            ]
        );
    }

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
        check_existing_topic_name(&path, "any-topic", tmp.path())
            .expect("should pass when no file exists");
    }

    #[test]
    fn check_existing_topic_name_succeeds_when_matching() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("topic-name"), "abc").unwrap();

        let path = tmp.path().to_string_lossy().to_string();
        check_existing_topic_name(&path, "abc", tmp.path()).expect("should pass when names match");
    }

    #[test]
    fn check_existing_topic_name_fails_when_mismatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("topic-name"), "existing").unwrap();

        let path = tmp.path().to_string_lossy().to_string();
        let err = check_existing_topic_name(&path, "abc", tmp.path())
            .expect_err("should fail on mismatch");
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
        let state_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("topic-name"), "").unwrap();

        let path = tmp.path().to_string_lossy().to_string();
        check_existing_topic_name(&path, "new-topic", tmp.path())
            .expect("should pass when file is empty");
    }

    fn make_test_app() -> App {
        let (_, ws_rx) = tokio::sync::mpsc::unbounded_channel::<WsEvent>();
        App::new(ws_rx, None)
    }

    /// A `TopicInfo` as the per-topic info endpoint returns it — the detail
    /// the poll loop fetches for the topic on screen.
    ///
    /// The detail fetch and the renderers must agree on which topic is on
    /// screen — the screen rule lives in `shown_topic_key` for exactly that.
    #[test]
    fn shown_topic_key_follows_the_screen() {
        let mut app = make_test_app();
        let mut overview = make_overview_with_topics(&["a", "b"]);
        overview.topics[1].channel = "mail".to_string();
        app.state = Some(overview);

        // Dashboard: the row under the cursor.
        app.table_state.select(Some(1));
        assert_eq!(shown_topic_key(&app), Some(("mail".into(), "b".into())));

        // Chat screen: the visible chat's own topic, cursor elsewhere, and
        // resolved by name alone when the chat carries no channel.
        app.table_state.select(Some(0));
        app.chat.visible = true;
        app.chat.topic = Some("b".to_string());
        assert_eq!(shown_topic_key(&app), Some(("mail".into(), "b".into())));

        // A chat whose topic is not in the poll yet names nothing, rather than
        // falling back to the row under the cursor.
        app.chat.topic = Some("not-polled".to_string());
        assert_eq!(shown_topic_key(&app), None);
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
                    agent: None,
                    pattern: None,
                    status: TopicStatus::Idle,
                    context_input_tokens: None,
                    context_output_tokens: None,
                    max_tokens: None,
                    last_active_at: None,
                    topic_path: None,
                    parent: None,
                    commands: vec![],
                })
                .collect(),
            stats: Default::default(),
            models: vec![],
        }
    }

    /// Build an overview whose `topics[i].commands` matches `cmds[i]` (or
    /// `vec![]` if `i` is out of range). Used to seed per-topic commands in
    /// `refresh_chat_commands` tests.
    fn make_overview_with_topic_commands(
        names: &[&str],
        cmds: &[Vec<jyc_types::CommandInfo>],
    ) -> jyc_types::InspectOverview {
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
                .enumerate()
                .map(|(i, n)| TopicSummary {
                    name: (*n).to_string(),
                    channel: "chan".to_string(),
                    agent: None,
                    pattern: Some((*n).to_string()),
                    status: TopicStatus::Idle,
                    context_input_tokens: None,
                    context_output_tokens: None,
                    max_tokens: None,
                    last_active_at: None,
                    topic_path: None,
                    parent: None,
                    commands: cmds.get(i).cloned().unwrap_or_default(),
                })
                .collect(),
            stats: Default::default(),
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

    /// Regression: the topic table marks its selection with the same
    /// two-column `→` gutter + DIM as the chat popups (no reverse video),
    /// and the gutter is reserved on every row so the names don't shift.
    #[test]
    fn topics_table_selection_uses_arrow_gutter_without_reverse() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = make_test_app();
        app.state = Some(make_overview_with_topics(&["alpha", "beta"]));
        app.table_state.select(Some(0));

        let area = Rect::new(0, 0, 60, 6);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_topics(frame, area, &mut app))
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        let row = |y: u16| -> String {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect()
        };
        // Where a string starts, as a column: a row is one symbol per column
        // but `→` is three bytes in a single one, so a byte offset is not an
        // x — the two rows would read two apart with the columns aligned.
        let column_of = |s: &str, needle: &str| -> u16 {
            let at = s
                .find(needle)
                .unwrap_or_else(|| panic!("{needle:?} is not in {s:?}"));
            s[..at].chars().count() as u16
        };
        // Borderless: the table starts at the area's own x=0, the header on
        // y=0, the rule on y=1 and the first data row on y=2 — the `→ ` gutter
        // occupies the first two columns, then the Agent column, and only then
        // the names. Read the name's column off the row instead of hardcoding
        // it, so changing the columns does not silently move what this pins.
        assert_eq!(buffer[(0, 2)].symbol(), "→", "selected row needs the arrow");
        let first = row(2);
        let name_x = column_of(&first, "alpha");
        assert!(
            name_x > 2,
            "the name sits past the arrow gutter, at x={name_x}"
        );
        assert_eq!(
            buffer[(name_x, 2)].bg,
            Color::Reset,
            "selection paints no background"
        );
        assert!(buffer[(name_x, 2)].modifier.contains(Modifier::DIM));
        // The unselected row keeps the gutter width and stays undimmed, with
        // its name in the very column the selected row's is in.
        assert_eq!(
            buffer[(0, 3)].symbol(),
            " ",
            "gutter stays blank off-cursor"
        );
        let second = row(3);
        assert_eq!(column_of(&second, "beta"), name_x);
        assert!(!buffer[(name_x, 3)].modifier.contains(Modifier::DIM));
    }

    /// The table leads with the two identities a topic has — the agent and the
    /// topic's own name — and the channel column is gone: every topic on the
    /// hub's overview sits on one channel, so the column only repeated itself.
    #[test]
    fn topics_table_leads_with_agent_and_topic_and_drops_channel() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = make_test_app();
        let mut overview = make_overview_with_topics(&["issue-42"]);
        overview.topics[0].agent = Some("invoice".to_string());
        overview.topics[0].pattern = Some("planner".to_string());
        app.state = Some(overview);

        let area = Rect::new(0, 0, 100, 4);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_topics(frame, area, &mut app))
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        let row = |y: u16| -> String {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect()
        };
        // Nothing is selected here, so ratatui reserves no `→ ` gutter at all
        // and the header starts at x=0; the assertions trim rather than pin an
        // offset, which a selection would shift.
        let header = row(0);
        assert!(header.trim_start().starts_with("Agent"), "got: {header:?}");
        assert!(header.contains("Topic"), "got: {header:?}");
        assert!(header.contains("Pattern"), "got: {header:?}");
        assert!(!header.contains("Channel"), "got: {header:?}");

        // The rule spans the whole table width on the row below the header,
        // in the resting pane border colour rather than the header's bold.
        let rule = row(1);
        assert!(
            rule.chars().all(|c| c == '─'),
            "the header rule should run the table's full width, got: {rule:?}"
        );
        assert_eq!(buffer[(0, 1)].fg, PANE_BORDER, "got: {rule:?}");

        let first = row(2);
        assert!(first.contains("invoice"), "got: {first:?}");
        assert!(first.contains("issue-42"), "got: {first:?}");
        assert!(first.contains("planner"), "got: {first:?}");
    }

    /// The context column is wide enough for the whole figure: the shared
    /// formatter prints two decimals in K and M, so a context is never cut
    /// off mid-number.
    #[test]
    fn topics_table_shows_the_whole_context_figure() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = make_test_app();
        let mut overview = make_overview_with_topics(&["issue-42"]);
        overview.topics[0].context_input_tokens = Some(45_231);
        overview.topics[0].max_tokens = Some(120_000);
        app.state = Some(overview);

        let area = Rect::new(0, 0, 120, 4);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_topics(frame, area, &mut app))
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        let first: String = (0..area.width)
            .map(|x| buffer[(x, 2)].symbol().to_string())
            .collect();
        assert!(
            first.contains("44.17K/117.19K"),
            "the context figure should not be cut off, got: {first:?}"
        );
    }

    /// A topic whose agent could not be resolved shows a dash rather than an
    /// empty cell, the same way a missing pattern does.
    #[test]
    fn topics_table_shows_a_dash_for_an_unknown_agent() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = make_test_app();
        app.state = Some(make_overview_with_topics(&["alpha"]));

        let area = Rect::new(0, 0, 60, 4);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_topics(frame, area, &mut app))
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        // Sampled from x=0 and trimmed: with no selected row the gutter is not
        // reserved at all, and with one it sits in front — the dash is the
        // first thing in the row either way. The first data row is y=2, below
        // the header and its rule.
        let first: String = (0..area.width)
            .map(|x| buffer[(x, 2)].symbol().to_string())
            .collect();
        assert!(first.trim_start().starts_with('-'), "got: {first:?}");
    }

    /// With the channels bar gone, an overview that cannot reach its server is
    /// the one line the screen may still spend on saying so — and only then: a
    /// loaded overview leaves the table's top row blank, the way the info pane
    /// leaves its own.
    #[test]
    fn overview_says_it_is_unreachable_only_while_it_is() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let draw = |mut app: App| -> (String, String, String) {
            let backend = TestBackend::new(40, 8);
            let mut terminal = Terminal::new(backend).expect("terminal");
            terminal
                .draw(|frame| ui_normal_mode(frame, frame.area(), &mut app))
                .expect("draw");
            let buffer = terminal.backend().buffer().clone();
            let row = |y: u16| -> String {
                (0..40)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            };
            // The table takes the left 80% of the screen and the info pane's
            // left border sits at column 32, so only the table's own columns
            // can be blank — the full row is never empty.
            let table_row = |y: u16| -> String { row(y).chars().take(32).collect() };
            (row(0), table_row(0), row(1))
        };

        let mut loading = make_test_app();
        let (top, _, _) = draw(make_test_app());
        assert!(top.starts_with("Connecting..."), "got: {top:?}");

        // Loaded: no status line, so the table's own header takes the first
        // row the column has, below the blank one.
        loading.state = Some(make_overview_with_topics(&["alpha"]));
        let (full_blank, blank, header) = draw(loading);
        assert!(blank.trim().is_empty(), "got: {blank:?}");
        assert_eq!(full_blank.chars().nth(32), Some('│'), "got: {full_blank:?}");
        assert!(header.trim_start().starts_with("Agent"), "got: {header:?}");

        let mut broken = make_test_app();
        broken.error = Some("connection refused".to_string());
        let (top, _, _) = draw(broken);
        assert!(top.starts_with("Not connected:"), "got: {top:?}");
    }

    // --- refresh_chat_commands uses chat.topic, not table_state.selected() ---

    fn cmd(name: &str) -> jyc_types::CommandInfo {
        jyc_types::CommandInfo {
            name: name.to_string(),
            description: String::new(),
            ..Default::default()
        }
    }

    fn cmd_arg(name: &str, value: &str) -> jyc_types::CommandInfo {
        jyc_types::CommandInfo {
            name: name.to_string(),
            description: String::new(),
            args: vec![jyc_types::CommandArg {
                value: value.to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn overview_with_models(
        cmds: Vec<jyc_types::CommandInfo>,
        models: &[&str],
    ) -> jyc_types::InspectOverview {
        let mut overview = make_overview_with_topic_commands(&["jyc"], &[cmds]);
        overview.models = models
            .iter()
            .map(|m| jyc_types::ModelInfo {
                name: (*m).to_string(),
            })
            .collect();
        overview
    }

    /// The nested `/` popup levels ride in on the server's commands.
    #[tokio::test]
    async fn refresh_chat_commands_keeps_server_argument_values() {
        let mut app = make_test_app();
        app.chat.topic = Some("jyc".to_string());
        app.state = Some(overview_with_models(
            vec![cmd_arg("/model", "gpt-4")],
            &["deepseek/deepseek-chat"],
        ));

        app.refresh_chat_commands();

        assert_eq!(
            app.chat.commands[0].args[0].value, "gpt-4",
            "values the server sent must survive the model refill"
        );
    }

    /// Mixed-version compat: a server older than `CommandInfo::args` sends
    /// `/model` with no values at all; the picker still gets filled from the
    /// overview's own model list.
    #[tokio::test]
    async fn refresh_chat_commands_refills_model_args_for_old_servers() {
        let mut app = make_test_app();
        app.chat.topic = Some("jyc".to_string());
        app.state = Some(overview_with_models(
            vec![cmd("/model"), cmd("/plan")],
            &["deepseek/deepseek-chat"],
        ));

        app.refresh_chat_commands();

        let model = &app.chat.commands[0];
        assert_eq!(model.name, "/model");
        assert_eq!(model.args[0].value, "deepseek/deepseek-chat");
        assert!(
            app.chat.commands[1].args.is_empty(),
            "only /model gets the refill"
        );
    }

    #[tokio::test]
    async fn refresh_chat_commands_picks_chat_topic_over_table_selection() {
        let mut app = make_test_app();
        // Chat is bound to the "jyc" topic (e.g. opened via ad-hoc CLI),
        // but the topics list comes back with "dotfiles" first — exactly the
        // shape that previously mis-routed the `/` popup to dotfiles's
        // commands (the bug where `/deploy` was missing in jyc).
        let dotfiles_cmds = vec![cmd("/dotfiles-only")];
        let jyc_cmds = vec![cmd("/deploy")];
        let jin_cmds = vec![cmd("/jin-only")];
        app.chat.topic = Some("jyc".to_string());
        app.state = Some(make_overview_with_topic_commands(
            &["dotfiles", "jyc", "jin"],
            &[dotfiles_cmds.clone(), jyc_cmds.clone(), jin_cmds.clone()],
        ));
        // Auto-select row 0 as the poll loop does — leaves the table
        // highlighting dotfiles while chat is bound to jyc.
        app.table_state.select(Some(0));

        app.refresh_chat_commands();

        // Must contain `/deploy` (jyc's command) and NOT `/dotfiles-only`.
        assert!(
            app.chat.commands.iter().any(|c| c.name == "/deploy"),
            "chat.commands should include jyc's `/deploy`, got: {:?}",
            app.chat
                .commands
                .iter()
                .map(|c| &c.name)
                .collect::<Vec<_>>()
        );
        assert!(
            !app.chat.commands.iter().any(|c| c.name == "/dotfiles-only"),
            "chat.commands must not include `/dotfiles-only` (table row 0), got: {:?}",
            app.chat
                .commands
                .iter()
                .map(|c| &c.name)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn refresh_chat_commands_ignores_table_selection_change() {
        let mut app = make_test_app();
        let dotfiles_cmds = vec![cmd("/dotfiles-only")];
        let jyc_cmds = vec![cmd("/deploy")];
        app.chat.topic = Some("jyc".to_string());
        app.state = Some(make_overview_with_topic_commands(
            &["dotfiles", "jyc"],
            &[dotfiles_cmds.clone(), jyc_cmds.clone()],
        ));
        app.table_state.select(Some(0));
        app.refresh_chat_commands();
        let after_first = app.chat.commands.clone();

        // Navigate the table to row 1 (which now points at "jyc"). chat
        // is still bound to "jyc", so commands should be unchanged.
        app.table_state.select(Some(1));
        app.refresh_chat_commands();
        let after_second = app.chat.commands.clone();

        assert_eq!(
            after_first.iter().map(|c| &c.name).collect::<Vec<_>>(),
            after_second.iter().map(|c| &c.name).collect::<Vec<_>>(),
            "navigating the table must not change chat.commands when chat.topic is unchanged"
        );
    }

    #[tokio::test]
    async fn refresh_chat_commands_falls_back_to_builtins_when_topic_unknown() {
        let mut app = make_test_app();
        // Chat bound to a topic not in state.topics (ad-hoc transient
        // state). All topics have empty commands, so we fall back to
        // jyc_core::command::all_commands().
        app.chat.topic = Some("ghost".to_string());
        app.state = Some(make_overview_with_topics(&["alpha", "beta"]));
        app.refresh_chat_commands();
        let builtins = jyc_core::command::all_commands();
        assert_eq!(
            app.chat.commands.len(),
            builtins.len(),
            "fallback should yield the full builtin list when chat.topic is not in state.topics"
        );
    }

    #[tokio::test]
    async fn refresh_chat_commands_skips_topics_with_empty_command_list() {
        // Defends the `filter(|cmds| !cmds.is_empty())` clause: if the
        // server returned an empty list for chat.topic's row, we should
        // fall back to builtins rather than leaving the popup empty.
        let mut app = make_test_app();
        app.chat.topic = Some("jyc".to_string());
        app.state = Some(make_overview_with_topic_commands(
            &["dotfiles", "jyc"],
            &[vec![cmd("/dotfiles-only")], vec![]],
        ));
        app.refresh_chat_commands();
        let builtins = jyc_core::command::all_commands();
        assert_eq!(app.chat.commands.len(), builtins.len());
    }

    /// The overview renders the chat screen's panes, not lookalikes: the
    /// topic info pane on the right with the chat screen's left border and
    /// the status block pinned inside it, and the activity pane across the
    /// bottom with the chat screen's untitled top border. Driven through
    /// `ui_normal_mode` so the assertions pin the layout *and* the wiring.
    #[test]
    fn overview_renders_the_chat_panes() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = make_test_app();
        app.state = Some(make_overview_with_topics(&["alpha"]));
        app.table_state.select(Some(0));
        // The chat screen's visibility flags are all in their "closed" state
        // here: they are its own commands' state, and the overview must
        // render its three panes regardless of them.
        app.chat.activity_split = 0;
        app.chat.info_visible = false;
        app.chat.status_visible = false;

        let width: u16 = 100;
        let height: u16 = 24;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| ui_normal_mode(frame, frame.area(), &mut app))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();

        // Info pane: 20% of the width, so its left border sits at 80% of
        // the screen. With no channels bar above it, every row from the top
        // must carry the chat screen's vertical border there.
        let info_x = width * 80 / 100;
        let info_border: String = (0..12)
            .map(|y| buffer[(info_x, y)].symbol().to_string())
            .collect();
        assert!(
            info_border.chars().all(|c| c == '\u{2502}'),
            "the info pane should keep the chat screen's left border, got: {info_border:?}"
        );

        // The status sub-pane is the chat screen's own block (its version
        // line), rendered inside the info column on both screens. It is
        // pinned to the *bottom* of the column with its version line first,
        // so the whole column is sampled rather than one guessed row.
        let activity_top = height - height / 5 - 1;
        let info_text: String = (0..activity_top)
            .flat_map(|y| (info_x + 1..width).map(move |x| (x, y)))
            .map(|(x, y)| buffer[(x, y)].symbol().to_string())
            .collect();
        assert!(
            info_text.contains("JYC AI v"),
            "the info pane should carry the shared status block, got: {info_text:?}"
        );

        // Activity pane: the top border closes off the bottom strip, and
        // it carries no title - the chat screen's look, which PR #669
        // restored for both screens.
        let activity_row: String = (0..width)
            .map(|x| buffer[(x, activity_top)].symbol().to_string())
            .collect();
        assert!(
            activity_row.chars().all(|c| c == '\u{2500}'),
            "the activity pane should be closed off by its top border, got: {activity_row:?}"
        );
        assert!(
            !activity_row.contains("Activity"),
            "the activity pane should keep the untitled chat-screen look, got: {activity_row:?}"
        );
    }

    /// The overview's three panes are always on screen, so `Tab` cycles
    /// through all of them without consulting the chat screen's visibility
    /// flags — and the chat screen's own panes stay out of the cycle.
    #[test]
    fn overview_focus_cycles_all_three_panes_whatever_the_chat_flags_say() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(rx, None);
        // Every one of these would close a pane on the chat screen.
        app.chat.activity_split = 0;
        app.chat.info_visible = false;
        app.chat.status_visible = false;

        assert_eq!(app.chat.focus, Focus::TopicsTable);
        cycle_overview_focus(&mut app);
        assert_eq!(app.chat.focus, Focus::InfoPane);
        cycle_overview_focus(&mut app);
        assert_eq!(app.chat.focus, Focus::ActivityPane);
        cycle_overview_focus(&mut app);
        assert_eq!(app.chat.focus, Focus::TopicsTable);

        // A focus left on one of the chat screen's panes — leaving a chat
        // with the info pane focused, say — comes back to the table.
        app.chat.focus = Focus::ExplorerPane;
        cycle_overview_focus(&mut app);
        assert_eq!(app.chat.focus, Focus::TopicsTable);
    }

    /// A `last_active_at` ahead of the viewer's clock — hub/TUI clock skew, or
    /// an activity entry whose timestamp came from the sender's message — must
    /// not be reported as an age. It falls back to a clock time, like any other
    /// timestamp that is not recent.
    #[test]
    fn future_last_active_at_is_not_rendered_as_minutes_ago() {
        let future = (chrono::Local::now() + chrono::Duration::minutes(5)).to_rfc3339();
        let rendered = format_last_active(Some(&future));
        assert!(
            !rendered.contains("ago"),
            "future timestamp rendered as an age: {rendered:?}"
        );
    }
}
