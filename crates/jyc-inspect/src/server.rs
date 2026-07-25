use arc_swap::ArcSwap;
use axum::{
    extract::{
        ws::WebSocketUpgrade,
        ConnectInfo, Path, State,
    },
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use jyc_core::activity_log_store::ActivityLogStore;
use jyc_core::command::{all_commands, list_available_models};
use jyc_core::metrics::SharedHealthStats;
use jyc_core::thread_event::ThreadEvent;
use jyc_core::thread_manager::ThreadManager;
use jyc_types::AppConfig;
use jyc_types::*;

/// Handler for WebSocket connections on the inspect server.
///
/// The websocket channel registers itself as the handler. When a dashboard
/// client connects via WebSocket upgrade on `/ws[/<channel>]`, the inspect
/// server hands the upgraded socket to this handler.
#[async_trait::async_trait]
pub trait WebsocketHandler: Send + Sync {
    /// Handle a single WebSocket connection.
    async fn handle(
        &self,
        socket: axum::extract::ws::WebSocket,
        addr: SocketAddr,
    ) -> anyhow::Result<()>;
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
    /// `GET /ws/<channel>` routes to `handlers[channel]`.
    /// `GET /ws` (no channel) routes to the first available handler.
    pub websocket_handlers: Option<HashMap<String, Arc<dyn WebsocketHandler>>>,
    /// Optional reload callback — invoked after config is swapped atomically.
    pub reload_callback: Option<ReloadCallback>,
}

/// HTTP-based inspect server.
///
/// Listens on the configured bind address and serves:
///
/// - `GET  /health`           — liveness probe
/// - `GET  /state`            — full runtime state snapshot
/// - `POST /reload_config`    — reload config from disk and apply
/// - `POST /reset_session`    — delete the session file for a thread
/// - `POST /inject_message`   — enqueue a synthetic message into a thread
/// - `GET  /ws[/<channel>]`   — WebSocket upgrade for chat
///
/// All non-loopback requests require `Authorization: Bearer <token>` matching
/// the contents of `<data_dir>/inspect-token`. Tokens are re-read fresh on
/// every connection so rotation takes effect immediately.
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

        let app = build_router(self.context.clone());
        let service = app.into_make_service_with_connect_info::<SocketAddr>();

        let server = axum::serve(listener, service).with_graceful_shutdown(async move {
            self.cancel.cancelled().await;
        });

        if let Err(e) = server.await {
            tracing::error!(error = %e, "Inspect server bind/serve error");
        }
        tracing::debug!("Inspect server shutting down");
        Ok(())
    }
}

/// Build the HTTP router for the inspect server.
///
/// Exposed for integration tests that want to drive the router directly via
/// `axum::serve` against a random port.
pub fn build_router(context: Arc<InspectContext>) -> Router {
    Router::new()
        .route("/health", get(handle_health))
        .route("/state", get(handle_get_state))
        .route("/reload_config", post(handle_reload_config))
        .route("/reset_session", post(handle_reset_session))
        .route("/inject_message", post(handle_inject_message))
        .route("/ws", get(ws_upgrade_root))
        .route("/ws/{channel}", get(ws_upgrade_channel))
        .layer(middleware::from_fn_with_state(
            context.clone(),
            auth_middleware,
        ))
        .with_state(context)
}

// ── Auth middleware ──

/// Authenticate every non-loopback request via `Authorization: Bearer <token>`.
///
/// - Loopback (127.0.0.0/8, ::1, ::ffff:127.0.0.1): bypass entirely, no file read.
/// - Non-loopback: read `<data_dir>/inspect-token` fresh, constant-time compare.
///
/// Loopback bypass matches the original TCP-line-protocol design.
async fn auth_middleware(
    State(_ctx): State<Arc<InspectContext>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    if is_loopback(addr.ip()) {
        return next.run(req).await;
    }

    let token = match extract_bearer(&req.headers()) {
        Some(t) => t,
        None => {
            return ApiError::unauthorized("missing Authorization: Bearer header").into_response();
        }
    };

    match verify_token(token) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

/// Return true for loopback IPv4 (127.0.0.0/8) and IPv6 (`::1` and the
/// IPv4-mapped form `::ffff:127.0.0.1`). Private ranges (10/8, 172.16/12,
/// 192.168/16) are deliberately NOT considered loopback — those hit Docker
/// bridges and corporate VPNs and would silently fail-open for a surprising
/// surface.
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return true;
            }
            // IPv4-mapped IPv6 (::ffff:127.0.0.1)
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return mapped.is_loopback();
            }
            false
        }
    }
}

/// Parse `Authorization: Bearer <token>` from headers. Case-insensitive header
/// name, case-insensitive scheme.
pub fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let mut parts = value.splitn(2, char::is_whitespace);
    let scheme = parts.next()?;
    let token = parts.next()?.trim();
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    if token.is_empty() {
        return None;
    }
    Some(token)
}

/// Verify a token against the on-disk token file at `<data_dir>/inspect-token`.
///
/// Read fresh on every call so rotation takes effect immediately for new
/// connections. Uses constant-time comparison via `subtle::ConstantTimeEq`.
fn verify_token(provided: &str) -> Result<(), ApiError> {
    let expected = jyc_utils::inspect_token::read()
        .map_err(|e| ApiError::internal(format!("failed to read token file: {e}")))?;

    let Some(expected) = expected else {
        return Err(ApiError::unauthorized(
            "no token file; run `jyc token generate` to enable remote access",
        ));
    };

    if expected.as_bytes().ct_eq(provided.as_bytes()).into() {
        Ok(())
    } else {
        Err(ApiError::unauthorized("invalid token"))
    }
}

// ── API error type ──

/// Error type that maps to an HTTP status code + JSON `{"error":"…"}` body.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self::internal(format!("{e:#}"))
    }
}

// ── HTTP handlers ──

async fn handle_health() -> Json<HealthResponse> {
    Json(HealthResponse::ok())
}

async fn handle_get_state(
    State(ctx): State<Arc<InspectContext>>,
) -> Result<Json<InspectState>, ApiError> {
    let state = build_state(&ctx).await;
    Ok(Json(state))
}

async fn handle_reload_config(
    State(ctx): State<Arc<InspectContext>>,
) -> Result<Json<ReloadResult>, ApiError> {
    let result = reload_config_impl(&ctx).await;
    Ok(Json(result))
}

async fn handle_reset_session(
    State(ctx): State<Arc<InspectContext>>,
    Json(req): Json<ResetSessionRequest>,
) -> Result<Json<ResetSessionResult>, ApiError> {
    let result = reset_session_impl(&ctx, &req.thread_name).await?;
    Ok(Json(result))
}

async fn handle_inject_message(
    State(ctx): State<Arc<InspectContext>>,
    Json(req): Json<InjectMessageRequest>,
) -> Result<Json<InjectMessageResult>, ApiError> {
    let result = inject_message_impl(&ctx, &req).await?;
    Ok(Json(result))
}

async fn ws_upgrade_root(
    State(ctx): State<Arc<InspectContext>>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    ws_upgrade_impl(ctx, ws, addr, None).await
}

async fn ws_upgrade_channel(
    State(ctx): State<Arc<InspectContext>>,
    Path(channel): Path<String>,
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    ws_upgrade_impl(ctx, ws, addr, Some(channel)).await
}

async fn ws_upgrade_impl(
    ctx: Arc<InspectContext>,
    ws: WebSocketUpgrade,
    addr: SocketAddr,
    channel: Option<String>,
) -> Response {
    let handler = resolve_ws_handler(&ctx, channel.as_deref());
    match handler {
        Some(handler) => ws.on_upgrade(move |socket| async move {
            if let Err(e) = handler.handle(socket, addr).await {
                tracing::warn!(error = %e, addr = %addr, "websocket handler error");
            }
        }),
        None => ApiError::not_found("no websocket handler for this channel").into_response(),
    }
}

fn resolve_ws_handler(
    context: &InspectContext,
    channel: Option<&str>,
) -> Option<Arc<dyn WebsocketHandler>> {
    let handlers = context.websocket_handlers.as_ref()?;
    match channel {
        Some(name) => handlers.get(name).cloned(),
        None => handlers.values().next().cloned(),
    }
}

// ── Business logic (extracted from handlers for testability) ──

/// Load stored routing metadata for a thread from `.jyc/thread-meta.json`.
///
/// Written by `process_message()` on the first message for a thread.
/// Returns `None` if the file doesn't exist or can't be parsed.
async fn load_thread_meta(
    tm: &Arc<ThreadManager>,
    thread_name: &str,
) -> Option<serde_json::Value> {
    let thread_path = tm.thread_path(thread_name).await?;
    let meta_path = thread_path.join(".jyc").join("thread-meta.json");
    let content = tokio::fs::read_to_string(&meta_path).await.ok()?;
    serde_json::from_str(&content).ok()
}

/// Inject a message into a thread's queue for AI processing.
async fn inject_message_impl(
    context: &InspectContext,
    req: &InjectMessageRequest,
) -> Result<InjectMessageResult, ApiError> {
    // Find the ThreadManager for this channel
    let tms = context.thread_managers.load();
    let tm = tms
        .iter()
        .find(|tm| tm.channel_name() == req.channel)
        .cloned();
    drop(tms);
    let tm = tm.ok_or_else(|| {
        ApiError::not_found(format!("no thread manager found for channel '{}'", req.channel))
    })?;

    // Load stored routing metadata for this thread (written on first message).
    let thread_meta = load_thread_meta(&tm, &req.thread).await;
    let (channel_uid, external_id, thread_refs, metadata) = match thread_meta {
        Some(meta) => {
            let uid = meta
                .get("channel_uid")
                .and_then(|v| v.as_str())
                .unwrap_or("dashboard")
                .to_string();
            let ext_id = meta
                .get("external_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let refs = meta
                .get("thread_refs")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            let md = meta
                .get("metadata")
                .and_then(|v| v.as_object())
                .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            (uid, ext_id, refs, md)
        }
        None => ("dashboard".to_string(), None, None, HashMap::new()),
    };

    // Build synthetic InboundMessage (same pattern as send_to_thread tool)
    let message = InboundMessage {
        id: format!("inspect-{}", chrono::Utc::now().timestamp_millis()),
        channel: req.channel.clone(),
        channel_uid,
        sender: "dashboard".to_string(),
        sender_address: "dashboard@inspect".to_string(),
        recipients: vec![],
        topic: req.thread.clone(),
        content: MessageContent {
            text: Some(req.text.clone()),
            html: None,
            markdown: None,
        },
        timestamp: chrono::Utc::now(),
        thread_refs,
        reply_to_id: None,
        external_id,
        attachments: vec![],
        metadata,
        matched_pattern: None,
    };

    let pattern_match = PatternMatch {
        pattern_name: String::new(),
        channel: req.channel.clone(),
        matches: HashMap::new(),
    };

    tm.enqueue(
        message,
        req.thread.clone(),
        pattern_match,
        None,
        true,
        None,
    )
    .await;

    tracing::info!(
        channel = %req.channel,
        thread = %req.thread,
        text_len = req.text.len(),
        "Dashboard message injected"
    );

    Ok(InjectMessageResult {
        success: true,
        message: format!("message injected into {}/{}", req.channel, req.thread),
    })
}

/// Reload configuration from disk and swap it atomically.
async fn reload_config_impl(context: &InspectContext) -> ReloadResult {
    let (config_path, config_swap) = match (&context.config_path, &context.config) {
        (Some(path), Some(config)) => (path.clone(), config.clone()),
        _ => {
            return ReloadResult {
                success: false,
                message: "config reload not available (no config path)".to_string(),
            };
        }
    };

    tracing::info!(path = %config_path.display(), "Reloading configuration");

    // Load and validate new config (layered: global base + workdir overlay)
    let new_config = match jyc_types::load_config_layered(
        context.global_config_path.as_deref(),
        &config_path,
    ) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to load config: {e:#}");
            tracing::warn!("{msg}");
            return ReloadResult {
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
        return ReloadResult {
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
            return ReloadResult {
                success: false,
                message: msg,
            };
        }
    }

    ReloadResult {
        success: true,
        message: "configuration reloaded".to_string(),
    }
}

/// Delete the agent session file for a given thread.
async fn reset_session_impl(
    context: &InspectContext,
    thread_name: &str,
) -> Result<ResetSessionResult, ApiError> {
    if thread_name.contains("..") || thread_name.contains('/') || thread_name.contains('\\') {
        return Err(ApiError::bad_request(
            "invalid thread_name: path traversal not allowed",
        ));
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
        if let Err(e) = tm.reset_session(thread_name, &config).await {
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
                .join(thread_name)
                .join(".jyc")
                .join("agent-session.json");
            if session_path.exists() {
                tokio::fs::remove_file(&session_path).await.ok();
                deleted = true;
            }
            let context_path = dir
                .join(thread_name)
                .join(".jyc")
                .join("agent-context.json");
            if context_path.exists() {
                tokio::fs::remove_file(&context_path).await.ok();
                deleted = true;
            }
        }

        if deleted {
            tracing::info!(thread = %thread_name, "Session reset via inspect protocol (filesystem fallback)");
            Ok(ResetSessionResult {
                success: true,
                message: format!("session deleted for {thread_name}"),
            })
        } else {
            Ok(ResetSessionResult {
                success: true,
                message: format!("no session exists for {thread_name}"),
            })
        }
    } else {
        tracing::info!(thread = %thread_name, "Session reset via inspect protocol");
        Ok(ResetSessionResult {
            success: true,
            message: format!("session reset for {thread_name}"),
        })
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

// ── Activity tracker (unchanged) ──

/// Background task that subscribes to thread event buses and buffers
/// activity entries for the inspect server.
pub struct ActivityTracker;

impl ActivityTracker {
    /// Start tracking activity for all thread managers.
    /// Periodically discovers new threads and subscribes to their event buses.
    /// Persists activity entries to `.jyc/activity.jsonl` per thread.
    /// On startup, loads historical activity from disk.
    pub fn start(
        thread_managers: Arc<ArcSwap<Vec<Arc<ThreadManager>>>>,
        activity_map: SharedActivityMap,
        _workspace_dirs: Arc<ArcSwap<Vec<PathBuf>>>,
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
                                                                            })
                                                                        }
                                                                        ThreadEvent::ReplySent { text, timestamp, .. } => {
                                                                            Some(ChatMessageEntry {
                                                                                sender: "ai".to_string(),
                                                                                text: text.clone(),
                                                                                timestamp: Some(timestamp.to_rfc3339()),
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
                                                                        let entry = event_to_activity(&event);
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
                                                                        // ProcessingProgress is a heartbeat, not a discrete
                                                                        // activity. Persist it to disk but skip the in-memory
                                                                        // activity log so it doesn't crowd out ToolStarted /
                                                                        // ToolCompleted entries that show the actual tool name.
                                                                        if !is_progress {
                                                                            state.entries.push_back(entry);
                                                                            if state.entries.len() > MAX_ACTIVITY_ENTRIES {
                                                                                state.entries.pop_front();
                                                                            }
                                                                        }
                                                                        if let Some(msg) = chat_msg {
                                                                            state.recent_messages.push_back(msg);
                                                                            if state.recent_messages.len() > MAX_RECENT_MESSAGES {
                                                                                state.recent_messages.pop_front();
                                                                            }
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
                                                                        // Thinking event: update thinking_text only.
                                                                        if let ThreadEvent::Thinking { ref text, .. } = event {
                                                                            let mut map = map.lock().await;
                                                                            let state = map
                                                                                .entry((channel_for_task.clone(), name.clone()))
                                                                                .or_default();
                                                                            state.thinking_text = Some(text.clone());
                                                                            state.last_active_at = Some(event.timestamp());
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

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
        })
    }

    /// Bind a fresh axum server to `127.0.0.1:0` and return its base URL + handle.
    async fn spawn_test_server(
        context: Arc<InspectContext>,
        cancel: CancellationToken,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = build_router(context);
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        let server = axum::serve(listener, service).with_graceful_shutdown(async move {
            cancel.cancelled().await;
        });
        let handle = tokio::spawn(async move {
            let _ = server.await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (format!("http://{addr}"), handle)
    }

    // ── is_loopback ──

    #[test]
    fn test_is_loopback_ipv4() {
        assert!(is_loopback(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_loopback(IpAddr::V4(Ipv4Addr::new(127, 1, 2, 3))));
        assert!(!is_loopback(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_loopback(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(!is_loopback(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    }

    #[test]
    fn test_is_loopback_ipv6() {
        assert!(is_loopback("::1".parse::<IpAddr>().unwrap()));
        // IPv4-mapped loopback
        assert!(is_loopback("::ffff:127.0.0.1".parse::<IpAddr>().unwrap()));
        // Regular public IPv6
        assert!(!is_loopback("2606:4700:4700::1111".parse::<IpAddr>().unwrap()));
    }

    // ── extract_bearer ──

    #[test]
    fn test_extract_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(extract_bearer(&headers), Some("abc"));

        // case-insensitive scheme
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "bearer xyz".parse().unwrap(),
        );
        assert_eq!(extract_bearer(&headers), Some("xyz"));

        // missing
        let headers = HeaderMap::new();
        assert_eq!(extract_bearer(&headers), None);

        // wrong scheme
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Basic abc".parse().unwrap(),
        );
        assert_eq!(extract_bearer(&headers), None);

        // empty token
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer ".parse().unwrap(),
        );
        assert_eq!(extract_bearer(&headers), None);
    }

    // ── HTTP integration ──

    #[tokio::test]
    async fn test_health_endpoint() {
        let cancel = CancellationToken::new();
        let ctx = test_context();
        let (base, handle) = spawn_test_server(ctx, cancel.clone()).await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{base}/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: HealthResponse = resp.json().await.unwrap();
        assert_eq!(body.status, "ok");

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_get_state_endpoint() {
        let cancel = CancellationToken::new();
        let ctx = test_context();
        let (base, handle) = spawn_test_server(ctx, cancel.clone()).await;

        let client = reqwest::Client::new();
        let resp = client.get(format!("{base}/state")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let state: InspectState = resp.json().await.unwrap();
        assert_eq!(state.channels.len(), 1);
        assert_eq!(state.channels[0].name, "emf");
        assert!(!state.version.is_empty());

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_reload_config_endpoint() {
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
        });

        let cancel = CancellationToken::new();
        let (base, handle) = spawn_test_server(ctx, cancel.clone()).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/reload_config"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: ReloadResult = resp.json().await.unwrap();
        assert!(body.success, "reload should succeed: {}", body.message);
        assert!(body.message.contains("reloaded"));

        assert_eq!(config_swap.load().general.max_concurrent_threads, 5);

        // Modify and reload again
        let updated_toml =
            config_toml.replace("max_concurrent_threads = 5", "max_concurrent_threads = 10");
        std::fs::write(&config_path, updated_toml).unwrap();

        let resp = client
            .post(format!("{base}/reload_config"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: ReloadResult = resp.json().await.unwrap();
        assert!(body.success);

        assert_eq!(config_swap.load().general.max_concurrent_threads, 10);

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_reset_session_endpoint() {
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
        });

        let cancel = CancellationToken::new();
        let (base, handle) = spawn_test_server(ctx, cancel.clone()).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/reset_session"))
            .json(&serde_json::json!({ "thread_name": thread_name }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: ResetSessionResult = resp.json().await.unwrap();
        assert!(body.success, "reset should succeed: {}", body.message);
        assert!(body.message.contains("session deleted"));

        assert!(!jyc_dir.join("agent-session.json").exists());

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_reset_session_path_traversal_rejected() {
        let cancel = CancellationToken::new();
        let ctx = test_context();
        let (base, handle) = spawn_test_server(ctx, cancel.clone()).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/reset_session"))
            .json(&serde_json::json!({ "thread_name": "../../etc" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("path traversal"));

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_reset_session_no_existing_session() {
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
        });

        let cancel = CancellationToken::new();
        let (base, handle) = spawn_test_server(ctx, cancel.clone()).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/reset_session"))
            .json(&serde_json::json!({ "thread_name": "nonexistent" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: ResetSessionResult = resp.json().await.unwrap();
        assert!(body.success, "no-session case should still succeed");
        assert!(body.message.contains("no session exists"));

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_inject_message_unknown_channel() {
        let cancel = CancellationToken::new();
        let ctx = test_context();
        let (base, handle) = spawn_test_server(ctx, cancel.clone()).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/inject_message"))
            .json(&serde_json::json!({
                "channel": "nonexistent",
                "thread": "t",
                "text": "x"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("no thread manager found"));

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_inject_message_missing_field() {
        let cancel = CancellationToken::new();
        let ctx = test_context();
        let (base, handle) = spawn_test_server(ctx, cancel.clone()).await;

        let client = reqwest::Client::new();
        // Missing `text` field
        let resp = client
            .post(format!("{base}/inject_message"))
            .json(&serde_json::json!({
                "channel": "emf",
                "thread": "t"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 422);

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
        assert!(entry.text.contains("ERROR (attempt #3)"));
        assert!(entry.text.contains("server overload"));
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

    /// Regression test for cross-channel issue collision.
    ///
    /// When two channels both have a thread with the same name (e.g. `issue-20`),
    /// the activity map keyed by `(channel, thread)` resolves them independently.
    #[tokio::test]
    async fn test_activity_map_disambiguates_same_named_threads_across_channels() {
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

        let ch1 = threads.iter().find(|t| t.channel == "channel1").unwrap();
        assert!(matches!(ch1.status, ThreadStatus::Processing));
        assert_eq!(ch1.activity.len(), 1);
        assert_eq!(ch1.activity[0].text, "channel1 working");

        let ch2 = threads.iter().find(|t| t.channel == "channel2").unwrap();
        assert!(
            matches!(ch2.status, ThreadStatus::Idle),
            "channel2's issue-20 leaked channel1's processing status"
        );
        assert!(
            ch2.activity.is_empty(),
            "channel2's issue-20 leaked channel1's activity log"
        );
    }

    /// Regression test: idle threads should clear stale `is_processing`.
    #[tokio::test]
    async fn test_idle_thread_clears_stale_processing_state() {
        let activity_map: SharedActivityMap = Arc::new(Mutex::new(HashMap::new()));
        let key = ("test-channel".to_string(), "test-thread".to_string());
        {
            let mut map = activity_map.lock().await;
            let state = map.entry(key.clone()).or_default();
            state.is_processing = true;
            state.entries.push_back(ActivityEntry {
                text: "Processing started".to_string(),
                timestamp: Some(chrono::Utc::now().to_rfc3339()),
                severity: Severity::Info,
            });
        }

        {
            let map = activity_map.lock().await;
            assert!(map.get(&key).unwrap().is_processing);
        }

        {
            let mut map = activity_map.lock().await;
            if let Some(state) = map.get_mut(&key) {
                state.is_processing = false;
            }
        }

        {
            let map = activity_map.lock().await;
            assert!(!map.get(&key).unwrap().is_processing);
        }
    }

    /// Auth middleware: loopback requests bypass auth even when no token file exists.
    #[tokio::test]
    async fn test_auth_bypass_loopback_no_token() {
        // No token file exists at the data home — loopback should still get through.
        let cancel = CancellationToken::new();
        let ctx = test_context();
        let (base, handle) = spawn_test_server(ctx, cancel.clone()).await;

        let client = reqwest::Client::new();
        let resp = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(resp.status(), 200);

        cancel.cancel();
        handle.await.unwrap();
    }
}