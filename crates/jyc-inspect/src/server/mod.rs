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
use jyc_core::command::spawn_handler::spawned_from;
use jyc_core::command::{ArgCtx, all_commands_with, command_args, per_agent_commands};
use jyc_core::metrics::SharedHealthStats;
use jyc_core::topic_manager::TopicManagers;
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

/// Per-topic activity state: bounded event log + the live buffers.
#[derive(Debug, Default)]
pub struct TopicActivityState {
    pub entries: VecDeque<ActivityEntry>,
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
    pub topic_managers: Arc<TopicManagers>,
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
    /// Inspect-server shutdown signal. Cloned into each WS handler so its
    /// `select!` loop can detect shutdown and send a Close frame, allowing
    /// axum's `with_graceful_shutdown` to complete instead of waiting
    /// forever on still-open WS connections.
    pub ws_shutdown: CancellationToken,
    /// The agents dir this instance creates topic homes under
    /// (`resolve_agents_dir(data_dir)` at startup) — the root a topic's home,
    /// and the state dir inside it, sit under.
    ///
    /// Read to name a topic's owning agent by stripping this prefix off that
    /// home. It has to be the instance's own dir rather than a process-wide
    /// default: where the platform data home is unavailable, homes still land
    /// in `<data dir>/agents`, and every topic would otherwise read as
    /// agentless.
    pub agents_dir: PathBuf,
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
                    context.ws_shutdown.clone(),
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
    /// and `thinking_text`, and skipping the per-topic `git` probe (so `branch`
    /// and `changed_files` stay `None`). Used by the dashboard's polling loop to keep
    /// payloads small and the round trip cheap; the topic on screen gets those two
    /// fields from the per-topic info endpoint.
    pub async fn build_overview_state(context: &InspectContext) -> InspectOverview {
        let uptime = context.start_time.elapsed().as_secs();
        let (mut topics, total_topics, active_workers, max_concurrent, per_channel_workers) =
            collect_topic_snapshot(context, false).await;

        // The status comes from the topic's own snapshot — the runtime's live
        // status, the one source of truth for it (the same value `/info` and
        // the info pane report). Only the last-activity stamp is refreshed
        // here, from the event stream; the activity/messages/thinking buffers
        // stay out of the list — that's the whole point.
        let activity_map = context.activity_map.lock().await;
        for topic in &mut topics {
            let key = (topic.channel.clone(), topic.name.clone());
            if let Some(state) = activity_map.get(&key)
                && let Some(last_active) = state.last_active_at
            {
                topic.last_active_at = Some(last_active.to_rfc3339());
            }
        }
        drop(activity_map);

        // Slim each TopicInfo down to a TopicSummary, attaching per-topic
        // commands so the dashboard `/` popup reflects what actually
        // dispatches in that topic (built-ins + globals + per-agent for
        // the topic's pattern; per-agent wins on collision).
        let cfg = context.config.as_ref().map(|c| c.load_full());
        let fallback_cfg = AppConfig::default();
        let cfg_ref = cfg.as_deref().unwrap_or(&fallback_cfg);
        let models = list_available_models(&cfg_ref.ai.providers);
        let subagent_types = jyc_core::command::spawn_handler::type_catalog_pairs(
            cfg_ref,
            jyc_utils::paths::config_dir().as_deref(),
        );
        let agents = agents_of(&topics);
        // The agents dir carries each topic's owning agent in its home's first
        // path segment. The instance's own dir, as resolved from its data dir
        // at startup: a process-wide default would name no agent at all where
        // the platform data home is unavailable, and read a home outside it as
        // agentless.
        let agents_dir = context.agents_dir.clone();
        let mut summaries: Vec<TopicSummary> = topics
            .into_iter()
            .map(|t| {
                let commands = commands_for_topic(
                    t.pattern.as_deref(),
                    cfg_ref,
                    &t.available_skills,
                    &models,
                    &subagent_types,
                    &t.name,
                    &agents,
                );
                // Lineage for the tree views: read before the struct literal
                // moves `t.name`. A topic whose state dir can't be read (or
                // has no breadcrumb) is a root. `spawned_from` looks the state
                // dir up by name, and that suffices here: a listed topic got
                // its path from `set_topic_path`, which adopts a state dir, so
                // the `topic_path` fallback cannot fire. (Registration is
                // name-keyed and global, so two same-named topics on different
                // channels share one dir and read the same breadcrumb.)
                let parent = t
                    .topic_path
                    .as_deref()
                    .and_then(|path| spawned_from(&t.name, path));
                // The agent from the home the state dir sits in — by name
                // rather than by `topic_path`, which a pinned topic points
                // outside the agents dir.
                let agent = agent_of_topic(&t.name, &agents_dir);
                TopicSummary {
                    name: t.name,
                    channel: t.channel,
                    agent,
                    pattern: t.pattern.clone(),
                    status: t.status,
                    context_input_tokens: t.context_input_tokens,
                    max_tokens: t.max_tokens,
                    last_active_at: t.last_active_at,
                    topic_path: t.topic_path,
                    parent,
                    commands,
                }
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
            models,
        }
    }
    pub async fn build_state(context: &InspectContext) -> InspectState {
        let uptime = context.start_time.elapsed().as_secs();
        let (mut topics, total_topics, active_workers, max_concurrent, per_channel_workers) =
            collect_topic_snapshot(context, true).await;

        // Merge the activity logs into topics. The status is not merged: it
        // comes from the topic's own snapshot — the runtime's live status, the
        // one source of truth for it.
        let activity_map = context.activity_map.lock().await;
        for topic in &mut topics {
            let key = (topic.channel.clone(), topic.name.clone());
            if let Some(state) = activity_map.get(&key) {
                topic.activity = state.entries.iter().cloned().collect();
                topic.recent_messages = state.recent_messages.iter().cloned().collect();
                topic.thinking_text = state.thinking_text.clone();
                if let Some(last_active) = state.last_active_at {
                    topic.last_active_at = Some(last_active.to_rfc3339());
                }
            }
        }
        drop(activity_map);

        let stats =
            compute_global_stats(context, active_workers, total_topics, max_concurrent).await;
        let channels = build_channels(context, &per_channel_workers);

        // Attach per-topic commands so the dashboard `/` popup reflects
        // what actually dispatches in this topic (built-ins + globals +
        // per-agent for the topic's pattern; per-agent wins on collision).
        // The commands also carry their enumerable argument values, which
        // drive the popup's nested levels (`/model <id>`, `/skill on <name>`).
        // TODO(perf): `commands_for_topic` recomputes per topic on every
        // ~500 ms overview poll, but the dashboard TUI only reads commands
        // for its current chat topic. To stop the per-poll work: either
        // memoize here keyed by (`cfg_ptr`, pattern) and clear on
        // `AppConfig` reload, or drop the per-topic `commands` from the
        // overview and expose `GET /api/commands?pattern=X` for the TUI
        // to fetch lazily. `/close`'s per-topic subagent list widens it: every
        // topic's list reads its children's `spawned-from` breadcrumbs from
        // disk on that same poll.
        let cfg = context.config.as_ref().map(|c| c.load_full());
        let fallback_cfg = AppConfig::default();
        let cfg_ref = cfg.as_deref().unwrap_or(&fallback_cfg);
        let models = list_available_models(&cfg_ref.ai.providers);
        let subagent_types = jyc_core::command::spawn_handler::type_catalog_pairs(
            cfg_ref,
            jyc_utils::paths::config_dir().as_deref(),
        );
        let agents = agents_of(&topics);
        let topics: Vec<TopicInfo> = topics
            .into_iter()
            .map(|mut t| {
                t.commands = commands_for_topic(
                    t.pattern.as_deref(),
                    cfg_ref,
                    &t.available_skills,
                    &models,
                    &subagent_types,
                    &t.name,
                    &agents,
                );
                t
            })
            .collect();

        InspectState {
            uptime_secs: uptime,
            version: env!("CARGO_PKG_VERSION").to_string(),
            channels,
            topics,
            stats,
            models,
        }
    }
}

/// Collect topics from all topic managers plus aggregate worker statistics.
///
/// `include_git` gates the per-topic `git` probe behind `branch` and
/// `changed_files`: the overview poll turns it off, every other caller leaves
/// it on.
///
/// Returns `(topics, total_topics, active_workers, max_concurrent,
/// per_channel_workers)`.
pub(crate) async fn collect_topic_snapshot(
    context: &InspectContext,
    include_git: bool,
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

    let tms = context.topic_managers.all();
    for tm in tms.iter() {
        let tm_topics = if include_git {
            tm.list_topics().await
        } else {
            tm.list_topics_without_git().await
        };
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

/// The agent a listed topic belongs to, read off the state dir registered
/// under its name (`<home>/.jyc`, hence `home` is its parent).
///
/// By name, never by the topic's `topic_path`: a pinned topic works in a
/// directory of the user's choosing, outside the agents dir, while its state
/// — and with it the agent — stays under the agents dir where the home is.
/// `None` for a topic with no registration, which the renderers show as a dash.
fn agent_of_topic(name: &str, agents_dir: &Path) -> Option<String> {
    let home = jyc_types::state_dir::registered_state(name)?
        .parent()?
        .to_path_buf();
    jyc_core::topic_path::agent_of_home(&home, agents_dir)
}

/// The agents `/send` can address, by name: the active topics of the agents
/// channel, read off a snapshot the caller already holds — the payload builder
/// lists every manager's topics in one pass, and `TopicInfo.channel` is the
/// manager's own channel name, so this is the same list `list_topics` would
/// give without walking the workspace again.
///
/// Both payload builders go through this so the two lists cannot drift.
fn agents_of(topics: &[TopicInfo]) -> Vec<String> {
    topics
        .iter()
        .filter(|t| t.channel == jyc_core::command::send_handler::AGENTS_CHANNEL)
        .map(|t| t.name.clone())
        .collect()
}

/// Build the dashboard-visible command list for one topic. Composed
/// of built-ins + globals + `[[agents.<pattern>.commands]]` for the
/// topic's pattern; per-agent wins on collision, matching
/// `CommandRegistry::register`.
///
/// Per-agent lookup is the shared
/// `jyc_core::command::per_agent_commands` helper — the same call the
/// worker makes — so the popup and `/?` always agree.
///
/// Each command also gets its enumerable argument values (`CommandInfo::args`)
/// from `jyc_core::command::command_args`, which is what lets the TUI popup
/// offer `/model <id>` or `/skill on <name>` without knowing any commands.
///
/// `available` is the topic's live skill list
/// ([`jyc_core::agent::AgentService::available_skills`], via
/// `TopicInfo::available_skills`): it both adds the `/skill:<name>` rows and
/// feeds the `/skill on|off` argument values — deliberately the *available*
/// names rather than the enabled snapshot in `TopicInfo::skills`, so a skill
/// the user toggled off can still be completed back on.
///
/// `subagent_types` is `/spawn`'s first level, resolved by the caller with
/// `spawn_handler::type_catalog_pairs` — once per payload rather than once per
/// topic, since it reads the installed role files and the payload carries a
/// command list for every topic.
///
/// `agents` is `/send`'s first level (the same for every topic), and `name` is
/// the topic this list belongs to — `/close`'s subagent list is read from it.
fn commands_for_topic(
    pattern: Option<&str>,
    cfg: &AppConfig,
    available: &[jyc_types::SkillMeta],
    models: &[ModelInfo],
    subagent_types: &[(String, String)],
    name: &str,
    agents: &[String],
) -> Vec<CommandInfo> {
    let per_agent = pattern
        .map(|p| per_agent_commands(cfg, p))
        .unwrap_or_default();
    let skill_names: Vec<String> = available.iter().map(|s| s.name.clone()).collect();
    let subagents: Vec<String> = jyc_core::command::spawn_handler::subagents_of(name)
        .into_iter()
        .map(|(child, _)| child)
        .collect();
    let arg_ctx = ArgCtx {
        config: cfg,
        skills: &skill_names,
        models,
        subagent_types,
        agents,
        subagents: &subagents,
    };
    let mut commands = all_commands_with(&cfg.commands, &per_agent, available);
    for cmd in &mut commands {
        cmd.args = command_args(&cmd.name, &arg_ctx);
    }
    commands
}

#[cfg(test)]
mod command_args_tests {
    use super::*;

    /// The payload's commands carry the argument values that drive the TUI
    /// popup's nested levels.
    #[test]
    fn commands_carry_argument_values() {
        let cfg = AppConfig::default();
        let models = vec![ModelInfo {
            name: "deepseek/deepseek-chat".into(),
        }];
        let skills = vec![jyc_types::SkillMeta {
            name: "ponytail".to_string(),
            description: "Laziest solution that works".to_string(),
            source_path: std::path::PathBuf::from("/skills/ponytail"),
        }];
        let subagent_types = vec![(
            "review".to_string(),
            "Reviews a change before you push (shares your directory)".to_string(),
        )];
        let commands = commands_for_topic(
            None,
            &cfg,
            &skills,
            &models,
            &subagent_types,
            "command-args-test",
            &[],
        );

        let find = |name: &str| commands.iter().find(|c| c.name == name).expect(name);
        assert_eq!(find("/model").args[0].value, "deepseek/deepseek-chat");
        assert!(
            find("/model").args.iter().any(|a| a.value == "reset"),
            "`reset` rides along with the models"
        );
        let on = find("/skill")
            .args
            .iter()
            .find(|a| a.value == "on")
            .expect("/skill on");
        assert_eq!(on.args[0].value, "ponytail");
        assert!(
            find("/plan").args.is_empty(),
            "a free-text command carries none"
        );
        // `/spawn`'s level: the types of this instance first (the caller's
        // lookup, not this function's), then the flags — and the type's own
        // line arrives with the default its role declares.
        let spawn = find("/spawn");
        assert_eq!(spawn.args[0].value, "review");
        assert!(spawn.args[0].description.contains("shares your directory"));
        assert_eq!(
            spawn.args[0]
                .args
                .iter()
                .map(|a| a.value.as_str())
                .collect::<Vec<_>>(),
            ["--inherit", "--share", "--name", "--dir"]
        );
        // The skill itself arrives as a `/skill:<name>` row, described by its
        // frontmatter, and continues into the agent run.
        let row = find("/skill:ponytail");
        assert_eq!(row.description, "Laziest solution that works");
        assert!(row.continues_to_agent);
    }
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
pub(crate) fn seed_next_id_from_disk(
    state: &mut TopicActivityState,
    topic_name: &str,
    topic_path: Option<&Path>,
) {
    if state.next_id != 0 {
        return;
    }
    let mem_max = state.entries.iter().map(|e| e.id).max().unwrap_or(0);
    if mem_max > 0 {
        state.next_id = mem_max + 1;
        return;
    }
    if let Some(path) = topic_path
        && let Ok(last) = ActivityLogStore::load_recent(topic_name, path, 1)
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
/// Filter chat messages by `since` timestamp (RFC 3339 string).
#[cfg(test)]
mod agent_of_topic_tests {
    use super::*;
    use tempfile::tempdir;

    /// The agent comes from the home the registered state dir sits in, for
    /// both home shapes: an agent-keyed topic's own directory, and a dynamic
    /// topic nested under its agent.
    #[test]
    fn agent_keyed_and_dynamic_homes_both_name_their_agent() {
        let tmp = tempdir().expect("tempdir");
        let agents = tmp.path().join("agents");

        jyc_types::state_dir::register(
            "agent-of-topic-keyed",
            &agents.join("invoice").join(".jyc"),
        );
        jyc_types::state_dir::register(
            "agent-of-topic-dynamic",
            &agents.join("invoice").join("issue-42").join(".jyc"),
        );

        assert_eq!(
            agent_of_topic("agent-of-topic-keyed", &agents).as_deref(),
            Some("invoice"),
        );
        assert_eq!(
            agent_of_topic("agent-of-topic-dynamic", &agents).as_deref(),
            Some("invoice"),
        );

        jyc_types::state_dir::unregister("agent-of-topic-keyed");
        jyc_types::state_dir::unregister("agent-of-topic-dynamic");
    }

    /// A topic with no registered state names no agent, and one whose state
    /// sits outside the agents dir (a legacy layout) does not either — the
    /// renderers show both as a dash.
    #[test]
    fn unregistered_and_foreign_homes_name_no_agent() {
        let tmp = tempdir().expect("tempdir");
        let agents = tmp.path().join("agents");

        assert_eq!(agent_of_topic("agent-of-topic-missing", &agents), None);

        jyc_types::state_dir::register(
            "agent-of-topic-legacy",
            &tmp.path().join("legacy").join(".jyc"),
        );
        assert_eq!(agent_of_topic("agent-of-topic-legacy", &agents), None);
        jyc_types::state_dir::unregister("agent-of-topic-legacy");
    }
}

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
    fn session_status_entry_carries_raw_event() {
        let entry = event_to_activity(&no_reply_event());
        assert_eq!(entry.event["type"], "session_status");
        assert_eq!(entry.event["status_type"], "no_reply");
        assert_eq!(
            entry.event["message"],
            "AI produced no text and no tool call"
        );
        assert!(entry.text.is_none());
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
        seed_next_id_from_disk(&mut state, "", None);
        assert_eq!(state.next_id, 1);
    }

    #[test]
    fn seed_next_id_uses_in_memory_max_id() {
        let mut state = TopicActivityState::default();
        state.entries.push_back(ActivityEntry {
            id: 42,
            event: serde_json::Value::Null,
            text: Some("old".to_string()),
            timestamp: None,
        });
        seed_next_id_from_disk(&mut state, "", None);
        assert_eq!(state.next_id, 43);
    }

    #[test]
    fn seed_next_id_falls_back_to_disk_when_memory_is_empty() {
        let tmp = TempDir::new().unwrap();
        let topic_path = tmp.path().to_path_buf();
        let state_dir = topic_path.join(".jyc");
        fs::create_dir_all(&state_dir).unwrap();
        let entry = ActivityEntry {
            id: 7,
            event: serde_json::Value::Null,
            text: Some("disk".to_string()),
            timestamp: None,
        };
        fs::write(
            state_dir.join("activity.jsonl"),
            serde_json::to_string(&entry).unwrap(),
        )
        .unwrap();
        let topic = "seed_next_id_falls_back_to_disk_when_memory_is_empty";
        jyc_types::state_dir::register(topic, &state_dir);
        let mut state = TopicActivityState::default();
        seed_next_id_from_disk(&mut state, topic, Some(&topic_path));
        assert_eq!(state.next_id, 8);
    }

    #[test]
    fn seed_next_id_noop_once_initialized() {
        let mut state = TopicActivityState {
            next_id: 99,
            ..Default::default()
        };
        seed_next_id_from_disk(&mut state, "", None);
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
            topic_managers: Arc::new(TopicManagers::new()),
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
            ws_shutdown: CancellationToken::new(),
            agents_dir: PathBuf::from("/nonexistent/agents"),
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

    /// The pipe uploads to `POST /api/inbound` (`jyc-channels` pins its half
    /// of that URL); this pins the hub's half, so a rename on either side
    /// cannot pass CI and 404 in production.
    #[tokio::test]
    async fn inbound_upload_route_requires_bearer() {
        let app = build_router(ctx_with_token(Some("secret")));
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/inbound?filename=a.pdf")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// Authed, the request reaches the handler, so a missing `filename` is a
    /// `400`. Nothing is written: only that rejection is safe to exercise
    /// here, because the handler resolves the real `<data dir>/.inbound/`
    /// and a test must not stage files in it.
    #[tokio::test]
    async fn inbound_upload_route_reaches_the_handler() {
        let app = build_router(ctx_with_token(Some("secret")));
        let res = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/inbound")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
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
pub mod pipe_proxy;
mod routes;
pub mod websocket;

pub use activity::ActivityTracker;
pub(crate) use activity::*;
pub use routes::build_router;
