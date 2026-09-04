use arc_swap::ArcSwap;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::scoped_ws::ScopedWsHandler;
use crate::topic_proxy::TopicProxyHandler;
use jyc_core::activity_log_store::ActivityLogStore;
use jyc_core::command::list_available_models;
use jyc_core::command::{all_commands, all_commands_with};
use jyc_core::metrics::SharedHealthStats;
use jyc_core::topic_manager::TopicManager;
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
    /// `scoped_topic` is the topic name bound from the URL path
    /// (`/ws/<channel>/<topic>`). Handlers that route by URL may use
    /// this to populate per-connection state without requiring the client
    /// to repeat the topic name in the payload. Pass-through handlers
    /// (e.g. `WebsocketInboundAdapter`) can ignore it.
    async fn handle(
        &self,
        ws: axum::extract::ws::WebSocket,
        addr: std::net::SocketAddr,
        scoped_topic: Option<&str>,
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
    /// `GET /ws/<channel>` — adhoc topic on a websocket-type channel.
    Channel(String),
    /// `GET /ws/<channel>/<topic>` — proxy to a specific topic. For
    /// websocket-type channels, the inner handler is wrapped in
    /// `ScopedWsHandler` to auto-subscribe. For other channels, a
    /// `TopicProxyHandler` is used.
    Topic { channel: String, name: String },
}

/// Max activity entries kept per topic.
pub(crate) const MAX_ACTIVITY_ENTRIES: usize = 180;

/// Max recent chat messages kept per topic for live dashboard display.
pub(crate) const MAX_RECENT_MESSAGES: usize = 50;

/// Per-topic activity buffer, shared between the activity tracker and the server.
///
/// Key is `(channel_name, topic_name)` so that two channels with same-named
/// topics (e.g. both have `issue-20`) do not collide.
pub type SharedActivityMap = Arc<Mutex<HashMap<(String, String), TopicActivityState>>>;

/// Per-topic activity state: bounded event log + processing flag.
#[derive(Debug, Default)]
pub struct TopicActivityState {
    pub entries: VecDeque<ActivityEntry>,
    pub is_processing: bool,
    pub has_error: bool,
    pub last_active_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Recent chat messages (incoming + replies) for live dashboard display.
    pub recent_messages: VecDeque<ChatMessageEntry>,
    /// Latest AI thinking/reasoning text (full, untruncated).
    pub thinking_text: Option<String>,
    /// Monotonic per-topic counter used to assign unique `id` to each entry
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
    /// Per-channel topic managers (dynamic — updated on reload)
    pub topic_managers: Arc<ArcSwap<Vec<Arc<TopicManager>>>>,
    /// Channel info (name, type) (dynamic — updated on reload)
    pub channels: Arc<ArcSwap<Vec<ChannelInfo>>>,
    /// Shared health stats from MetricsCollector
    pub health_stats: SharedHealthStats,
    /// Per-topic activity logs from SSE events
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
    /// `TopicProxyHandler` to forward activity/chat/thinking events to
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
    /// - `WsRoute::Topic { channel, name }`:
    ///   - If `<channel>` is a websocket-type channel, use `ScopedWsHandler`
    ///     to wrap its `WebsocketInboundAdapter` and pre-send `subscribe`.
    ///   - Otherwise, construct a `TopicProxyHandler` that routes through
    ///     the channel's `TopicManager` and the inspect-broadcast bus.
    /// - `WsRoute::Channel(name)`: use the channel's own handler (must be a
    ///   websocket-type channel, else error).
    /// - `WsRoute::Bare`: use the first available handler; error if none.
    pub fn resolve_ws_handler(
        context: &InspectContext,
        route: WsRoute,
    ) -> anyhow::Result<DynWebsocketHandler> {
        let handlers = context.websocket_handlers.as_ref();

        match route {
            WsRoute::Topic { channel, name } => {
                // If the channel has a websocket handler, use the scoped wrapper.
                // Otherwise, fall through to the proxy (works for any channel type).
                if let Some(handlers) = handlers
                    && let Some(handler) = handlers.get(&channel)
                {
                    return Ok(Arc::new(ScopedWsHandler::new(handler.clone())));
                }
                Ok(Arc::new(TopicProxyHandler::new(
                    channel,
                    name,
                    context.topic_managers.clone(),
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
    /// `TopicSummary` instead of `TopicInfo`, dropping `activity`, `recent_messages`,
    /// and `thinking_text`. Used by the dashboard's polling loop to keep payloads small.
    pub async fn build_overview_state(context: &InspectContext) -> InspectOverview {
        let uptime = context.start_time.elapsed().as_secs();
        let (mut topics, total_topics, active_workers, max_concurrent, per_channel_workers) =
            collect_topic_snapshot(context).await;

        // Override status from activity_map (Processing / Error flags) but skip
        // copying activity/messages/thinking — that's the whole point.
        let activity_map = context.activity_map.lock().await;
        for topic in &mut topics {
            let key = (topic.channel.clone(), topic.name.clone());
            if let Some(state) = activity_map.get(&key) {
                if state.is_processing {
                    topic.status = TopicStatus::Processing;
                } else if state.has_error {
                    topic.status = TopicStatus::Error;
                }
                if let Some(last_active) = state.last_active_at {
                    topic.last_active_at = Some(last_active.to_rfc3339());
                }
            }
        }
        drop(activity_map);

        // Slim each TopicInfo down to a TopicSummary.
        let mut summaries: Vec<TopicSummary> = topics
            .into_iter()
            .map(|t| TopicSummary {
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
                topic_path: t.topic_path,
                branch: t.branch,
                changed_files: t.changed_files,
                cost: t.cost,
            })
            .collect();
        // list_topics() sorts within each channel, but topics from multiple
        // channels are concatenated above — re-sort globally by (name, channel)
        // so the dashboard table and chat explorer show one alphabetical list.
        summaries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.channel.cmp(&b.channel)));

        let stats =
            compute_global_stats(context, active_workers, total_topics, max_concurrent).await;
        let channels = build_channels(context, &per_channel_workers);

        InspectOverview {
            uptime_secs: uptime,
            version: env!("CARGO_PKG_VERSION").to_string(),
            channels,
            topics: summaries,
            stats,
            commands: context
                .config
                .as_ref()
                .map(|cfg| all_commands_with(&cfg.load().commands))
                .unwrap_or_else(all_commands),
            models: context
                .config
                .as_ref()
                .map(|cfg| list_available_models(&cfg.load().ai.providers))
                .unwrap_or_default(),
        }
    }
    pub async fn build_state(context: &InspectContext) -> InspectState {
        let uptime = context.start_time.elapsed().as_secs();
        let (mut topics, total_topics, active_workers, max_concurrent, per_channel_workers) =
            collect_topic_snapshot(context).await;

        // Merge activity logs and status into topics
        let activity_map = context.activity_map.lock().await;
        for topic in &mut topics {
            let key = (topic.channel.clone(), topic.name.clone());
            if let Some(state) = activity_map.get(&key) {
                topic.activity = state.entries.iter().cloned().collect();
                topic.recent_messages = state.recent_messages.iter().cloned().collect();
                topic.thinking_text = state.thinking_text.clone();
                if state.is_processing {
                    topic.status = TopicStatus::Processing;
                } else if state.has_error {
                    topic.status = TopicStatus::Error;
                }
                if let Some(last_active) = state.last_active_at {
                    topic.last_active_at = Some(last_active.to_rfc3339());
                }
            }
        }
        drop(activity_map);

        let stats =
            compute_global_stats(context, active_workers, total_topics, max_concurrent).await;
        let channels = build_channels(context, &per_channel_workers);

        InspectState {
            uptime_secs: uptime,
            version: env!("CARGO_PKG_VERSION").to_string(),
            channels,
            topics,
            stats,
            commands: context
                .config
                .as_ref()
                .map(|cfg| all_commands_with(&cfg.load().commands))
                .unwrap_or_else(all_commands),
            models: context
                .config
                .as_ref()
                .map(|cfg| list_available_models(&cfg.load().ai.providers))
                .unwrap_or_default(),
        }
    }
}

/// Collect topics from all topic managers plus aggregate worker statistics.
///
/// Returns `(topics, total_topics, active_workers, max_concurrent,
/// per_channel_workers)`.
pub(crate) async fn collect_topic_snapshot(
    context: &InspectContext,
) -> (
    Vec<TopicInfo>,
    usize,
    usize,
    usize,
    HashMap<String, (usize, usize)>,
) {
    let mut topics = Vec::new();
    let mut total_topics = 0;
    let mut active_workers = 0;
    let mut max_concurrent = 0;
    let mut per_channel_workers: HashMap<String, (usize, usize)> = HashMap::new();

    let tms = context.topic_managers.load();
    for tm in tms.iter() {
        let tm_topics = tm.list_topics().await;
        total_topics += tm_topics.len();
        let stats = tm.get_stats().await;
        active_workers += stats.active_workers;
        max_concurrent += tm.max_concurrent();
        per_channel_workers.insert(
            tm.channel_name().to_string(),
            (stats.active_workers, tm.max_concurrent()),
        );
        topics.extend(tm_topics);
    }
    drop(tms);

    (
        topics,
        total_topics,
        active_workers,
        max_concurrent,
        per_channel_workers,
    )
}

/// Compute `GlobalStats` from health counters.
pub(crate) async fn compute_global_stats(
    context: &InspectContext,
    active_workers: usize,
    total_topics: usize,
    max_concurrent: usize,
) -> GlobalStats {
    let health = context.health_stats.lock().await;
    GlobalStats {
        active_workers,
        total_topics,
        max_concurrent,
        available_workers: max_concurrent.saturating_sub(active_workers),
        messages_received: health.messages_received,
        messages_processed: health.messages_processed,
        errors: health.errors,
    }
}

/// Enrich channel info with per-channel worker counts.
pub(crate) fn build_channels(
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
pub(crate) fn seed_next_id_from_disk(state: &mut TopicActivityState, topic_path: Option<&Path>) {
    if state.next_id != 0 {
        return;
    }
    let mem_max = state.entries.iter().map(|e| e.id).max().unwrap_or(0);
    if mem_max > 0 {
        state.next_id = mem_max + 1;
        return;
    }
    if let Some(path) = topic_path
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
#[cfg(test)]
mod no_reply_rendering_tests {
    use super::*;
    use chrono::Utc;
    use jyc_core::topic_event::TopicEvent;

    fn no_reply_event() -> TopicEvent {
        TopicEvent::SessionStatus {
            topic_name: "t1".to_string(),
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
        let retry = TopicEvent::SessionStatus {
            topic_name: "t1".to_string(),
            status_type: "retry".to_string(),
            attempt: Some(2),
            message: None,
            timestamp: Utc::now(),
        };
        let entry = event_to_activity(&retry);
        assert_eq!(entry.severity, Severity::Warning);
        assert!(entry.text.starts_with("RETRY"), "got {:?}", entry.text);
    }

    #[test]
    fn reply_tool_missing_renders_as_warning() {
        let event = TopicEvent::SessionStatus {
            topic_name: "t1".to_string(),
            status_type: "reply_tool_missing".to_string(),
            attempt: None,
            message: Some("reminded once to deliver via jyc_reply_message".to_string()),
            timestamp: Utc::now(),
        };
        let entry = event_to_activity(&event);
        assert_eq!(entry.severity, Severity::Warning);
        assert!(
            entry.text.starts_with("REPLY TOOL MISSING"),
            "expected 'REPLY TOOL MISSING' label, got {:?}",
            entry.text
        );
        assert!(
            entry
                .text
                .contains("reminded once to deliver via jyc_reply_message"),
            "expected message body in entry text, got {:?}",
            entry.text
        );
        assert!(
            !entry.is_internal,
            "reply-tool-missing must be user-visible"
        );
    }
}

#[cfg(test)]
mod next_id_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn seed_next_id_from_empty_state_starts_at_one() {
        let mut state = TopicActivityState::default();
        seed_next_id_from_disk(&mut state, None);
        assert_eq!(state.next_id, 1);
    }

    #[test]
    fn seed_next_id_uses_in_memory_max_id() {
        let mut state = TopicActivityState::default();
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
        let topic_path = tmp.path().to_path_buf();
        let jyc_dir = topic_path.join(".jyc");
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
        let mut state = TopicActivityState::default();
        seed_next_id_from_disk(&mut state, Some(&topic_path));
        assert_eq!(state.next_id, 8);
    }

    #[test]
    fn seed_next_id_noop_once_initialized() {
        let mut state = TopicActivityState {
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
            topic_managers: Arc::new(ArcSwap::from_pointee(vec![])),
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
    /// control is the per-topic `?token=`. With no topic manager the
    /// handler 403s; the point is that it is not a 401.
    #[tokio::test]
    async fn exchange_route_bypasses_bearer_middleware() {
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
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
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

mod activity;
mod routes;

pub use activity::ActivityTracker;
pub(crate) use activity::*;
pub use routes::build_router;
