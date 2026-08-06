use arc_swap::ArcSwap;
use axum::{
    Router,
    extract::{Path as AxPath, State as AxState, ws::WebSocketUpgrade},
    middleware::from_fn_with_state,
    response::IntoResponse,
    routing::{get, post},
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::scoped_ws::ScopedWsHandler;
use crate::thread_proxy::ThreadProxyHandler;
use jyc_core::activity_log_store::ActivityLogStore;
use jyc_core::command::list_available_models;
use jyc_core::command::{all_commands, all_commands_with};
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
    ///
    /// `scoped_thread` is the thread name bound from the URL path
    /// (`/ws/<channel>/<thread>`). Handlers that route by URL may use
    /// this to populate per-connection state without requiring the client
    /// to repeat the thread name in the payload. Pass-through handlers
    /// (e.g. `WebsocketInboundAdapter`) can ignore it.
    async fn handle(
        &self,
        ws: axum::extract::ws::WebSocket,
        addr: std::net::SocketAddr,
        scoped_thread: Option<&str>,
    ) -> anyhow::Result<()>;
}

/// Trait object alias used in `InspectContext::websocket_handlers`.
pub type DynWebsocketHandler = Arc<dyn WebsocketHandler>;

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
    pub websocket_handlers: Option<HashMap<String, DynWebsocketHandler>>,
    /// Optional reload callback — invoked after config is swapped atomically.
    pub reload_callback: Option<ReloadCallback>,
    /// Optional authorization token required by inspect clients.
    pub auth_token: Option<String>,
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

        let app = build_router(self.context.clone());
        let cancel = self.cancel.clone();

        axum::serve(listener, app)
            .with_graceful_shutdown(async move { cancel.cancelled().await })
            .await?;

        tracing::debug!("Inspect server shutting down");
        Ok(())
    }

    /// Resolve the right `WebsocketHandler` for a given route:
    /// - `WsRoute::Thread { channel, name }`:
    ///   - If `<channel>` is a websocket-type channel, use `ScopedWsHandler`
    ///     to wrap its `WebsocketInboundAdapter` and pre-send `subscribe`.
    ///   - Otherwise, construct a `ThreadProxyHandler` that routes through
    ///     the channel's `ThreadManager` and the inspect-broadcast bus.
    /// - `WsRoute::Channel(name)`: use the channel's own handler (must be a
    ///   websocket-type channel, else error).
    /// - `WsRoute::Bare`: use the first available handler; error if none.
    pub fn resolve_ws_handler(
        context: &InspectContext,
        route: WsRoute,
    ) -> anyhow::Result<DynWebsocketHandler> {
        let handlers = context.websocket_handlers.as_ref();

        match route {
            WsRoute::Thread { channel, name } => {
                // If the channel has a websocket handler, use the scoped wrapper.
                // Otherwise, fall through to the proxy (works for any channel type).
                if let Some(handlers) = handlers
                    && let Some(handler) = handlers.get(&channel)
                {
                    return Ok(Arc::new(ScopedWsHandler::new(handler.clone())));
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

    /// Build a slim overview snapshot — same shape as `build_state` but with
    /// `ThreadSummary` instead of `ThreadInfo`, dropping `activity`, `recent_messages`,
    /// and `thinking_text`. Used by the dashboard's polling loop to keep payloads small.
    pub async fn build_overview_state(context: &InspectContext) -> InspectOverview {
        let uptime = context.start_time.elapsed().as_secs();
        let (mut threads, total_threads, active_workers, max_concurrent, per_channel_workers) =
            collect_thread_snapshot(context).await;

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
        let mut summaries: Vec<ThreadSummary> = threads
            .into_iter()
            .map(|t| ThreadSummary {
                name: t.name,
                channel: t.channel,
                pattern: t.pattern,
                status: t.status,
                model: t.model,
                mode: t.mode,
                context_input_tokens: t.context_input_tokens,
                max_tokens: t.max_tokens,
                output_tokens: t.output_tokens,
                total_input_tokens: t.total_input_tokens,
                total_cache_hit_tokens: t.total_cache_hit_tokens,
                total_cache_creation_tokens: t.total_cache_creation_tokens,
                last_active_at: t.last_active_at,
                skills: t.skills,
                thread_path: t.thread_path,
                branch: t.branch,
                changed_files: t.changed_files,
                cost: t.cost,
            })
            .collect();
        // list_threads() sorts within each channel, but threads from multiple
        // channels are concatenated above — re-sort globally by (name, channel)
        // so the dashboard table and chat explorer show one alphabetical list.
        summaries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.channel.cmp(&b.channel)));

        let stats =
            compute_global_stats(context, active_workers, total_threads, max_concurrent).await;
        let channels = build_channels(context, &per_channel_workers);

        InspectOverview {
            uptime_secs: uptime,
            version: env!("CARGO_PKG_VERSION").to_string(),
            channels,
            threads: summaries,
            stats,
            commands: context
                .config
                .as_ref()
                .map(|cfg| all_commands_with(&cfg.load().commands))
                .unwrap_or_else(all_commands),
            models: context
                .config
                .as_ref()
                .map(|cfg| list_available_models(&cfg.load().agent.providers))
                .unwrap_or_default(),
        }
    }
    pub async fn build_state(context: &InspectContext) -> InspectState {
        let uptime = context.start_time.elapsed().as_secs();
        let (mut threads, total_threads, active_workers, max_concurrent, per_channel_workers) =
            collect_thread_snapshot(context).await;

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

        let stats =
            compute_global_stats(context, active_workers, total_threads, max_concurrent).await;
        let channels = build_channels(context, &per_channel_workers);

        InspectState {
            uptime_secs: uptime,
            version: env!("CARGO_PKG_VERSION").to_string(),
            channels,
            threads,
            stats,
            commands: context
                .config
                .as_ref()
                .map(|cfg| all_commands_with(&cfg.load().commands))
                .unwrap_or_else(all_commands),
            models: context
                .config
                .as_ref()
                .map(|cfg| list_available_models(&cfg.load().agent.providers))
                .unwrap_or_default(),
        }
    }
}

/// Collect threads from all thread managers plus aggregate worker statistics.
///
/// Returns `(threads, total_threads, active_workers, max_concurrent,
/// per_channel_workers)`.
async fn collect_thread_snapshot(
    context: &InspectContext,
) -> (
    Vec<ThreadInfo>,
    usize,
    usize,
    usize,
    HashMap<String, (usize, usize)>,
) {
    let mut threads = Vec::new();
    let mut total_threads = 0;
    let mut active_workers = 0;
    let mut max_concurrent = 0;
    let mut per_channel_workers: HashMap<String, (usize, usize)> = HashMap::new();

    let tms = context.thread_managers.load();
    for tm in tms.iter() {
        let tm_threads = tm.list_threads().await;
        total_threads += tm_threads.len();
        let stats = tm.get_stats().await;
        active_workers += stats.active_workers;
        max_concurrent += tm.max_concurrent();
        per_channel_workers.insert(
            tm.channel_name().to_string(),
            (stats.active_workers, tm.max_concurrent()),
        );
        threads.extend(tm_threads);
    }
    drop(tms);

    (
        threads,
        total_threads,
        active_workers,
        max_concurrent,
        per_channel_workers,
    )
}

/// Compute `GlobalStats` from health counters.
async fn compute_global_stats(
    context: &InspectContext,
    active_workers: usize,
    total_threads: usize,
    max_concurrent: usize,
) -> GlobalStats {
    let health = context.health_stats.lock().await;
    GlobalStats {
        active_workers,
        total_threads,
        max_concurrent,
        available_workers: max_concurrent.saturating_sub(active_workers),
        messages_received: health.messages_received,
        messages_processed: health.messages_processed,
        errors: health.errors,
    }
}

/// Enrich channel info with per-channel worker counts.
fn build_channels(
    context: &InspectContext,
    per_channel_workers: &HashMap<String, (usize, usize)>,
) -> Vec<ChannelInfo> {
    let channels = context.channels.load();
    let mut channels: Vec<ChannelInfo> = channels.iter().cloned().collect();
    for ch in &mut channels {
        if let Some((aw, mc)) = per_channel_workers.get(&ch.name) {
            ch.active_workers = *aw;
            ch.max_concurrent = *mc;
        }
    }
    channels
}

/// Filter activity entries by `since` timestamp (RFC 3339 string).
/// Returns entries whose timestamp is `>= since`. If `since` is None,
/// returns all entries unchanged.
pub fn filter_by_since(mut entries: Vec<ActivityEntry>, since: Option<&str>) -> Vec<ActivityEntry> {
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

/// Seed `state.next_id` from the persisted activity log so ids stay monotonic
/// across monitor restarts. If the in-memory buffer already has historical
/// entries loaded from disk, use their max id; otherwise read the last entry
/// from `.jyc/activity.jsonl`. Falls back to 1 when no history exists.
fn seed_next_id_from_disk(state: &mut ThreadActivityState, thread_path: Option<&Path>) {
    if state.next_id != 0 {
        return;
    }
    let mem_max = state.entries.iter().map(|e| e.id).max().unwrap_or(0);
    if mem_max > 0 {
        state.next_id = mem_max + 1;
        return;
    }
    if let Some(path) = thread_path
        && let Ok(last) = ActivityLogStore::load_recent(path, 1)
        && let Some(max_entry) = last.iter().max_by_key(|e| e.id)
    {
        state.next_id = max_entry.id + 1;
        return;
    }
    state.next_id = 1;
}

/// Whether an `ActivityEntry` should be shown in user-facing surfaces
/// (overview activity pane, chat activity pane, chat progress, REST API).
///
/// Filters out:
/// - New entries marked `is_internal = true` (set by `event_to_activity` for
///   `ProcessingProgress` heartbeats).
/// - Legacy entries from old log files (predecessors of the `is_internal`
///   field) that match the `ProcessingProgress` text shape.
pub fn is_user_visible_activity(entry: &ActivityEntry) -> bool {
    if entry.is_internal {
        return false;
    }
    // Backward compat: old log entries predate the field. Detect the
    // ProcessingProgress output shape: "<activity> (<N>s, <M> chars)".
    if entry.text.ends_with(" chars)") {
        return false;
    }
    true
}

/// Filter chat messages by `since` timestamp (RFC 3339 string).
pub fn filter_chat_by_since(
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

/// Broadcast a 1 Hz wall-clock elapsed tick to dashboard WS clients so
/// the chat pane, chat-mode info pane, and dashboard Details panel can
/// show a live duration indicator. The first tick fires at t=0 (see
/// `run_ticker` in the agent loop) so sub-second loops still emit one
/// event. Not persisted to `activity.jsonl` (handled upstream via
/// `is_internal`).
///
/// Payload format:
///   {"type":"loop_tick","channel":"...","thread":"...","elapsed_ms":u64}
fn publish_loop_tick_event(
    bus: &tokio::sync::broadcast::Sender<String>,
    channel: &str,
    thread: &str,
    elapsed_ms: u64,
) {
    let payload = serde_json::json!({
        "type": "loop_tick",
        "channel": channel,
        "thread": thread,
        "elapsed_ms": elapsed_ms,
    });
    let _ = bus.send(payload.to_string());
}
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
                                                                        // Internal events (ProcessingProgress heartbeats) are
                                                                        // debug-only: skip persisting to disk and skip the
                                                                        // in-memory log + WS broadcast so they don't flood the
                                                                        // activity pane / chat progress.
                                                                        let is_internal = entry.is_internal;
                                                                        let mut map = map.lock().await;
                                                                        let state = map
                                                                            .entry((channel_for_task.clone(), name.clone()))
                                                                            .or_default();
                                                                        seed_next_id_from_disk(state, thread_path.as_deref());
                                                                        if !is_internal {
                                                                            // Assign monotonic per-thread id BEFORE persisting to
                                                                            // disk and pushing to the in-memory buffer, so the log
                                                                            // carries the same ids the dashboard uses for dedup.
                                                                            entry.id = state.next_id;
                                                                            state.next_id = state.next_id.wrapping_add(1);
                                                                            if let Some(ref path) = thread_path
                                                                                && let Err(e) = ActivityLogStore::append(path, &entry)
                                                                            {
                                                                                tracing::warn!(error = %e, thread = %name, "Failed to persist activity entry");
                                                                            }
                                                                            state.entries.push_back(entry.clone());
                                                                            if state.entries.len() > MAX_ACTIVITY_ENTRIES {
                                                                                state.entries.pop_front();
                                                                            }
                                                                            // Fan out to the inspect-broadcast bus so dashboard
                                                                            // WebSocket clients receive live events.
                                                                            publish_activity_event(
                                                                                &inspect_broadcast_for_task,
                                                                                &channel_for_task,
                                                                                &name,
                                                                                &entry,
                                                                            );
                                                                        }

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
                                                                        // Publish processing-status AFTER the state
                                                                        // update so that ProcessingCompleted sends
                                                                        // is_processing=false, not the stale true value.
                                                                        if matches!(
                                                                            &event,
                                                                            ThreadEvent::ProcessingStarted { .. }
                                                                            | ThreadEvent::ProcessingCompleted { .. }
                                                                        ) {
                                                                            publish_processing_event(
                                                                                &inspect_broadcast_for_task,
                                                                                &channel_for_task,
                                                                                &name,
                                                                                state.is_processing,
                                                                                state.has_error,
                                                                            );
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

                                                                    // Live-duration ticker: LoopTick is `is_internal` so it
                                                                    // was skipped above (no activity.jsonl write, no
                                                                    // activity-pane entry), but we still want to fan it
                                                                    // out over WS so the dashboard can render a live
                                                                    // "12.4s" indicator. LoopTick fires at 1 Hz (with
                                                                    // the first tick at t=0); the elapsed_ms value
                                                                    // on the variant is what we forward.
                                                                    if let ThreadEvent::LoopTick { elapsed_ms, .. } = &event {
                                                                        publish_loop_tick_event(
                                                                            &inspect_broadcast_for_task,
                                                                            &channel_for_task,
                                                                            &name,
                                                                            *elapsed_ms,
                                                                        );
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

/// Whether a `ThreadEvent` should be marked internal (filtered from
/// user-facing surfaces like the activity pane and REST API).
fn is_event_internal(event: &ThreadEvent) -> bool {
    // ProcessingProgress heartbeats are emitted frequently during long
    // tool runs to indicate the agent is still working. They're useful
    // for debug logs but noisy in the UI - filter them.
    //
    // LoopTick is a 1 Hz wall-clock heartbeat that drives the dashboard's
    // live-duration ticker (with the first tick fired at t=0). Same
    // reasoning as ProcessingProgress: useful for the WS ticker
    // payload, not for the activity pane.
    matches!(
        event,
        ThreadEvent::ProcessingProgress { .. } | ThreadEvent::LoopTick { .. }
    )
}

/// Convert a ThreadEvent into a human-readable ActivityEntry.
fn event_to_activity(event: &ThreadEvent) -> ActivityEntry {
    let severity = match event {
        ThreadEvent::SessionStatus { status_type, .. } => match status_type.as_str() {
            "error" | "timeout" => Severity::Error,
            "retry" | "rate_limit" | "no_reply" => Severity::Warning,
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
                "no_reply" => "NO REPLY",
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
        // LoopTick is `is_internal` so this match arm should never run in
        // practice, but the match must be exhaustive. Format it as a
        // short debug string so a future regression doesn't produce a
        // confusing fall-through.
        ThreadEvent::LoopTick { elapsed_ms, .. } => {
            format!("LoopTick ({elapsed_ms}ms)")
        }
    };
    ActivityEntry {
        text,
        timestamp: Some(event.timestamp().to_rfc3339()),
        severity,
        id: 0, // assigned by ActivityTracker on push (see fanout step)
        is_internal: is_event_internal(event),
    }
}

/// Build the axum `Router` for the inspect server.
///
/// All routes share the same `Authorization: Bearer <token>` middleware
/// (`auth::require_bearer`). WebSocket upgrades flow through the same
/// auth gate.
///
/// Exception: `/exchange/*` is mounted WITHOUT bearer auth — access control
/// there is the per-thread `?token=` created by the `jyc_publish_file`
/// tool (rotated by `/reset`), so share links work for end users.
pub fn build_router(context: Arc<InspectContext>) -> Router {
    use crate::api;

    let authed = Router::new()
        .route("/api/state", get(api::get_state))
        .route("/api/state/overview", get(api::get_state_overview))
        .route(
            "/api/threads/:channel/:thread/activity",
            get(api::get_thread_activity),
        )
        .route(
            "/api/threads/:channel/:thread/chat",
            get(api::get_thread_chat),
        )
        .route("/api/channels/:channel/patterns", get(api::get_patterns))
        .route("/api/threads", post(api::post_thread))
        .route("/api/config/reload", post(api::post_reload_config))
        .route("/ws", get(ws_bare))
        .route("/ws/:channel", get(ws_channel))
        .route("/ws/:channel/:thread", get(ws_thread))
        .layer(from_fn_with_state(
            context.clone(),
            crate::auth::require_bearer,
        ));

    Router::new()
        .route(
            "/exchange/:channel/:thread/*file_path",
            get(api::get_exchange_file),
        )
        .merge(authed)
        .with_state(context)
}

/// WS upgrade for `GET /ws` — use the first registered handler.
async fn ws_bare(
    AxState(ctx): AxState<Arc<InspectContext>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws_upgrade_for_route(ws, ctx, WsRoute::Bare).await
}

/// WS upgrade for `GET /ws/<channel>` — adhoc thread on a websocket channel.
async fn ws_channel(
    AxState(ctx): AxState<Arc<InspectContext>>,
    ws: WebSocketUpgrade,
    AxPath(channel): AxPath<String>,
) -> impl IntoResponse {
    ws_upgrade_for_route(ws, ctx, WsRoute::Channel(channel)).await
}

/// WS upgrade for `GET /ws/<channel>/<thread>` — proxy to a thread.
async fn ws_thread(
    AxState(ctx): AxState<Arc<InspectContext>>,
    ws: WebSocketUpgrade,
    AxPath((channel, name)): AxPath<(String, String)>,
) -> impl IntoResponse {
    ws_upgrade_for_route(ws, ctx, WsRoute::Thread { channel, name }).await
}

async fn ws_upgrade_for_route(
    ws: WebSocketUpgrade,
    ctx: Arc<InspectContext>,
    route: WsRoute,
) -> axum::response::Response {
    use axum::extract::ws::Message;
    // Extract the URL-scoped thread name (`/ws/<channel>/<thread>`) before
    // `resolve_ws_handler` consumes `route`. Handlers that bind the thread
    // from the URL (e.g. `WebsocketInboundAdapter` wrapped in
    // `ScopedWsHandler`) rely on this to route inbound chat messages
    // without requiring a `thread` field in the payload.
    let scoped_thread: Option<String> = match &route {
        WsRoute::Thread { name, .. } => Some(name.clone()),
        _ => None,
    };
    match InspectServer::resolve_ws_handler(&ctx, route) {
        Ok(handler) => {
            let addr = std::net::SocketAddr::from(([0, 0, 0, 0], 0));
            ws.on_upgrade(move |socket| async move {
                if let Err(e) = handler.handle(socket, addr, scoped_thread.as_deref()).await {
                    tracing::debug!(error = %e, "ws handler exited with error");
                }
            })
        }
        Err(e) => {
            tracing::debug!(error = %e, "ws route resolution failed");
            ws.on_upgrade(|mut socket| async move {
                let _ = socket.send(Message::Close(None)).await;
            })
        }
    }
}

#[cfg(test)]
mod no_reply_rendering_tests {
    use super::*;
    use chrono::Utc;

    fn no_reply_event() -> ThreadEvent {
        ThreadEvent::SessionStatus {
            thread_name: "t1".to_string(),
            status_type: "no_reply".to_string(),
            attempt: None,
            message: Some("AI produced no text and no tool call".to_string()),
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn no_reply_renders_as_warning_no_reply() {
        let entry = event_to_activity(&no_reply_event());
        assert_eq!(entry.severity, Severity::Warning);
        assert!(
            entry.text.starts_with("NO REPLY"),
            "expected 'NO REPLY' label, got {:?}",
            entry.text
        );
        assert!(
            entry.text.contains("AI produced no text and no tool call"),
            "expected message body in entry text, got {:?}",
            entry.text
        );
        assert!(!entry.is_internal, "no-reply must be user-visible");
    }

    #[test]
    fn other_session_statuses_unchanged() {
        let retry = ThreadEvent::SessionStatus {
            thread_name: "t1".to_string(),
            status_type: "retry".to_string(),
            attempt: Some(2),
            message: None,
            timestamp: Utc::now(),
        };
        let entry = event_to_activity(&retry);
        assert_eq!(entry.severity, Severity::Warning);
        assert!(entry.text.starts_with("RETRY"), "got {:?}", entry.text);
    }
}

#[cfg(test)]
mod next_id_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn seed_next_id_from_empty_state_starts_at_one() {
        let mut state = ThreadActivityState::default();
        seed_next_id_from_disk(&mut state, None);
        assert_eq!(state.next_id, 1);
    }

    #[test]
    fn seed_next_id_uses_in_memory_max_id() {
        let mut state = ThreadActivityState::default();
        state.entries.push_back(ActivityEntry {
            id: 42,
            text: "old".to_string(),
            timestamp: None,
            severity: Severity::Info,
            is_internal: false,
        });
        seed_next_id_from_disk(&mut state, None);
        assert_eq!(state.next_id, 43);
    }

    #[test]
    fn seed_next_id_falls_back_to_disk_when_memory_is_empty() {
        let tmp = TempDir::new().unwrap();
        let thread_path = tmp.path().to_path_buf();
        let jyc_dir = thread_path.join(".jyc");
        fs::create_dir_all(&jyc_dir).unwrap();
        let entry = ActivityEntry {
            id: 7,
            text: "disk".to_string(),
            timestamp: None,
            severity: Severity::Info,
            is_internal: false,
        };
        fs::write(
            jyc_dir.join("activity.jsonl"),
            serde_json::to_string(&entry).unwrap(),
        )
        .unwrap();
        let mut state = ThreadActivityState::default();
        seed_next_id_from_disk(&mut state, Some(&thread_path));
        assert_eq!(state.next_id, 8);
    }

    #[test]
    fn seed_next_id_noop_once_initialized() {
        let mut state = ThreadActivityState {
            next_id: 99,
            ..Default::default()
        };
        seed_next_id_from_disk(&mut state, None);
        assert_eq!(state.next_id, 99);
    }
}

#[cfg(test)]
mod exchange_route_auth_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use jyc_core::metrics::HealthStats;
    use tower::ServiceExt;

    fn ctx_with_token(token: Option<&str>) -> Arc<InspectContext> {
        Arc::new(InspectContext {
            thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
            channels: Arc::new(ArcSwap::from_pointee(vec![])),
            health_stats: Arc::new(Mutex::new(HealthStats::default())),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            start_time: Instant::now(),
            config_path: None,
            global_config_path: None,
            config: None,
            workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![])),
            websocket_handlers: None,
            reload_callback: None,
            auth_token: token.map(String::from),
            inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(1).0),
        })
    }

    /// `/exchange/*` must NOT be gated by the bearer middleware — access
    /// control is the per-thread `?token=`. With no thread manager the
    /// handler 404s; the point is that it is not a 401.
    #[tokio::test]
    async fn public_route_bypasses_bearer_middleware() {
        let app = build_router(ctx_with_token(Some("secret")));
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/exchange/ch/th/f.txt?token=x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    /// Sanity: authed routes still reject requests without the bearer token.
    #[tokio::test]
    async fn api_route_still_requires_bearer() {
        let app = build_router(ctx_with_token(Some("secret")));
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
