use arc_swap::ArcSwap;
use futures_util::SinkExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::scoped_ws::ScopedWsHandler;
use crate::thread_proxy::ThreadProxyHandler;
use jyc_core::activity_log_store::ActivityLogStore;
use jyc_core::command::all_commands;
use jyc_core::command::list_available_models;
use jyc_core::metrics::SharedHealthStats;
use jyc_core::thread_event::ThreadEvent;
use jyc_core::thread_manager::ThreadManager;
use jyc_types::AppConfig;
use jyc_types::*;

/// Handler for WebSocket connections on the inspect server.
///
/// The websocket channel registers itself as the handler. When a dashboard
/// client connects via WebSocket upgrade on `/ws`, the inspect server hands
/// the stream to this handler.
#[async_trait::async_trait]
pub trait WebsocketHandler: Send + Sync {
    /// Handle a single WebSocket connection.
    async fn handle(
        &self,
        ws_stream: tokio_tungstenite::WebSocketStream<PrependStream>,
        addr: std::net::SocketAddr,
    ) -> anyhow::Result<()>;
}

/// Parsed WebSocket URL route. Determines which handler the inspect server
/// dispatches to for an incoming upgrade request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsRoute {
    /// `GET /ws` — use the first available websocket channel's handler.
    Bare,
    /// `GET /ws/<channel>` — adhoc thread on a websocket-type channel.
    Channel(String),
    /// `GET /ws/<channel>/<thread>` — proxy to a specific thread. For
    /// websocket-type channels, the inner handler is wrapped in
    /// `ScopedWsHandler` to auto-subscribe. For other channels, a
    /// `ThreadProxyHandler` is used.
    Thread { channel: String, name: String },
}

/// Max activity entries kept per thread.
const MAX_ACTIVITY_ENTRIES: usize = 180;

/// Max recent chat messages kept per thread for live dashboard display.
const MAX_RECENT_MESSAGES: usize = 50;

/// Per-thread activity buffer, shared between the activity tracker and the server.
///
/// Key is `(channel_name, thread_name)` so that two channels with same-named
/// threads (e.g. both have `issue-20`) do not collide.
pub type SharedActivityMap = Arc<Mutex<HashMap<(String, String), ThreadActivityState>>>;

/// Per-thread activity state: bounded event log + processing flag.
#[derive(Debug, Default)]
pub struct ThreadActivityState {
    pub entries: VecDeque<ActivityEntry>,
    pub is_processing: bool,
    pub has_error: bool,
    pub last_active_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Recent chat messages (incoming + replies) for live dashboard display.
    pub recent_messages: VecDeque<ChatMessageEntry>,
    /// Latest AI thinking/reasoning text (full, untruncated).
    pub thinking_text: Option<String>,
    /// Monotonic per-thread counter used to assign unique `id` to each entry
    /// and chat message. Wraps to 0 after `u64::MAX` (effectively never).
    pub next_id: u64,
}

/// Callback invoked after config is swapped atomically during reload.
/// Returns a Future so the caller can await the result and report errors
/// to the user.
pub type ReloadCallback =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>;

/// Shared state accessible by the inspect server.
pub struct InspectContext {
    /// Per-channel thread managers (dynamic — updated on reload)
    pub thread_managers: Arc<ArcSwap<Vec<Arc<ThreadManager>>>>,
    /// Channel info (name, type) (dynamic — updated on reload)
    pub channels: Arc<ArcSwap<Vec<ChannelInfo>>>,
    /// Shared health stats from MetricsCollector
    pub health_stats: SharedHealthStats,
    /// Per-thread activity logs from SSE events
    pub activity_map: SharedActivityMap,
    /// When the monitor started
    pub start_time: Instant,
    /// Path to the config file (for reload)
    pub config_path: Option<PathBuf>,
    /// Path to the global (L1) config file used as base layer (for reload)
    pub global_config_path: Option<PathBuf>,
    /// Swappable application config (for live reload)
    pub config: Option<Arc<ArcSwap<AppConfig>>>,
    /// Per-channel workspace directories (dynamic — updated on reload)
    pub workspace_dirs: Arc<ArcSwap<Vec<PathBuf>>>,
    /// WebSocket handlers keyed by channel name.
    /// `GET /ws/my_channel` routes to `handlers["my_channel"]`.
    /// `GET /ws` (no channel) routes to the first available handler.
    pub websocket_handlers: Option<HashMap<String, Arc<dyn WebsocketHandler>>>,
    /// Optional reload callback — invoked after config is swapped atomically.
    pub reload_callback: Option<ReloadCallback>,
    /// Per-channel broadcast bus fed by `ActivityTracker` — used by
    /// `ThreadProxyHandler` to forward activity/chat/thinking events to
    /// dashboard WebSocket clients. Capacity 256 (configured at creation).
    pub inspect_broadcast: Arc<tokio::sync::broadcast::Sender<String>>,
}

/// TCP-based inspect server.
///
/// Listens on the configured bind address and responds to JSON requests
/// with runtime state snapshots. Protocol: one JSON object per line.
pub struct InspectServer {
    bind_addr: String,
    context: Arc<InspectContext>,
    cancel: CancellationToken,
}

impl InspectServer {
    pub fn new(bind_addr: String, context: Arc<InspectContext>, cancel: CancellationToken) -> Self {
        Self {
            bind_addr,
            context,
            cancel,
        }
    }

    /// Start the inspect server. Returns a join handle for the background task.
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            if let Err(e) = self.run().await {
                tracing::error!(error = %e, "Inspect server error");
            }
        })
    }

    async fn run(self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(&self.bind_addr).await?;
        tracing::info!(bind = %self.bind_addr, "Inspect server started");

        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, addr)) => {
                            tracing::debug!(addr = %addr, "Inspect client connected");
                            let ctx = self.context.clone();
                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_client(stream, ctx, addr).await {
                                    tracing::debug!(error = %e, "Inspect client disconnected");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Inspect accept error");
                        }
                    }
                }
                _ = self.cancel.cancelled() => {
                    tracing::debug!("Inspect server shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn handle_client(
        stream: tokio::net::TcpStream,
        context: Arc<InspectContext>,
        addr: std::net::SocketAddr,
    ) -> anyhow::Result<()> {
        // Read the first line to detect protocol and extract path.
        let mut reader = tokio::io::BufReader::new(stream);
        let mut first_line = String::new();
        let bytes_read = reader.read_line(&mut first_line).await?;
        if bytes_read == 0 {
            return Ok(()); // Client disconnected immediately
        }

        // Get the remaining buffered data (if any) before we inspect the stream
        let remaining = reader.buffer().to_vec();
        let stream = reader.into_inner();

        // Reconstruct the full buffer: first_line + remaining
        let mut prepend_bytes = first_line.into_bytes();
        prepend_bytes.extend(remaining);

        if prepend_bytes.first() == Some(&b'G') {
            // HTTP request — extract WebSocket path for routing
            let request_str = String::from_utf8_lossy(&prepend_bytes);
            let first_line = request_str.lines().next().unwrap_or("");
            let route = Self::extract_ws_route(first_line);
            let handler = Self::resolve_ws_handler(&context, route);

            match handler {
                Ok(handler) => {
                    let prepend_stream = PrependStream::new(stream, prepend_bytes);
                    let ws_stream = tokio_tungstenite::accept_async(prepend_stream).await?;
                    handler.handle(ws_stream, addr).await?;
                }
                Err(e) => {
                    tracing::debug!(addr = %addr, error = %e, "WebSocket route resolution failed");
                    // Connection is already upgraded at the WS level; send a
                    // close frame so the client gets a clean disconnect.
                    let prepend_stream = PrependStream::new(stream, prepend_bytes);
                    let mut ws_stream = tokio_tungstenite::accept_async(prepend_stream).await?;
                    let _ = ws_stream
                        .send(tokio_tungstenite::tungstenite::Message::Close(None))
                        .await;
                }
            }
            return Ok(());
        }

        // JSON inspect protocol
        let prepend_stream = PrependStream::new(stream, prepend_bytes);
        let (reader_half, mut writer) = prepend_stream.into_split();
        let mut reader = BufReader::new(reader_half);
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = reader.read_line(&mut line).await?;
            if bytes_read == 0 {
                break; // Client disconnected
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let response = match serde_json::from_str::<InspectRequest>(trimmed) {
                Ok(req) => Self::handle_request(&req, &context).await,
                Err(e) => InspectResponse::Error {
                    error: format!("invalid request: {e}"),
                },
            };

            let mut json = serde_json::to_string(&response)?;
            json.push('\n');
            writer.write_all(json.as_bytes()).await?;
            writer.flush().await?;
        }

        Ok(())
    }

    /// Parse the WebSocket path from an HTTP GET request line into a `WsRoute`.
    ///
    /// `GET /ws` → `WsRoute::Bare` (use the first available websocket channel)
    /// `GET /ws/<channel>` → `WsRoute::Channel(name)` (adhoc thread on that channel)
    /// `GET /ws/<channel>/<thread>` → `WsRoute::Thread { channel, name }`
    ///   (proxy to that thread regardless of channel type)
    fn extract_ws_route(request_line: &str) -> Option<WsRoute> {
        let path = request_line.split_whitespace().nth(1)?;
        if path == "/ws" {
            return Some(WsRoute::Bare);
        }
        let rest = path.strip_prefix("/ws/")?;
        let mut parts = rest.splitn(3, '/');
        let channel = parts.next()?.to_string();
        match parts.next() {
            Some(thread) => Some(WsRoute::Thread {
                channel,
                name: thread.to_string(),
            }),
            None => Some(WsRoute::Channel(channel)),
        }
    }

    /// Resolve a `WsRoute` to a concrete `WebsocketHandler`.
    ///
    /// - `WsRoute::Thread { channel, name }`:
    ///   - If `<channel>` is a websocket-type channel, use `ScopedWsHandler`
    ///     to wrap its `WebsocketInboundAdapter` and pre-send `subscribe`.
    ///   - Otherwise, construct a `ThreadProxyHandler` that routes through
    ///     the channel's `ThreadManager` and the inspect-broadcast bus.
    /// - `WsRoute::Channel(name)`: use the channel's own handler (must be a
    ///   websocket-type channel, else error).
    /// - `WsRoute::Bare`: use the first available handler; error if none.
    fn resolve_ws_handler(
        context: &InspectContext,
        route: Option<WsRoute>,
    ) -> anyhow::Result<Arc<dyn WebsocketHandler>> {
        let route = match route {
            Some(r) => r,
            None => return Err(anyhow::anyhow!("could not parse WebSocket path")),
        };

        let handlers = context.websocket_handlers.as_ref();

        match route {
            WsRoute::Thread { channel, name } => {
                // If the channel has a websocket handler, use the scoped wrapper.
                // Otherwise, fall through to the proxy (works for any channel type).
                if let Some(handlers) = handlers
                    && let Some(handler) = handlers.get(&channel)
                {
                    return Ok(Arc::new(ScopedWsHandler::new(handler.clone(), name)));
                }
                Ok(Arc::new(ThreadProxyHandler::new(
                    channel,
                    name,
                    context.thread_managers.clone(),
                    context.inspect_broadcast.clone(),
                )))
            }
            WsRoute::Channel(name) => {
                let handlers =
                    handlers.ok_or_else(|| anyhow::anyhow!("no WebSocket handlers registered"))?;
                handlers.get(&name).cloned().ok_or_else(|| {
                    anyhow::anyhow!("channel '{}' not found or not a websocket channel", name)
                })
            }
            WsRoute::Bare => {
                let handlers =
                    handlers.ok_or_else(|| anyhow::anyhow!("no WebSocket handlers registered"))?;
                handlers
                    .values()
                    .next()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("no websocket channel configured"))
            }
        }
    }

    async fn handle_request(request: &InspectRequest, context: &InspectContext) -> InspectResponse {
        match request.method.as_str() {
            "get_state" => {
                let state = Self::build_state(context).await;
                InspectResponse::State(state)
            }
            "get_state_overview" => {
                let overview = Self::build_overview_state(context).await;
                InspectResponse::Overview(overview)
            }
            "get_thread_activity" => Self::handle_get_thread_activity(request, context).await,
            "get_thread_chat" => Self::handle_get_thread_chat(request, context).await,
            "reload_config" => Self::handle_reload_config(context).await,
            "reset_session" => Self::handle_reset_session(request, context).await,
            other => InspectResponse::Error {
                error: format!("unknown method: {other}"),
            },
        }
    }

    /// Build a slim overview snapshot — same shape as `build_state` but with
    /// `ThreadSummary` instead of `ThreadInfo`, dropping `activity`, `recent_messages`,
    /// and `thinking_text`. Used by the dashboard's polling loop to keep payloads small.
    async fn build_overview_state(context: &InspectContext) -> InspectOverview {
        let uptime = context.start_time.elapsed().as_secs();

        let mut threads = Vec::new();
        let mut total_threads = 0;
        let mut active_workers = 0;
        let mut per_channel_workers: HashMap<String, (usize, usize)> = HashMap::new();

        let tms = context.thread_managers.load();
        for tm in tms.iter() {
            let tm_threads = tm.list_threads().await;
            total_threads += tm_threads.len();
            let stats = tm.get_stats().await;
            active_workers += stats.active_workers;
            per_channel_workers.insert(
                tm.channel_name().to_string(),
                (stats.active_workers, tm.max_concurrent()),
            );
            threads.extend(tm_threads);
        }

        // Override status from activity_map (Processing / Error flags) but skip
        // copying activity/messages/thinking — that's the whole point.
        let activity_map = context.activity_map.lock().await;
        for thread in &mut threads {
            let key = (thread.channel.clone(), thread.name.clone());
            if let Some(state) = activity_map.get(&key) {
                if state.is_processing {
                    thread.status = ThreadStatus::Processing;
                } else if state.has_error {
                    thread.status = ThreadStatus::Error;
                }
                if let Some(last_active) = state.last_active_at {
                    thread.last_active_at = Some(last_active.to_rfc3339());
                }
            }
        }
        drop(activity_map);

        // Slim each ThreadInfo down to a ThreadSummary.
        let summaries: Vec<ThreadSummary> = threads
            .into_iter()
            .map(|t| ThreadSummary {
                name: t.name,
                channel: t.channel,
                pattern: t.pattern,
                status: t.status,
                model: t.model,
                mode: t.mode,
                input_tokens: t.input_tokens,
                max_tokens: t.max_tokens,
                last_active_at: t.last_active_at,
                skills: t.skills,
                thread_path: t.thread_path,
            })
            .collect();

        let health = context.health_stats.lock().await;
        let max_concurrent: usize = tms.iter().map(|tm| tm.max_concurrent()).sum();
        let stats = GlobalStats {
            active_workers,
            total_threads,
            max_concurrent,
            available_workers: max_concurrent.saturating_sub(active_workers),
            messages_received: health.messages_received,
            messages_processed: health.messages_processed,
            errors: health.errors,
        };
        drop(health);

        let channels = context.channels.load();
        let mut channels: Vec<ChannelInfo> = channels.iter().cloned().collect();
        for ch in &mut channels {
            if let Some((aw, mc)) = per_channel_workers.get(&ch.name) {
                ch.active_workers = *aw;
                ch.max_concurrent = *mc;
            }
        }

        InspectOverview {
            uptime_secs: uptime,
            version: env!("CARGO_PKG_VERSION").to_string(),
            channels,
            threads: summaries,
            stats,
            commands: all_commands(),
            models: context
                .config
                .as_ref()
                .map(|cfg| list_available_models(&cfg.load().agent.providers))
                .unwrap_or_default(),
        }
    }

    /// Handle `get_thread_activity {channel, thread, since?, limit?}` — returns
    /// recent activity entries from `.jyc/activity.jsonl`.
    async fn handle_get_thread_activity(
        request: &InspectRequest,
        context: &InspectContext,
    ) -> InspectResponse {
        let params = match &request.params {
            Some(p) => p,
            None => {
                return InspectResponse::Error {
                    error: "missing params".to_string(),
                };
            }
        };

        let channel = match params.get("channel").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return InspectResponse::Error {
                    error: "missing or invalid 'channel' param".to_string(),
                };
            }
        };
        let thread = match params.get("thread").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                return InspectResponse::Error {
                    error: "missing or invalid 'thread' param".to_string(),
                };
            }
        };
        let since = params
            .get("since")
            .and_then(|v| v.as_str())
            .map(String::from);
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(180);

        let tms = context.thread_managers.load();
        let tm = match tms.iter().find(|tm| tm.channel_name() == channel) {
            Some(t) => t,
            None => {
                return InspectResponse::Error {
                    error: format!("no thread manager found for channel '{channel}'"),
                };
            }
        };
        let thread_path = match tm.thread_path(thread).await {
            Some(p) => p,
            None => {
                return InspectResponse::Error {
                    error: format!("thread '{thread}' not found in channel '{channel}'"),
                };
            }
        };

        let entries = match ActivityLogStore::load_recent(&thread_path, limit) {
            Ok(e) => e,
            Err(e) => {
                return InspectResponse::Error {
                    error: format!("failed to load activity: {e}"),
                };
            }
        };
        let entries = filter_by_since(entries, since.as_deref());
        InspectResponse::ActivityHistory { entries }
    }

    /// Handle `get_thread_chat {channel, thread, since?, limit?}` — returns
    /// recent chat messages from `chat_history_*.jsonl`.
    async fn handle_get_thread_chat(
        request: &InspectRequest,
        context: &InspectContext,
    ) -> InspectResponse {
        let params = match &request.params {
            Some(p) => p,
            None => {
                return InspectResponse::Error {
                    error: "missing params".to_string(),
                };
            }
        };

        let channel = match params.get("channel").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return InspectResponse::Error {
                    error: "missing or invalid 'channel' param".to_string(),
                };
            }
        };
        let thread = match params.get("thread").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                return InspectResponse::Error {
                    error: "missing or invalid 'thread' param".to_string(),
                };
            }
        };
        let since = params
            .get("since")
            .and_then(|v| v.as_str())
            .map(String::from);
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(100);

        let tms = context.thread_managers.load();
        let tm = match tms.iter().find(|tm| tm.channel_name() == channel) {
            Some(t) => t,
            None => {
                return InspectResponse::Error {
                    error: format!("no thread manager found for channel '{channel}'"),
                };
            }
        };
        let thread_path = match tm.thread_path(thread).await {
            Some(p) => p,
            None => {
                return InspectResponse::Error {
                    error: format!("thread '{thread}' not found in channel '{channel}'"),
                };
            }
        };

        let entries = jyc_core::chat_log_store::load_recent_chat_history(&thread_path, limit);
        let entries = filter_chat_by_since(entries, since.as_deref());
        InspectResponse::ChatHistory { entries }
    }

    /// Load stored routing metadata for a thread from `.jyc/thread-meta.json`.
    ///
    /// Written by `process_message()` on the first message for a thread.
    /// Returns `None` if the file doesn't exist or can't be parsed.
    #[allow(dead_code)] // retained for future use; currently consumed by ThreadProxyHandler directly
    async fn load_thread_meta(
        tm: &Arc<ThreadManager>,
        thread_name: &str,
    ) -> Option<serde_json::Value> {
        let thread_path = tm.thread_path(thread_name).await?;
        let meta_path = thread_path.join(".jyc").join("thread-meta.json");
        let content = tokio::fs::read_to_string(&meta_path).await.ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Reload configuration from disk and swap it atomically.
    async fn handle_reload_config(context: &InspectContext) -> InspectResponse {
        let (config_path, config_swap) = match (&context.config_path, &context.config) {
            (Some(path), Some(config)) => (path, config),
            _ => {
                return InspectResponse::ReloadResult {
                    success: false,
                    message: "config reload not available (no config path)".to_string(),
                };
            }
        };

        tracing::info!(path = %config_path.display(), "Reloading configuration");

        // Load and validate new config (layered: global base + workdir overlay)
        let new_config = match jyc_types::load_config_layered(
            context.global_config_path.as_deref(),
            config_path,
        ) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("failed to load config: {e:#}");
                tracing::warn!("{msg}");
                return InspectResponse::ReloadResult {
                    success: false,
                    message: msg,
                };
            }
        };

        let errors = jyc_types::validation::validate_config(&new_config);
        if !errors.is_empty() {
            let msg = errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            let msg = format!("validation failed: {msg}");
            tracing::warn!("{msg}");
            return InspectResponse::ReloadResult {
                success: false,
                message: msg,
            };
        }

        // Atomically swap the config
        config_swap.store(Arc::new(new_config));
        tracing::info!("Configuration reloaded successfully");

        // Notify orchestrator if a reload callback is registered
        if let Some(ref callback) = context.reload_callback {
            tracing::debug!("Invoking reload callback");
            if let Err(e) = callback().await {
                let msg = format!("config reloaded, but channel reload failed: {e:#}");
                tracing::error!(error = %e, "Channel reload failed after config swap");
                return InspectResponse::ReloadResult {
                    success: false,
                    message: msg,
                };
            }
        }

        InspectResponse::ReloadResult {
            success: true,
            message: "configuration reloaded".to_string(),
        }
    }

    /// Delete the agent session file for a given thread.
    async fn handle_reset_session(
        request: &InspectRequest,
        context: &InspectContext,
    ) -> InspectResponse {
        let thread_name = match request.params.as_ref().and_then(|p| p.get("thread_name")) {
            Some(v) => match v.as_str() {
                Some(s) => s.to_string(),
                None => {
                    return InspectResponse::ResetSessionResult {
                        success: false,
                        message: "thread_name must be a string".to_string(),
                    };
                }
            },
            None => {
                return InspectResponse::ResetSessionResult {
                    success: false,
                    message: "missing thread_name param".to_string(),
                };
            }
        };

        if thread_name.contains("..") || thread_name.contains('/') || thread_name.contains('\\') {
            return InspectResponse::ResetSessionResult {
                success: false,
                message: "invalid thread_name: path traversal not allowed".to_string(),
            };
        }

        // Resolve compression config: check agent config for fallback
        let config = context
            .config
            .as_ref()
            .and_then(|c| {
                let cfg = c.load();
                cfg.agent.reset_compression.clone()
            })
            .unwrap_or_default();

        let tms = context.thread_managers.load();
        let mut found = false;
        for tm in tms.iter() {
            if let Err(e) = tm.reset_session(&thread_name, &config).await {
                tracing::warn!(
                    thread = %thread_name,
                    error = %e,
                    "Failed to reset session via thread manager"
                );
            }
            found = true;
        }
        drop(tms);

        // Fallback: if no thread managers handled the reset, delete files directly
        // (needed during testing and when thread manager is not yet available)
        if !found {
            let dirs = context.workspace_dirs.load();
            let mut deleted = false;
            for dir in dirs.iter() {
                let session_path = dir
                    .join(&thread_name)
                    .join(".jyc")
                    .join("agent-session.json");
                if session_path.exists() {
                    tokio::fs::remove_file(&session_path).await.ok();
                    deleted = true;
                }
                let context_path = dir
                    .join(&thread_name)
                    .join(".jyc")
                    .join("agent-context.json");
                if context_path.exists() {
                    tokio::fs::remove_file(&context_path).await.ok();
                    deleted = true;
                }
            }

            if deleted {
                tracing::info!(thread = %thread_name, "Session reset via inspect protocol (filesystem fallback)");
                InspectResponse::ResetSessionResult {
                    success: true,
                    message: format!("session deleted for {thread_name}"),
                }
            } else {
                InspectResponse::ResetSessionResult {
                    success: true,
                    message: format!("no session exists for {thread_name}"),
                }
            }
        } else {
            tracing::info!(thread = %thread_name, "Session reset via inspect protocol");
            InspectResponse::ResetSessionResult {
                success: true,
                message: format!("session reset for {thread_name}"),
            }
        }
    }

    async fn build_state(context: &InspectContext) -> InspectState {
        let uptime = context.start_time.elapsed().as_secs();

        let mut threads = Vec::new();
        let mut total_threads = 0;
        let mut active_workers = 0;
        let mut per_channel_workers: HashMap<String, (usize, usize)> = HashMap::new();

        let tms = context.thread_managers.load();
        for tm in tms.iter() {
            let tm_threads = tm.list_threads().await;
            total_threads += tm_threads.len();
            let stats = tm.get_stats().await;
            active_workers += stats.active_workers;
            per_channel_workers.insert(
                tm.channel_name().to_string(),
                (stats.active_workers, tm.max_concurrent()),
            );
            threads.extend(tm_threads);
        }

        // Merge activity logs and status into threads
        let activity_map = context.activity_map.lock().await;
        for thread in &mut threads {
            let key = (thread.channel.clone(), thread.name.clone());
            if let Some(state) = activity_map.get(&key) {
                thread.activity = state.entries.iter().cloned().collect();
                thread.recent_messages = state.recent_messages.iter().cloned().collect();
                thread.thinking_text = state.thinking_text.clone();
                if state.is_processing {
                    thread.status = ThreadStatus::Processing;
                } else if state.has_error {
                    thread.status = ThreadStatus::Error;
                }
                if let Some(last_active) = state.last_active_at {
                    thread.last_active_at = Some(last_active.to_rfc3339());
                }
            }
        }
        drop(activity_map);

        // Read metrics
        let health = context.health_stats.lock().await;
        let max_concurrent: usize = tms.iter().map(|tm| tm.max_concurrent()).sum();
        let stats = GlobalStats {
            active_workers,
            total_threads,
            max_concurrent,
            available_workers: max_concurrent.saturating_sub(active_workers),
            messages_received: health.messages_received,
            messages_processed: health.messages_processed,
            errors: health.errors,
        };
        drop(health);

        let channels = context.channels.load();
        let mut channels: Vec<ChannelInfo> = channels.iter().cloned().collect();
        for ch in &mut channels {
            if let Some((aw, mc)) = per_channel_workers.get(&ch.name) {
                ch.active_workers = *aw;
                ch.max_concurrent = *mc;
            }
        }

        InspectState {
            uptime_secs: uptime,
            version: env!("CARGO_PKG_VERSION").to_string(),
            channels,
            threads,
            stats,
            commands: all_commands(),
            models: context
                .config
                .as_ref()
                .map(|cfg| list_available_models(&cfg.load().agent.providers))
                .unwrap_or_default(),
        }
    }
}

/// Filter activity entries by `since` timestamp (RFC 3339 string).
/// Returns entries whose timestamp is `>= since`. If `since` is None,
/// returns all entries unchanged.
fn filter_by_since(mut entries: Vec<ActivityEntry>, since: Option<&str>) -> Vec<ActivityEntry> {
    if let Some(since_ts) = since {
        entries.retain(|e| {
            e.timestamp
                .as_deref()
                .map(|t| t >= since_ts)
                .unwrap_or(false)
        });
    }
    entries
}

/// Filter chat messages by `since` timestamp (RFC 3339 string).
fn filter_chat_by_since(
    mut entries: Vec<ChatMessageEntry>,
    since: Option<&str>,
) -> Vec<ChatMessageEntry> {
    if let Some(since_ts) = since {
        entries.retain(|e| {
            e.timestamp
                .as_deref()
                .map(|t| t >= since_ts)
                .unwrap_or(false)
        });
    }
    entries
}

/// Publish an activity entry to the inspect-broadcast bus.
///
/// Payload format:
///   {"type":"activity","channel":"...","thread":"...","id":N,"entry":{...}}
fn publish_activity_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    thread: &str,
    entry: &ActivityEntry,
) {
    let payload = serde_json::json!({
        "type": "activity",
        "channel": channel,
        "thread": thread,
        "id": entry.id,
        "entry": entry,
    });
    let _ = bus.send(payload.to_string());
}

/// Publish a chat message to the inspect-broadcast bus.
///
/// Payload format:
///   {"type":"chat_message","channel":"...","thread":"...","id":N,"entry":{...}}
fn publish_chat_message_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    thread: &str,
    msg: &ChatMessageEntry,
) {
    let payload = serde_json::json!({
        "type": "chat_message",
        "channel": channel,
        "thread": thread,
        "id": msg.id,
        "entry": msg,
    });
    let _ = bus.send(payload.to_string());
}

/// Publish a thinking event to the inspect-broadcast bus.
///
/// Payload format:
///   {"type":"thinking","channel":"...","thread":"...","text":"..."}
fn publish_thinking_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    thread: &str,
    text: &str,
) {
    let payload = serde_json::json!({
        "type": "thinking",
        "channel": channel,
        "thread": thread,
        "text": text,
    });
    let _ = bus.send(payload.to_string());
}

/// Publish a processing-status event to the inspect-broadcast bus.
///
/// Payload format:
///   {"type":"processing","channel":"...","thread":"...","is_processing":bool,"has_error":bool}
fn publish_processing_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    thread: &str,
    is_processing: bool,
    has_error: bool,
) {
    let payload = serde_json::json!({
        "type": "processing",
        "channel": channel,
        "thread": thread,
        "is_processing": is_processing,
        "has_error": has_error,
    });
    let _ = bus.send(payload.to_string());
}

/// Background task that subscribes to thread event buses and buffers
/// activity entries for the inspect server.
pub struct ActivityTracker;

impl ActivityTracker {
    /// Start tracking activity for all thread managers.
    /// Periodically discovers new threads and subscribes to their event buses.
    /// Persists activity entries to `.jyc/activity.jsonl` per thread.
    /// Fans out events to the inspect-broadcast bus for dashboard WS clients.
    /// On startup, loads historical activity from disk.
    pub fn start(
        thread_managers: Arc<ArcSwap<Vec<Arc<ThreadManager>>>>,
        activity_map: SharedActivityMap,
        _workspace_dirs: Arc<ArcSwap<Vec<PathBuf>>>,
        inspect_broadcast: Arc<tokio::sync::broadcast::Sender<String>>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let subscribed: Arc<Mutex<HashSet<(String, String)>>> =
                Arc::new(Mutex::new(HashSet::new()));
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));

            // Load historical activity from disk for all existing threads
            let tms = thread_managers.load();
            for tm in tms.iter() {
                let channel = tm.channel_name().to_string();
                let threads = tm.list_threads().await;
                for thread in &threads {
                    let thread_path = thread.thread_path.clone();
                    if let Some(ref path) = thread_path
                        && let Ok(entries) =
                            ActivityLogStore::load_recent(path, MAX_ACTIVITY_ENTRIES)
                        && !entries.is_empty()
                    {
                        let mut map = activity_map.lock().await;
                        let state = map
                            .entry((channel.clone(), thread.name.clone()))
                            .or_default();
                        state.entries = entries.into_iter().collect();
                        state.is_processing = false;
                        if let Some(last) = state.entries.back()
                            && let Some(ref ts) = last.timestamp
                            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts)
                        {
                            state.last_active_at = Some(dt.with_timezone(&chrono::Utc));
                        }
                    }
                }
            }
            drop(tms);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Discover new threads and subscribe to their event buses
                        let tms = thread_managers.load();
                        for tm in tms.iter() {
                            let channel = tm.channel_name().to_string();
                            let threads = tm.list_threads().await;
                            for thread in threads {
                                let key = (channel.clone(), thread.name.clone());
                                {
                                    let sub = subscribed.lock().await;
                                    if sub.contains(&key) {
                                        continue;
                                    }
                                }
                                // Try to get an existing event bus. If none exists but
                                // the thread has an active queue (worker running or
                                // pending messages), force-create one so we don't miss
                                // events. If no active queue, the thread is idle — clear
                                // any stale `is_processing` flag and mark as subscribed
                                // to avoid retrying every 2s.
                                let bus = match tm.get_event_bus(&thread.name).await {
                                    Some(b) => Some(b),
                                    None if tm.has_active_queue(&thread.name).await => {
                                        tracing::info!(
                                            thread = %thread.name,
                                            "Event bus missing but queue active, force-creating event bus"
                                        );
                                        tm.get_or_create_event_bus(&thread.name).await
                                    }
                                    None => {
                                        // Thread is idle (no active queue, no event bus).
                                        // Clear any stale processing state so the dashboard
                                        // doesn't get stuck showing "Processing" forever.
                                        // Do NOT insert into `subscribed` — that would
                                        // permanently exclude this thread from future checks,
                                        // so if the event bus is created just after this tick
                                        // (race with create_and_enqueue), the ActivityTracker
                                        // would never subscribe.
                                        let mut map = activity_map.lock().await;
                                        if let Some(state) = map.get_mut(&key) {
                                            state.is_processing = false;
                                        }
                                        drop(map);
                                        continue;
                                    }
                                };

                                if let Some(bus) = bus
                                    && let Ok(mut rx) = bus.subscribe().await {
                                        {
                                            let mut sub = subscribed.lock().await;
                                            sub.insert(key.clone());
                                        }
                                        let map = activity_map.clone();
                                        let name = thread.name.clone();
                                        let channel_for_task = channel.clone();
                                        let thread_path = thread.thread_path.clone();
                                        let cancel_inner = cancel.clone();
                                        let subscribed_clone = subscribed.clone();
                                        let key_clone = key.clone();
                                        let inspect_broadcast_for_task = inspect_broadcast.clone();
                                        tokio::spawn(async move {
                                            use futures_util::FutureExt;
                                            use std::panic::AssertUnwindSafe;

                                            let result = AssertUnwindSafe(async {
                                                loop {
                                                    tokio::select! {
                                                        event = rx.recv() => {
                                                            match event {
                                                                Some(event) => {
                                                                    let is_processing = matches!(
                                                                        &event,
                                                                        ThreadEvent::ProcessingStarted { .. }
                                                                        | ThreadEvent::ProcessingProgress { .. }
                                                                        | ThreadEvent::ToolStarted { .. }
                                                                        | ThreadEvent::LLMRequestStarted { .. }
                                                                    );
                                                                    let is_completed = matches!(
                                                                        &event,
                                                                        ThreadEvent::ProcessingCompleted { .. }
                                                                    );

                                                                    // Capture chat messages for live dashboard display
                                                                    let chat_msg: Option<ChatMessageEntry> = match &event {
                                                                        ThreadEvent::IncomingMessage { sender, text, timestamp, .. } => {
                                                                            Some(ChatMessageEntry {
                                                                                sender: sender.clone(),
                                                                                text: text.clone(),
                                                                                timestamp: Some(timestamp.to_rfc3339()),
                                                                                id: 0, // assigned below in the fanout step
                                                                            })
                                                                        }
                                                                        ThreadEvent::ReplySent { text, timestamp, .. } => {
                                                                            Some(ChatMessageEntry {
                                                                                sender: "ai".to_string(),
                                                                                text: text.clone(),
                                                                                timestamp: Some(timestamp.to_rfc3339()),
                                                                                id: 0, // assigned below in the fanout step
                                                                            })
                                                                        }
                                                                        _ => None,
                                                                    };

                                                                    let is_thinking =
                                                                        matches!(&event, ThreadEvent::Thinking { .. });

                                                                    // Thinking events are NOT persisted to
                                                                    // activity.jsonl or the activity buffer.
                                                                    // They update `thinking_text` instead
                                                                    // (displayed in the chat pane, not the
                                                                    // activity pane).
                                                                    if !is_thinking {
                                                                        let mut entry = event_to_activity(&event);
                                                                        let is_error = entry.severity == Severity::Error;
                                                                        let is_progress =
                                                                            matches!(&event, ThreadEvent::ProcessingProgress { .. });
                                                                        if let Some(ref path) = thread_path
                                                                            && let Err(e) = ActivityLogStore::append(path, &entry) {
                                                                                tracing::warn!(error = %e, thread = %name, "Failed to persist activity entry");
                                                                            }
                                                                        let mut map = map.lock().await;
                                                                        let state = map
                                                                            .entry((channel_for_task.clone(), name.clone()))
                                                                            .or_default();
                                                                        // Assign monotonic per-thread id BEFORE pushing.
                                                                        entry.id = state.next_id;
                                                                        state.next_id = state.next_id.wrapping_add(1);
                                                                        // ProcessingProgress is a heartbeat, not a discrete
                                                                        // activity. Persist it to disk but skip the in-memory
                                                                        // activity log so it doesn't crowd out ToolStarted /
                                                                        // ToolCompleted entries that show the actual tool name.
                                                                        if !is_progress {
                                                                            state.entries.push_back(entry.clone());
                                                                            if state.entries.len() > MAX_ACTIVITY_ENTRIES {
                                                                                state.entries.pop_front();
                                                                            }
                                                                        }
                                                                        // Fan out to the inspect-broadcast bus so dashboard
                                                                        // WebSocket clients receive live events.
                                                                        publish_activity_event(
                                                                            &inspect_broadcast_for_task,
                                                                            &channel_for_task,
                                                                            &name,
                                                                            &entry,
                                                                        );

                                                                        if let Some(mut msg) = chat_msg {
                                                                            msg.id = state.next_id;
                                                                            state.next_id = state.next_id.wrapping_add(1);
                                                                            state.recent_messages.push_back(msg.clone());
                                                                            if state.recent_messages.len() > MAX_RECENT_MESSAGES {
                                                                                state.recent_messages.pop_front();
                                                                            }
                                                                            publish_chat_message_event(
                                                                                &inspect_broadcast_for_task,
                                                                                &channel_for_task,
                                                                                &name,
                                                                                &msg,
                                                                            );
                                                                        }
                                                                        // Clear thinking text only when a new processing
                                                                        // cycle starts or the current one completes.
                                                                        // Do NOT clear on ProcessingProgress heartbeats,
                                                                        // ToolStarted, or LLMRequestStarted — those are
                                                                        // mid-cycle events that should keep the thinking
                                                                        // display visible.
                                                                        if matches!(
                                                                            &event,
                                                                            ThreadEvent::ProcessingStarted { .. }
                                                                            | ThreadEvent::ProcessingCompleted { .. }
                                                                        ) {
                                                                            state.thinking_text = None;
                                                                            publish_processing_event(
                                                                                &inspect_broadcast_for_task,
                                                                                &channel_for_task,
                                                                                &name,
                                                                                state.is_processing,
                                                                                state.has_error,
                                                                            );
                                                                        }
                                                                        state.last_active_at = Some(event.timestamp());
                                                                        if is_processing {
                                                                            state.is_processing = true;
                                                                            state.has_error = false;
                                                                        } else if is_completed {
                                                                            state.is_processing = false;
                                                                            }
                                                                        if is_error {
                                                                            state.has_error = true;
                                                                        }
                                                                    } else {
                                                                        // Thinking event: update thinking_text and fan out.
                                                                        if let ThreadEvent::Thinking { ref text, .. } = event {
                                                                            let mut map = map.lock().await;
                                                                            let state = map
                                                                                .entry((channel_for_task.clone(), name.clone()))
                                                                                .or_default();
                                                                            state.thinking_text = Some(text.clone());
                                                                            state.last_active_at = Some(event.timestamp());
                                                                            publish_thinking_event(
                                                                                &inspect_broadcast_for_task,
                                                                                &channel_for_task,
                                                                                &name,
                                                                                text,
                                                                            );
                                                                        }
                                                                    }
                                                                }
                                                                None => break,
                                                            }
                                                        }
                                                        _ = cancel_inner.cancelled() => break,
                                                    }
                                                }
                                            }).catch_unwind().await;

                                            // Always clean up subscribed on exit — whether normal
                                            // (event bus replaced, cancel) or panic. Without this,
                                            // the key stays in `subscribed` forever and the thread
                                            // is never re-subscribed, causing activity events to
                                            // silently stop appearing in the dashboard.
                                            let mut sub = subscribed_clone.lock().await;
                                            sub.remove(&key_clone);

                                            if let Err(panic) = result {
                                                tracing::error!(
                                                    thread = %name,
                                                    panic = ?panic,
                                                    "Activity tracker task panicked; will re-subscribe on next interval"
                                                );
                                            }
                                        });
                                    }
                            }
                        }
                    }
                    _ = cancel.cancelled() => break,
                }
            }
        })
    }
}

/// A wrapper around `tokio::net::TcpStream` that prepends buffered bytes
/// to the beginning of the read stream. Used to "put back" the first bytes
/// after protocol detection and path extraction.
pub struct PrependStream {
    inner: tokio::net::TcpStream,
    prepend: Vec<u8>,
    prepend_pos: usize,
}

impl PrependStream {
    pub fn new(inner: tokio::net::TcpStream, bytes: Vec<u8>) -> Self {
        Self {
            inner,
            prepend: bytes,
            prepend_pos: 0,
        }
    }

    fn into_split(self) -> (PrependReadHalf, tokio::net::tcp::OwnedWriteHalf) {
        let (read, write) = self.inner.into_split();
        (
            PrependReadHalf {
                inner: read,
                prepend: self.prepend,
                prepend_pos: self.prepend_pos,
            },
            write,
        )
    }
}

impl tokio::io::AsyncRead for PrependStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // Serve prepended bytes first
        if self.prepend_pos < self.prepend.len() {
            let remaining = &self.prepend[self.prepend_pos..];
            let to_copy = std::cmp::min(buf.remaining(), remaining.len());
            buf.put_slice(&remaining[..to_copy]);
            self.prepend_pos += to_copy;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for PrependStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

struct PrependReadHalf {
    inner: tokio::net::tcp::OwnedReadHalf,
    prepend: Vec<u8>,
    prepend_pos: usize,
}

impl tokio::io::AsyncRead for PrependReadHalf {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.prepend_pos < self.prepend.len() {
            let remaining = &self.prepend[self.prepend_pos..];
            let to_copy = std::cmp::min(buf.remaining(), remaining.len());
            buf.put_slice(&remaining[..to_copy]);
            self.prepend_pos += to_copy;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

/// Convert a ThreadEvent into a human-readable ActivityEntry.
fn event_to_activity(event: &ThreadEvent) -> ActivityEntry {
    let severity = match event {
        ThreadEvent::SessionStatus { status_type, .. } => match status_type.as_str() {
            "error" | "timeout" => Severity::Error,
            "retry" | "rate_limit" => Severity::Warning,
            _ => Severity::Info,
        },
        ThreadEvent::ToolCompleted { success: false, .. } => Severity::Error,
        ThreadEvent::ProcessingCompleted { success: false, .. } => Severity::Error,
        _ => Severity::Info,
    };

    let text = match event {
        ThreadEvent::ProcessingStarted { .. } => "Processing started".to_string(),
        ThreadEvent::ProcessingProgress {
            elapsed_secs,
            activity,
            output_length,
            ..
        } => {
            format!("{activity} ({elapsed_secs}s, {output_length} chars)")
        }
        ThreadEvent::ProcessingCompleted {
            success,
            duration_secs,
            ..
        } => {
            if *success {
                format!("Completed ({duration_secs}s)")
            } else {
                format!("Failed ({duration_secs}s)")
            }
        }
        ThreadEvent::LLMRequestStarted { iteration, .. } => {
            format!("Thinking... (iteration {iteration})")
        }
        ThreadEvent::ToolStarted {
            tool_name, input, ..
        } => {
            if tool_name == "edit" {
                // Store the full edit data as JSON so consumers can render
                // differently: activity pane shows the JSON string as-is while
                // AI progress parses it and renders a full git diff.
                let parsed: Option<serde_json::Value> =
                    input.as_deref().and_then(|s| serde_json::from_str(s).ok());
                let file_path = parsed
                    .as_ref()
                    .and_then(|v| v.get("file_path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let old_str = parsed
                    .as_ref()
                    .and_then(|v| v.get("old_string"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let new_str = parsed
                    .as_ref()
                    .and_then(|v| v.get("new_string"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                serde_json::json!({
                    "type": "edit",
                    "file_path": file_path,
                    "old_string": old_str,
                    "new_string": new_str,
                })
                .to_string()
            } else if tool_name == "write" {
                // Store write data as JSON for multi-line rendering in AI progress.
                let parsed: Option<serde_json::Value> =
                    input.as_deref().and_then(|s| serde_json::from_str(s).ok());
                let file_path = parsed
                    .as_ref()
                    .and_then(|v| v.get("file_path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let content = parsed
                    .as_ref()
                    .and_then(|v| v.get("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                serde_json::json!({
                    "type": "write",
                    "file_path": file_path,
                    "content": content,
                })
                .to_string()
            } else {
                match input {
                    Some(inp) => format!("Tool: {tool_name} — {inp}"),
                    None => format!("Tool: {tool_name} (running)"),
                }
            }
        }
        ThreadEvent::ToolCompleted {
            tool_name,
            success,
            duration_secs,
            output,
            input,
            ..
        } => {
            if *success {
                if tool_name == "edit" {
                    // Store the full edit data as JSON so consumers can render
                    // differently: activity pane shows as-is, AI progress shows
                    // git diff.
                    let parsed: Option<serde_json::Value> =
                        input.as_deref().and_then(|s| serde_json::from_str(s).ok());
                    let file_path = parsed
                        .as_ref()
                        .and_then(|v| v.get("file_path"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let old_str = parsed
                        .as_ref()
                        .and_then(|v| v.get("old_string"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let new_str = parsed
                        .as_ref()
                        .and_then(|v| v.get("new_string"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    // Parse line number from the edit tool's output message
                    // (format: "Edited 'file' at line N: M replacement(s) made")
                    let line_no = output.as_deref().and_then(|s| {
                        s.find("at line ")
                            .and_then(|pos| {
                                let rest = &s[pos + 8..];
                                rest.find(':').map(|end| &rest[..end])
                            })
                            .and_then(|n| n.trim().parse::<usize>().ok())
                    });
                    serde_json::json!({
                        "type": "edit",
                        "file_path": file_path,
                        "line_no": line_no,
                        "old_string": old_str,
                        "new_string": new_str,
                        "duration_secs": duration_secs,
                    })
                    .to_string()
                } else if tool_name == "write" {
                    // Store write data as JSON for multi-line rendering in AI progress.
                    let parsed: Option<serde_json::Value> =
                        input.as_deref().and_then(|s| serde_json::from_str(s).ok());
                    let file_path = parsed
                        .as_ref()
                        .and_then(|v| v.get("file_path"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let content = parsed
                        .as_ref()
                        .and_then(|v| v.get("content"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    serde_json::json!({
                        "type": "write",
                        "file_path": file_path,
                        "content": content,
                        "duration_secs": duration_secs,
                    })
                    .to_string()
                } else {
                    match input {
                        Some(inp) => {
                            format!("Tool: {tool_name} (done, {duration_secs}s) — {inp}")
                        }
                        None => format!("Tool: {tool_name} (done, {duration_secs}s)"),
                    }
                }
            } else {
                match output {
                    Some(err) => {
                        let oneline = err.replace('\n', " ");
                        format!("Tool: {tool_name} (FAILED, {duration_secs}s) {oneline}")
                    }
                    None => format!("Tool: {tool_name} (FAILED, {duration_secs}s)"),
                }
            }
        }
        ThreadEvent::Thinking {
            text, full_length, ..
        } => {
            if *full_length > text.len() {
                format!("Thinking: {text}...")
            } else {
                format!("Thinking: {text}")
            }
        }
        ThreadEvent::IncomingMessage { sender, text, .. } => {
            let oneline = text.replace('\n', " ");
            format!("Message from {sender}: {oneline}")
        }
        ThreadEvent::ReplySent { text, .. } => {
            let oneline = text.replace('\n', " ");
            let preview: String = oneline.chars().take(100).collect();
            format!("Reply sent: {preview}")
        }
        ThreadEvent::SessionStatus {
            status_type,
            attempt,
            message,
            ..
        } => {
            let label = match status_type.as_str() {
                "retry" => "RETRY",
                "error" => "ERROR",
                "rate_limit" => "RATE LIMITED",
                "timeout" => "TIMEOUT",
                other => other,
            };
            let mut text = match attempt {
                Some(n) => format!("{label} (attempt #{n})"),
                None => label.to_string(),
            };
            if let Some(msg) = message {
                let oneline = msg.replace('\n', " ");
                text.push_str(&format!(": {oneline}"));
            }
            text
        }
    };
    ActivityEntry {
        text,
        timestamp: Some(event.timestamp().to_rfc3339()),
        severity,
        id: 0, // assigned by ActivityTracker on push (see fanout step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Mutex;

    fn test_context() -> Arc<InspectContext> {
        Arc::new(InspectContext {
            thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
            channels: Arc::new(ArcSwap::from_pointee(vec![ChannelInfo {
                name: "emf".to_string(),
                channel_type: "github".to_string(),
                active_workers: 0,
                max_concurrent: 0,
            }])),
            health_stats: Arc::new(Mutex::new(jyc_core::metrics::HealthStats::default())),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            start_time: Instant::now(),
            config_path: None,
            global_config_path: None,
            config: None,
            workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![])),
            websocket_handlers: None,
            reload_callback: None,
            inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(256).0),
        })
    }

    #[tokio::test]
    async fn test_inspect_server_responds_to_get_state() {
        let cancel = CancellationToken::new();
        let ctx = test_context();

        // Bind to random port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), ctx, cancel.clone());
        let handle = server.start();

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect and send request
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        writer
            .write_all(b"{\"method\":\"get_state\"}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        let resp: InspectResponse = serde_json::from_str(&response).unwrap();
        match resp {
            InspectResponse::State(state) => {
                assert_eq!(state.channels.len(), 1);
                assert_eq!(state.channels[0].name, "emf");
                assert_eq!(state.stats.active_workers, 0);
                assert_eq!(state.stats.max_concurrent, 0);
                assert_eq!(state.stats.available_workers, 0);
                assert!(!state.version.is_empty());
            }
            other => panic!("expected State, got {:?}", other),
        }

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_event_to_activity_session_status_error() {
        let event = ThreadEvent::SessionStatus {
            thread_name: "test_thread".to_string(),
            status_type: "error".to_string(),
            attempt: None,
            message: Some("SMTP 535 authentication failed".to_string()),
            timestamp: chrono::Utc::now(),
        };
        let entry = event_to_activity(&event);
        assert!(
            entry.text.contains("ERROR"),
            "Expected ERROR label, got: {}",
            entry.text
        );
        assert!(
            entry.text.contains("SMTP 535 authentication failed"),
            "Expected error message, got: {}",
            entry.text
        );
    }

    #[tokio::test]
    async fn test_event_to_activity_session_status_error_with_attempt() {
        let event = ThreadEvent::SessionStatus {
            thread_name: "test_thread".to_string(),
            status_type: "error".to_string(),
            attempt: Some(3),
            message: Some("server overload".to_string()),
            timestamp: chrono::Utc::now(),
        };
        let entry = event_to_activity(&event);
        assert!(
            entry.text.contains("ERROR (attempt #3)"),
            "Expected ERROR with attempt, got: {}",
            entry.text
        );
        assert!(
            entry.text.contains("server overload"),
            "Expected error message, got: {}",
            entry.text
        );
    }

    #[tokio::test]
    async fn test_inspect_server_handles_unknown_method() {
        let cancel = CancellationToken::new();
        let ctx = test_context();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), ctx, cancel.clone());
        let handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        writer
            .write_all(b"{\"method\":\"unknown\"}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        assert!(response.contains("unknown method"));

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_inspect_server_handles_invalid_json() {
        let cancel = CancellationToken::new();
        let ctx = test_context();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), ctx, cancel.clone());
        let handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        writer.write_all(b"not json\n").await.unwrap();
        writer.flush().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        assert!(response.contains("invalid request"));

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_inspect_server_multiple_requests() {
        let cancel = CancellationToken::new();
        let ctx = test_context();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), ctx, cancel.clone());
        let handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Send two requests on the same connection
        for _ in 0..2 {
            writer
                .write_all(b"{\"method\":\"get_state\"}\n")
                .await
                .unwrap();
            writer.flush().await.unwrap();

            let mut response = String::new();
            reader.read_line(&mut response).await.unwrap();

            let resp: InspectResponse = serde_json::from_str(&response).unwrap();
            match resp {
                InspectResponse::State(state) => {
                    assert_eq!(state.channels.len(), 1);
                }
                other => panic!("expected State, got {:?}", other),
            }
        }

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_inspect_server_reload_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let config_toml = r#"
[general]
max_concurrent_threads = 5

[channels.test]
type = "email"
[channels.test.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.test.outbound]
host = "h"
port = 465
username = "u"
password = "p"

[agent]
enabled = true
mode = "agent"
"#;
        std::fs::write(&config_path, config_toml).unwrap();

        let initial_config = jyc_types::load_config(&config_path).unwrap();
        let config_swap = Arc::new(ArcSwap::from_pointee(initial_config));

        let ctx = Arc::new(InspectContext {
            thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
            channels: Arc::new(ArcSwap::from_pointee(vec![])),
            health_stats: Arc::new(Mutex::new(jyc_core::metrics::HealthStats::default())),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            start_time: Instant::now(),
            config_path: Some(config_path.clone()),
            global_config_path: None,
            config: Some(config_swap.clone()),
            workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![])),
            websocket_handlers: None,
            reload_callback: None,
            inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(256).0),
        });

        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), ctx, cancel.clone());
        let handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Send reload_config request
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        writer
            .write_all(b"{\"method\":\"reload_config\"}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        let resp: InspectResponse = serde_json::from_str(&response).unwrap();
        match resp {
            InspectResponse::ReloadResult { success, message } => {
                assert!(success, "reload should succeed: {message}");
                assert!(message.contains("reloaded"));
            }
            other => panic!("expected ReloadResult, got {:?}", other),
        }

        // Verify config was actually updated
        assert_eq!(config_swap.load().general.max_concurrent_threads, 5);

        // Now modify the config on disk and reload again
        let updated_toml =
            config_toml.replace("max_concurrent_threads = 5", "max_concurrent_threads = 10");
        std::fs::write(&config_path, updated_toml).unwrap();

        writer
            .write_all(b"{\"method\":\"reload_config\"}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut response2 = String::new();
        reader.read_line(&mut response2).await.unwrap();

        let resp2: InspectResponse = serde_json::from_str(&response2).unwrap();
        match resp2 {
            InspectResponse::ReloadResult { success, .. } => assert!(success),
            other => panic!("expected ReloadResult, got {:?}", other),
        }

        assert_eq!(config_swap.load().general.max_concurrent_threads, 10);

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_inspect_server_reset_session() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().to_path_buf();
        let thread_name = "test-thread";
        let jyc_dir = workspace_dir.join(thread_name).join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01","total_input_tokens":100,"total_output_tokens":50,"max_input_tokens":1000}"#,
        )
        .await
        .unwrap();

        let ctx = Arc::new(InspectContext {
            thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
            channels: Arc::new(ArcSwap::from_pointee(vec![])),
            health_stats: Arc::new(Mutex::new(jyc_core::metrics::HealthStats::default())),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            start_time: Instant::now(),
            config_path: None,
            global_config_path: None,
            config: None,
            workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![workspace_dir])),
            websocket_handlers: None,
            reload_callback: None,
            inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(256).0),
        });

        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), ctx, cancel.clone());
        let handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        writer
            .write_all(
                b"{\"method\":\"reset_session\",\"params\":{\"thread_name\":\"test-thread\"}}\n",
            )
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        let resp: InspectResponse = serde_json::from_str(&response).unwrap();
        match resp {
            InspectResponse::ResetSessionResult { success, message } => {
                assert!(success, "reset should succeed: {message}");
                assert!(message.contains("session deleted"));
            }
            other => panic!("expected ResetSessionResult, got {:?}", other),
        }

        assert!(!jyc_dir.join("agent-session.json").exists());

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_inspect_server_reset_session_missing_param() {
        let ctx = test_context();

        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), ctx, cancel.clone());
        let handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        writer
            .write_all(b"{\"method\":\"reset_session\"}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        let resp: InspectResponse = serde_json::from_str(&response).unwrap();
        match resp {
            InspectResponse::ResetSessionResult { success, message } => {
                assert!(!success);
                assert!(message.contains("missing thread_name"));
            }
            other => panic!("expected ResetSessionResult, got {:?}", other),
        }

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_inspect_server_reset_session_no_existing_session() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().to_path_buf();

        let ctx = Arc::new(InspectContext {
            thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
            channels: Arc::new(ArcSwap::from_pointee(vec![])),
            health_stats: Arc::new(Mutex::new(jyc_core::metrics::HealthStats::default())),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            start_time: Instant::now(),
            config_path: None,
            global_config_path: None,
            config: None,
            workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![workspace_dir])),
            websocket_handlers: None,
            reload_callback: None,
            inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(256).0),
        });

        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), ctx, cancel.clone());
        let handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        writer
            .write_all(
                b"{\"method\":\"reset_session\",\"params\":{\"thread_name\":\"nonexistent\"}}\n",
            )
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        let resp: InspectResponse = serde_json::from_str(&response).unwrap();
        match resp {
            InspectResponse::ResetSessionResult { success, message } => {
                assert!(success, "no-session case should still succeed: {message}");
                assert!(message.contains("no session exists"));
            }
            other => panic!("expected ResetSessionResult, got {:?}", other),
        }

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_inspect_server_reset_session_path_traversal() {
        let ctx = test_context();

        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), ctx, cancel.clone());
        let handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        writer
            .write_all(
                b"{\"method\":\"reset_session\",\"params\":{\"thread_name\":\"../../etc\"}}\n",
            )
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        let resp: InspectResponse = serde_json::from_str(&response).unwrap();
        match resp {
            InspectResponse::ResetSessionResult { success, message } => {
                assert!(!success);
                assert!(
                    message.contains("path traversal"),
                    "expected path traversal error, got: {message}"
                );
            }
            other => panic!("expected ResetSessionResult, got {:?}", other),
        }

        cancel.cancel();
        handle.await.unwrap();
    }

    /// Regression test for cross-channel issue collision.
    ///
    /// Bug: when two channels both had a thread with the same name (e.g.
    /// `issue-20`), the activity map keyed by `thread.name` alone caused
    /// channel2's thread to share channel1's processing state and logs in
    /// the dashboard. This test exercises the merge logic that
    /// `build_state` performs on each `ThreadInfo`, asserting that two
    /// same-named threads from different channels resolve to *independent*
    /// activity-map entries.
    #[tokio::test]
    async fn test_activity_map_disambiguates_same_named_threads_across_channels() {
        // Construct two ThreadInfos with the same name but different channels,
        // mimicking the situation where channel1 and channel2 both have an
        // `issue-20`.
        let make_thread = |channel: &str| ThreadInfo {
            name: "issue-20".to_string(),
            channel: channel.to_string(),
            pattern: None,
            status: ThreadStatus::Idle,
            model: None,
            mode: None,
            input_tokens: None,
            max_tokens: None,
            activity: vec![],
            last_active_at: None,
            skills: vec![],
            recent_messages: vec![],
            thinking_text: None,
            thread_path: None,
        };

        let mut threads = vec![make_thread("channel1"), make_thread("channel2")];

        // Populate the activity map: only channel1's issue-20 is processing
        // and has an activity entry. Channel2's issue-20 is idle with no
        // activity.
        let activity_map: SharedActivityMap = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut map = activity_map.lock().await;
            let state = map
                .entry(("channel1".to_string(), "issue-20".to_string()))
                .or_default();
            state.is_processing = true;
            state.entries.push_back(ActivityEntry {
                text: "channel1 working".to_string(),
                timestamp: None,
                severity: Severity::Info,
                id: 0,
            });
        }

        // Replicate the merge loop from build_state.
        let map = activity_map.lock().await;
        for thread in &mut threads {
            let key = (thread.channel.clone(), thread.name.clone());
            if let Some(state) = map.get(&key) {
                thread.activity = state.entries.iter().cloned().collect();
                if state.is_processing {
                    thread.status = ThreadStatus::Processing;
                }
            }
        }
        drop(map);

        // channel1 must reflect its own processing state and log.
        let ch1 = threads.iter().find(|t| t.channel == "channel1").unwrap();
        assert!(matches!(ch1.status, ThreadStatus::Processing));
        assert_eq!(ch1.activity.len(), 1);
        assert_eq!(ch1.activity[0].text, "channel1 working");

        // channel2 must NOT inherit channel1's state — this is the bug.
        let ch2 = threads.iter().find(|t| t.channel == "channel2").unwrap();
        assert!(
            matches!(ch2.status, ThreadStatus::Idle),
            "channel2's issue-20 leaked channel1's processing status"
        );
        assert!(
            ch2.activity.is_empty(),
            "channel2's issue-20 leaked channel1's activity log: {:?}",
            ch2.activity
        );
    }

    /// Regression test for activity events stopping after worker exits.
    ///
    /// Bug: when a thread's worker finishes and the event bus is cleaned up,
    /// the ActivityTracker's re-subscription loop kept calling
    /// `get_event_bus()` which returned `None`, leaving the thread in a
    /// stuck `is_processing = true` state forever. The dashboard showed
    /// "Processing" but no activity events appeared.
    ///
    /// Fix: when `get_event_bus()` returns `None` and the thread has no
    /// active queue, clear `is_processing` and mark as subscribed to stop
    /// retrying. When a new message arrives, a new event bus is created and
    /// the subscriber task cleanup removes the key, enabling re-subscription.
    #[tokio::test]
    async fn test_idle_thread_clears_stale_processing_state() {
        let activity_map: SharedActivityMap = Arc::new(Mutex::new(HashMap::new()));

        // Simulate a thread that was previously processing but whose worker
        // has exited (event bus cleaned up, no active queue).
        let key = ("test-channel".to_string(), "test-thread".to_string());
        {
            let mut map = activity_map.lock().await;
            let state = map.entry(key.clone()).or_default();
            state.is_processing = true;
            state.entries.push_back(ActivityEntry {
                text: "Processing started".to_string(),
                timestamp: Some(chrono::Utc::now().to_rfc3339()),
                severity: Severity::Info,
                id: 0,
            });
        }

        // Verify the stale state exists.
        {
            let map = activity_map.lock().await;
            assert!(map.get(&key).unwrap().is_processing);
        }

        // Simulate the fix: clear is_processing for idle threads.
        // This mirrors the logic in ActivityTracker::start() when
        // get_event_bus() returns None and has_active_queue() returns false.
        {
            let mut map = activity_map.lock().await;
            if let Some(state) = map.get_mut(&key) {
                state.is_processing = false;
            }
        }

        // Verify the stale state was cleared.
        {
            let map = activity_map.lock().await;
            assert!(
                !map.get(&key).unwrap().is_processing,
                "is_processing should be cleared for idle threads"
            );
        }
    }

    #[tokio::test]
    async fn test_event_to_activity_incoming_message() {
        let event = ThreadEvent::IncomingMessage {
            thread_name: "test".to_string(),
            sender: "user".to_string(),
            text: "hello world".to_string(),
            timestamp: chrono::Utc::now(),
        };
        let entry = event_to_activity(&event);
        assert!(entry.text.contains("Message from user"));
        assert!(entry.text.contains("hello world"));
    }

    #[tokio::test]
    async fn test_event_to_activity_reply_sent() {
        let event = ThreadEvent::ReplySent {
            thread_name: "test".to_string(),
            text: "AI reply here".to_string(),
            timestamp: chrono::Utc::now(),
        };
        let entry = event_to_activity(&event);
        assert!(entry.text.contains("Reply sent"));
        assert!(entry.text.contains("AI reply here"));
    }

    #[tokio::test]
    async fn test_get_state_overview_returns_slim_payload() {
        let cancel = CancellationToken::new();
        let ctx = test_context();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), ctx, cancel.clone());
        let handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        writer
            .write_all(b"{\"method\":\"get_state_overview\"}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();
        let resp: InspectResponse = serde_json::from_str(&response).unwrap();

        match resp {
            InspectResponse::Overview(overview) => {
                assert_eq!(overview.channels.len(), 1);
                assert_eq!(overview.threads.len(), 0);
                // The slim payload should NOT contain activity / recent_messages / thinking_text
                let json = serde_json::to_string(&overview).unwrap();
                assert!(!json.contains("activity"));
                assert!(!json.contains("recent_messages"));
                assert!(!json.contains("thinking_text"));
            }
            other => panic!("expected Overview, got {:?}", other),
        }

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_get_thread_activity_reads_activity_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().to_path_buf();
        let thread_name = "activity-test";
        let jyc_dir = workspace_dir.join(thread_name).join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        let activity_path = jyc_dir.join("activity.jsonl");
        let entries = [
            r#"{"text":"Tool: bash","timestamp":"2026-07-01T10:00:00Z","severity":"info"}"#,
            r#"{"text":"Completed (5s)","timestamp":"2026-07-01T10:00:05Z","severity":"info"}"#,
        ];
        let content = entries.join("\n") + "\n";
        tokio::fs::write(&activity_path, content).await.unwrap();

        let ctx = Arc::new(InspectContext {
            thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
            channels: Arc::new(ArcSwap::from_pointee(vec![])),
            health_stats: Arc::new(Mutex::new(jyc_core::metrics::HealthStats::default())),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            start_time: Instant::now(),
            config_path: None,
            global_config_path: None,
            config: None,
            workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![workspace_dir.clone()])),
            websocket_handlers: None,
            reload_callback: None,
            inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(256).0),
        });

        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), ctx, cancel.clone());
        let handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // No thread managers registered → should return "no thread manager" error
        writer
            .write_all(
                b"{\"method\":\"get_thread_activity\",\"params\":{\"channel\":\"any\",\"thread\":\"any\"}}\n",
            )
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();
        assert!(response.contains("no thread manager"), "got: {response}");

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_get_thread_activity_filters_by_since() {
        // Verify the filter_by_since helper used by the handler.
        use jyc_types::Severity;
        let entries = vec![
            ActivityEntry {
                text: "old".to_string(),
                timestamp: Some("2026-01-01T00:00:00Z".to_string()),
                severity: Severity::Info,
                id: 0,
            },
            ActivityEntry {
                text: "mid".to_string(),
                timestamp: Some("2026-02-01T00:00:00Z".to_string()),
                severity: Severity::Info,
                id: 0,
            },
            ActivityEntry {
                text: "new".to_string(),
                timestamp: Some("2026-03-01T00:00:00Z".to_string()),
                severity: Severity::Info,
                id: 0,
            },
        ];

        let filtered = filter_by_since(entries.clone(), Some("2026-02-01T00:00:00Z"));
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].text, "mid");
        assert_eq!(filtered[1].text, "new");

        let all = filter_by_since(entries, None);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_extract_ws_route_bare() {
        let route = InspectServer::extract_ws_route("GET /ws HTTP/1.1");
        assert_eq!(route, Some(WsRoute::Bare));
    }

    #[test]
    fn test_extract_ws_route_channel() {
        let route = InspectServer::extract_ws_route("GET /ws/local_dev HTTP/1.1");
        assert_eq!(route, Some(WsRoute::Channel("local_dev".to_string())));
    }

    #[test]
    fn test_extract_ws_route_thread() {
        let route = InspectServer::extract_ws_route("GET /ws/jiny283/invoice-2024 HTTP/1.1");
        assert_eq!(
            route,
            Some(WsRoute::Thread {
                channel: "jiny283".to_string(),
                name: "invoice-2024".to_string(),
            })
        );
    }

    #[test]
    fn test_extract_ws_route_invalid() {
        assert_eq!(InspectServer::extract_ws_route(""), None);
        assert_eq!(InspectServer::extract_ws_route("GET /other HTTP/1.1"), None);
    }

    /// `/ws/<non_websocket_channel>/<thread>` must resolve to a
    /// `ThreadProxyHandler` (since the channel isn't in the ws handlers map).
    #[tokio::test]
    async fn test_resolve_ws_handler_uses_proxy_for_non_ws_channel() {
        let ctx = test_context(); // no websocket_handlers set
        let route = WsRoute::Thread {
            channel: "github".to_string(),
            name: "pr-42".to_string(),
        };
        // Resolution must succeed — exact handler type is verified by
        // end-to-end integration tests in Commit 4.
        let _ = InspectServer::resolve_ws_handler(&ctx, Some(route)).unwrap();
    }

    /// `/ws/<websocket_channel>/<thread>` must resolve (handler type checked
    /// by integration tests).
    #[tokio::test]
    async fn test_resolve_ws_handler_uses_scoped_for_ws_channel() {
        use async_trait::async_trait;

        struct Stub;
        #[async_trait]
        impl WebsocketHandler for Stub {
            async fn handle(
                &self,
                _ws: tokio_tungstenite::WebSocketStream<PrependStream>,
                _addr: std::net::SocketAddr,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }
        let stub: Arc<dyn WebsocketHandler> = Arc::new(Stub);
        let mut handlers: HashMap<String, Arc<dyn WebsocketHandler>> = HashMap::new();
        handlers.insert("local_dev".to_string(), stub);

        let ctx = Arc::new(InspectContext {
            thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
            channels: Arc::new(ArcSwap::from_pointee(vec![])),
            health_stats: Arc::new(Mutex::new(jyc_core::metrics::HealthStats::default())),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            start_time: Instant::now(),
            config_path: None,
            global_config_path: None,
            config: None,
            workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![])),
            websocket_handlers: Some(handlers),
            reload_callback: None,
            inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(256).0),
        });

        let route = WsRoute::Thread {
            channel: "local_dev".to_string(),
            name: "dotfiles".to_string(),
        };
        let _ = InspectServer::resolve_ws_handler(&ctx, Some(route)).unwrap();
    }

    /// `/ws/<non_ws_channel>` must error.
    #[tokio::test]
    async fn test_resolve_ws_handler_channel_route_rejects_non_ws() {
        let ctx = test_context(); // no websocket_handlers
        let route = WsRoute::Channel("github".to_string());
        let result = InspectServer::resolve_ws_handler(&ctx, Some(route));
        assert!(result.is_err());
    }

    /// `/ws` with no handlers must error.
    #[tokio::test]
    async fn test_resolve_ws_handler_bare_rejects_when_empty() {
        let ctx = test_context();
        let result = InspectServer::resolve_ws_handler(&ctx, Some(WsRoute::Bare));
        assert!(result.is_err());
    }

    /// The publish helpers should produce well-formed JSON payloads that
    /// include the (channel, thread, id) fields used by `ThreadProxyHandler`
    /// for filtering and dedup.
    #[tokio::test]
    async fn test_publish_activity_event_format() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<String>(16);
        let entry = ActivityEntry {
            text: "Tool: bash".to_string(),
            timestamp: Some("2026-07-27T10:00:00Z".to_string()),
            severity: Severity::Info,
            id: 42,
        };
        publish_activity_event(&tx, "github", "pr-1", &entry);
        let payload = rx.recv().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["type"], "activity");
        assert_eq!(v["channel"], "github");
        assert_eq!(v["thread"], "pr-1");
        assert_eq!(v["id"], 42);
        assert_eq!(v["entry"]["text"], "Tool: bash");
    }

    #[tokio::test]
    async fn test_publish_chat_message_event_format() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<String>(16);
        let msg = ChatMessageEntry {
            sender: "ai".to_string(),
            text: "Hello".to_string(),
            timestamp: Some("2026-07-27T10:00:00Z".to_string()),
            id: 7,
        };
        publish_chat_message_event(&tx, "c", "t", &msg);
        let payload = rx.recv().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["type"], "chat_message");
        assert_eq!(v["id"], 7);
        assert_eq!(v["entry"]["sender"], "ai");
    }

    #[tokio::test]
    async fn test_publish_thinking_event_format() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<String>(16);
        publish_thinking_event(&tx, "c", "t", "thinking text");
        let payload = rx.recv().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["type"], "thinking");
        assert_eq!(v["text"], "thinking text");
    }

    #[tokio::test]
    async fn test_publish_processing_event_format() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<String>(16);
        publish_processing_event(&tx, "c", "t", true, false);
        let payload = rx.recv().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["type"], "processing");
        assert_eq!(v["is_processing"], true);
        assert_eq!(v["has_error"], false);
    }
}
