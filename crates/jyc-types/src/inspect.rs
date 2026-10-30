use crate::task::TaskList;
use serde::{Deserialize, Serialize};

// ── State snapshot ──

/// Full runtime state snapshot returned by `get_state`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InspectState {
    /// Seconds since monitor started
    pub uptime_secs: u64,
    /// JYC version
    pub version: String,
    /// Configured channels
    pub channels: Vec<ChannelInfo>,
    /// Active topics across all channels
    pub topics: Vec<TopicInfo>,
    /// Aggregate statistics
    pub stats: GlobalStats,
    /// Available models (name only), populated from agent config providers
    #[serde(default)]
    pub models: Vec<ModelInfo>,
}

/// Slim overview payload returned by `get_state_overview`.
///
/// Same shape as `InspectState` but with `TopicSummary` instead of `TopicInfo`,
/// dropping `activity`, `recent_messages`, and `thinking_text` to keep the
/// per-poll payload small.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InspectOverview {
    /// Seconds since monitor started
    pub uptime_secs: u64,
    /// JYC version
    pub version: String,
    /// Configured channels
    pub channels: Vec<ChannelInfo>,
    /// Active topics across all channels (lazy summary — no activity/messages)
    pub topics: Vec<TopicSummary>,
    /// Aggregate statistics
    pub stats: GlobalStats,
    /// Available models (name only), populated from agent config providers
    #[serde(default)]
    pub models: Vec<ModelInfo>,
}

/// Information about an active topic — the list form, for the overview page.
///
/// What the overview table needs and nothing more: who the topic is, whether
/// it is working, how much of its context window is used, and when it was
/// last active. Every other figure — model, mode, tokens, cost, latency,
/// tasks, branch, changed files — belongs to the topic's *detail*, which
/// `GET /api/topics/{channel}/{topic}/info` serves and the info pane and
/// `/info` read. One source per figure: a field is either in the list or in
/// the detail, never in both with two ways to fill it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicSummary {
    /// Topic name (e.g., "issue-42", "pr-43", "support-ticket")
    pub name: String,
    /// Channel this topic belongs to
    pub channel: String,
    /// The agent this topic belongs to — the first path segment of its home
    /// under the agents dir (`<agents dir>/<agent>/…`), resolved server-side.
    /// `None` when the home cannot be located (e.g. a topic registered
    /// outside the agents dir).
    ///
    /// Equal to `name` for an agent-keyed topic (a 1:1 topic and a `/spawn`
    /// child each own their own home); it differs for a pipe-routed dynamic
    /// topic, whose home nests it under its agent.
    #[serde(default)]
    pub agent: Option<String>,
    /// Pattern that created this topic (from `.jyc/pattern`)
    pub pattern: Option<String>,
    /// Current processing status
    pub status: TopicStatus,
    /// Current input tokens used in this session (= the input the most
    /// recent LLM call was sent). None when the session has no recorded
    /// context yet.
    pub context_input_tokens: Option<u64>,
    /// Output tokens of the most recent LLM call — the reply that gets
    /// appended to the context. Together with `context_input_tokens` this
    /// is the size of the context the next call will carry. None when the
    /// session has no recorded output yet.
    #[serde(default)]
    pub context_output_tokens: Option<u64>,
    /// The model's context window for this topic, in tokens — the whole
    /// budget the next call has to fit into.
    pub max_tokens: Option<u64>,
    /// Last activity timestamp (RFC 3339), if known
    #[serde(default)]
    pub last_active_at: Option<String>,
    /// Filesystem path for this topic (may differ from workspace/name when
    /// a pattern's `topic_path` override is active).
    #[serde(default)]
    pub topic_path: Option<std::path::PathBuf>,
    /// Name of the topic that spawned this one — read server-side from the
    /// `spawned-from` breadcrumb in the topic's state dir, the same file the
    /// subagent grants are validated against. `None` for topics the user
    /// started (and for a subagent whose breadcrumb can't be read), so the
    /// renderers treat it as "a root" and show it at the top level.
    ///
    /// The channel is not carried: a spawn always creates its child in the
    /// parent's channel (the tool spawns through the parent's own topic
    /// manager), so `(parent, channel)` names the parent uniquely.
    /// `#[serde(default)]` so old payloads (pre-this-field) still load.
    #[serde(default)]
    pub parent: Option<String>,
    /// Commands available in this topic: built-ins + globals +
    /// per-agent commands (`[[agents.<pattern>.commands]]`).
    ///
    /// Per-agent wins on name collision, matching runtime dispatch
    /// (`CommandRegistry::register` last-registered-wins). The dashboard
    /// `/` command popup reads from this field; it is the single source
    /// of truth for "what commands does this topic have" — same answer
    /// `/?` produces inside the topic.
    /// `#[serde(default)]` so older server payloads still deserialize
    /// (the popup falls back to the overview-level list in that case).
    #[serde(default)]
    pub commands: Vec<CommandInfo>,
}

/// Per-file change kind relative to the repo's default branch, derived
/// from the status letter in the default-branch `git diff --name-status`. `Modified` is the
/// default so old payloads (pre-this-field) and absent-on-wire copies
/// fall through as the most common case. The chat info pane renders a
/// one-column prefix glyph per row: `+` for Added, `-` for Deleted,
/// two spaces for Modified. Untracked, renamed, copied, and
/// type-change statuses from `git diff --name-status` are normalized
/// to `Modified` server-side (YAGNI on full status coverage).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    #[default]
    Modified,
    Added,
    Deleted,
}

/// Line counts of a file's change, straight from `git diff --numstat`.
///
/// Both numbers describe the same diff as the entry's `change`: the
/// branch-side diff for a committed file, the working-tree-vs-HEAD one
/// for a file that is only dirty.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineStats {
    pub insertions: u32,
    pub deletions: u32,
}

/// A file in `TopicSummary.changed_files` / `TopicInfo.changed_files`.
///
/// `uncommitted == true` iff the working tree has changes vs HEAD
/// (`git diff --name-status HEAD`); the chat info pane renders such
/// paths in yellow. `change` carries the branch-side status
/// (Added / Modified / Deleted) and is `#[serde(default)]` so old
/// payloads without the field deserialize as `Modified`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangedFileEntry {
    pub path: String,
    #[serde(default)]
    pub uncommitted: bool,
    #[serde(default)]
    pub change: ChangeKind,
    /// `None` when the change has no line counts to report — a binary
    /// file (`-`/`-` in `--numstat`), or a numstat pass that failed.
    /// `#[serde(default)]` keeps payloads from a server that predates
    /// the field readable (the pane then renders no counts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<LineStats>,
}

/// Accumulated cost for a topic, in `currency`.
///
/// `turn` is scoped to the latest turn, `session` to the current agent
/// session (both read from `agent-session.json`; a reset zeroes them);
/// `today` is the durable per-day total from the billing ledger, which no
/// reset touches. The row orders them by breadth, smallest first, and a
/// session outlives a day, so it is usually the largest of the three.
///
/// `session` and `today` are expected to differ after a reset — that is
/// the point of keeping both.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TopicCost {
    /// Cost of the latest turn so far. `None` when the current turn has
    /// not completed an LLM call yet — and in a payload from a server
    /// that predates the field, where `#[serde(default)]` yields `None`.
    /// The row then leaves the turn out rather than printing a `$0.00`
    /// it cannot vouch for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<f64>,
    /// Cost of the current session so far.
    pub session: f64,
    /// Cost accumulated today (UTC day), across sessions.
    pub today: f64,
    /// Currency label, e.g. `"USD"`. `"mixed"` when today's entries
    /// span more than one currency, where the summed amount is not a
    /// meaningful single figure.
    pub currency: String,
}

/// Format a cost amount for display.
///
/// Renders `$` for USD and `¥` for CNY (the two currencies the feature
/// was specified against); any other label is shown as a suffix, so an
/// unrecognised currency still reads correctly rather than being
/// mislabelled with the wrong symbol.
///
/// A cent is the smallest unit that reads as money, so an amount of at
/// least a cent keeps two decimals — the figures a bill would show. A
/// smaller amount escalates to four: a single short exchange, or a turn
/// that has just started, can sit well below a cent, and rounding it to
/// `0.00` reads as "nothing spent" when something was. Zero itself stays
/// `0.00`.
pub fn format_amount(amount: f64, currency: &str) -> String {
    let body = if amount != 0.0 && amount.abs() < 0.01 {
        format!("{amount:.4}")
    } else {
        format!("{amount:.2}")
    };
    match currency {
        "USD" => format!("${body}"),
        "CNY" => format!("¥{body}"),
        other => format!("{body} {other}"),
    }
}

/// Format the topic's cost row as one display string: `"$0.0031 turn ·
/// $0.05 today · $1.31 session"` — smallest scope first.
///
/// The row orders the three scopes by breadth, smallest first — turn,
/// today, session — and a session outlives a UTC day, so it is usually
/// the largest. That is an ordering, not a nesting guarantee: a session
/// reset mid-day leaves the day's ledger total larger than the fresh
/// session's. The row leaves the turn out when the current turn has no
/// call yet, reading `"$0.05 today · $1.31 session"`.
///
/// Takes the struct rather than the amounts: they share a type and a
/// currency, so swapped arguments would compile and print each figure
/// under the other's label.
pub fn format_cost_row(cost: &TopicCost) -> String {
    let today = format_amount(cost.today, &cost.currency);
    let session = format_amount(cost.session, &cost.currency);
    match cost.turn {
        Some(turn) => format!(
            "{} turn · {today} today · {session} session",
            format_amount(turn, &cost.currency)
        ),
        None => format!("{today} today · {session} session"),
    }
}

/// Format the day's per-currency totals as one display string: `"$12.34"`,
/// or `"$12.34 · ¥3.20"` when the day spans two currencies — the sums
/// cannot be added, so they stay apart.
///
/// `None` when there is nothing to show.
pub fn format_currency_totals(totals: &[(String, f64)]) -> Option<String> {
    if totals.is_empty() {
        return None;
    }
    Some(
        totals
            .iter()
            .map(|(currency, amount)| format_amount(*amount, currency))
            .collect::<Vec<_>>()
            .join(" · "),
    )
}

/// Information about a configured channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// Channel name from config (e.g., "emf", "work")
    pub name: String,
    /// Channel type: "email", "feishu", "github"
    pub channel_type: String,
    /// Number of active workers holding semaphore permits in this channel.
    #[serde(default)]
    pub active_workers: usize,
    /// Max concurrent workers allowed for this channel.
    #[serde(default)]
    pub max_concurrent: usize,
}

/// One region of the context sent to the most recent LLM call.
///
/// The regions are the three parts `SlidingWindow` sends — prior history,
/// the folded part of the current turn, and the current turn's recent raw
/// suffix — plus the single region `Full` mode sends. The figures describe
/// the wire messages only: the system prompt, tool definitions and provider
/// framing are outside them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextRegion {
    /// The whole context, unsplit (`Full` mode).
    Full,
    /// Prior turns, rendered as text-only user/assistant pairs.
    History,
    /// The folded older part of the current turn (`SlidingWindow` only).
    Checkpoint,
    /// The current turn's recent messages, sent verbatim.
    Current,
}

impl ContextRegion {
    /// The region's display label, shared by `/info` and the info pane.
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "Full context",
            Self::History => "History",
            Self::Checkpoint => "Checkpoint",
            Self::Current => "Current turn",
        }
    }
}

/// The size of one [`ContextRegion`] in the context sent to the most recent
/// LLM call.
///
/// `estimated_tokens` reuses the context budget's heuristic (CJK characters
/// count one token, other bytes a quarter) — it is not a provider tokenizer
/// and does not include the system prompt or tools. `bytes` is the exact
/// compact-JSON size of the region's wire messages, so the two figures
/// answer different questions: weight for the share, bytes for the payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextRegionInfo {
    /// Which part of the context this row describes.
    pub region: ContextRegion,
    /// Number of wire messages in the region.
    pub messages: u64,
    /// Exact compact-JSON bytes of the region's messages.
    pub bytes: u64,
    /// Heuristic token estimate for the region's messages.
    pub estimated_tokens: u64,
}

/// Information about an active topic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicInfo {
    /// Topic name (e.g., "issue-42", "pr-43", "support-ticket")
    pub name: String,
    /// Channel this topic belongs to
    pub channel: String,
    /// Pattern that created this topic (from `.jyc/pattern`)
    pub pattern: Option<String>,
    /// Current processing status
    pub status: TopicStatus,
    /// Messages accepted for this topic that the model has not read yet —
    /// the topic's backlog. The ones still in the queue, the ones the
    /// running turn pulled aside for a turn of their own, and the ones
    /// the running turn has taken in and the model has not read yet.
    ///
    /// Zero when the source does not report a backlog.
    #[serde(default)]
    pub queued: usize,
    /// How many messages the topic's queue holds before new ones are
    /// dropped — the queue's capacity, one slot per message the bar
    /// draws.
    ///
    /// Zero when the source does not report a capacity: the surfaces
    /// then omit the queue row rather than draw a bar of no slots.
    #[serde(default)]
    pub queue_capacity: usize,
    /// Name of the topic that spawned this one — read from the
    /// `spawned-from` breadcrumb in the topic's state dir. See
    /// `TopicSummary::parent` for the full semantics.
    ///
    /// The topic manager resolves it: one owner for the lineage, so the
    /// list and the detail cannot disagree about who spawned whom.
    #[serde(default)]
    pub parent: Option<String>,
    /// AI model in use (from model-override or default)
    pub model: Option<String>,
    /// Current mode (plan/build)
    pub mode: Option<String>,
    /// Current input tokens used in this session (= the input the most
    /// recent LLM call was sent).
    pub context_input_tokens: Option<u64>,
    /// Output tokens of the most recent LLM call — the reply appended to
    /// the context. Together with `context_input_tokens` this is the size
    /// of the context the next call will carry.
    #[serde(default)]
    pub context_output_tokens: Option<u64>,
    /// The model's context window for this topic, in tokens — the whole
    /// budget the next call has to fit into.
    pub max_tokens: Option<u64>,
    /// How the context sent to the most recent LLM call was split into
    /// regions. Empty when no call has completed yet, or when the session
    /// file predates this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_composition: Vec<ContextRegionInfo>,
    /// Running total of output tokens generated by LLM responses in this
    /// session. Mirrors the same field on `TopicSummary` so the
    /// activity/chat pane can show it without a separate fetch.
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// Output tokens of the LLM calls in the latest turn — the
    /// turn-scoped twin of `output_tokens`, so the info pane can print
    /// the turn beside the session total. `None` while the current turn
    /// has produced nothing.
    #[serde(default)]
    pub turn_output_tokens: Option<u64>,
    /// Reasoning (thinking) tokens of the LLM calls in the latest turn —
    /// the turn-scoped twin of `total_reasoning_tokens`, so the info pane
    /// can print the turn beside the session total the way it already
    /// does for output. `None` while the current turn has produced none.
    #[serde(default)]
    pub turn_reasoning_tokens: Option<u64>,
    /// Accumulated input tokens across all LLM calls in this session.
    /// See `TopicSummary::total_input_tokens` for the full description.
    #[serde(default)]
    pub total_input_tokens: Option<u64>,
    /// Accumulated prompt-cache-hit tokens across all LLM calls in
    /// this session. See `TopicSummary::total_cache_hit_tokens` for
    /// the full description.
    #[serde(default)]
    pub total_cache_hit_tokens: Option<u64>,
    /// Accumulated prompt-cache-**creation** (write) tokens across
    /// all LLM calls in this session. See
    /// `TopicSummary::total_cache_creation_tokens` for the full
    /// description.
    #[serde(default)]
    pub total_cache_creation_tokens: Option<u64>,
    /// Accumulated reasoning (thinking) tokens across all LLM calls in
    /// this session — the hidden chain-of-thought share of the output,
    /// already included in `output_tokens`. Informational only; `None`
    /// when zero or reported by no provider.
    #[serde(default)]
    pub total_reasoning_tokens: Option<u64>,
    /// The prompt-cache-hit tokens of the LLM calls in the latest turn.
    /// The turn-scoped twin of `total_cache_hit_tokens` — the numerator
    /// of the turn's cache rate. `None` when the turn has made no call
    /// yet.
    #[serde(default)]
    pub turn_cache_hit_tokens: Option<u64>,
    /// The input tokens of the LLM calls in the latest turn — the
    /// turn-scoped twin of `total_input_tokens`, the denominator of the
    /// turn's cache rate. `None` when the turn has made no call yet.
    #[serde(default)]
    pub turn_input_tokens: Option<u64>,
    /// Average wall-clock time of one LLM call in this session, in
    /// milliseconds. See `TopicSummary::avg_call_ms` for the full
    /// description.
    #[serde(default)]
    pub avg_call_ms: Option<u64>,
    /// Number of LLM calls the average covers. See
    /// `TopicSummary::call_count` for the full description.
    #[serde(default)]
    pub call_count: Option<u64>,
    /// Average wall-clock time of one LLM call in the latest turn, in
    /// milliseconds. See `TopicSummary::avg_turn_call_ms` for the full
    /// description.
    #[serde(default)]
    pub avg_turn_call_ms: Option<u64>,
    /// Number of LLM calls the latest-turn average covers. See
    /// `TopicSummary::turn_call_count` for the full description.
    #[serde(default)]
    pub turn_call_count: Option<u64>,
    /// Recent activity events (newest first, max ~20)
    #[serde(default)]
    pub activity: Vec<ActivityEntry>,
    /// Last activity timestamp (RFC 3339), if known
    #[serde(default)]
    pub last_active_at: Option<String>,
    /// Skills loaded for this topic
    #[serde(default)]
    pub skills: Vec<String>,
    /// Skills this topic can *invoke* — every discovered skill, whatever the
    /// topic's `skills` allow-list says (the allow-list decides what the
    /// system prompt lists, not what a message may ask for). Drives the
    /// `/skill:<name>` rows in the dashboard/TUI command popup. See
    /// [`jyc_core::agent::AgentService::available_skills`].
    #[serde(default)]
    pub available_skills: Vec<crate::SkillMeta>,
    /// Recent chat messages (incoming + replies) for live dashboard display
    #[serde(default)]
    pub recent_messages: Vec<ChatMessageEntry>,
    /// Latest AI thinking/reasoning text for live dashboard display.
    /// Set while the topic is processing and thinking is enabled; cleared on completion.
    #[serde(default)]
    pub thinking_text: Option<String>,
    /// Filesystem path for this topic (may differ from workspace/name when
    /// a pattern's `topic_path` override is active).
    #[serde(default)]
    pub topic_path: Option<std::path::PathBuf>,
    /// Resolved git branch for the topic's working directory. See
    /// `TopicSummary::branch` for the full semantics.
    #[serde(default)]
    pub branch: Option<String>,
    /// Files changed relative to `main`. See
    /// `TopicSummary::changed_files` for the full semantics; this field
    /// uses the same `Vec<ChangedFileEntry>` shape (path + uncommitted flag).
    #[serde(default)]
    pub changed_files: Option<Vec<ChangedFileEntry>>,
    /// The agent's current task list. See `TopicSummary::tasks` for the full
    /// semantics; this field carries the same `TaskList` shape.
    #[serde(default)]
    pub tasks: TaskList,
    /// Accumulated cost (session + today). `None` when the active model
    /// has no configured `pricing`, so the row is omitted entirely
    /// rather than showing a misleading zero.
    #[serde(default)]
    pub cost: Option<TopicCost>,
    /// The day's spend across every topic, per currency — the same
    /// figure `/bill` reports. Kept apart from `cost`, which is this
    /// topic's own spend. Empty when the day's ledger holds no entry.
    #[serde(default)]
    pub today_all: Vec<(String, f64)>,
    /// Commands available in this topic. See `TopicSummary::commands`
    /// for the full semantics.
    #[serde(default)]
    pub commands: Vec<CommandInfo>,
    /// The subagents this topic spawned, as a tree: every node's
    /// `children` are the subagents it spawned in turn.
    ///
    /// `None` when the lineage was not resolved — the manager resolves it
    /// for a topic's detail and leaves it out for the list shape, which has
    /// no field to carry a tree — and `Some(vec![])` when the topic has no
    /// subagents at all.
    #[serde(default)]
    pub subagents: Option<Vec<TopicNode>>,
}

/// A topic in a lineage tree: a subagent of the topic above it, with the
/// subagents it spawned under it.
///
/// The tree is the same data whichever way it is read — the info pane draws
/// it, `/info` prints it — so it travels inside [`TopicInfo`] rather than
/// being fetched per surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicNode {
    /// Topic name (e.g. "ci-watch-pr-961")
    pub name: String,
    /// Channel this topic belongs to
    pub channel: String,
    /// Current processing status
    pub status: TopicStatus,
    /// The subagents this topic spawned, nested
    #[serde(default)]
    pub children: Vec<TopicNode>,
}

/// Severity level for an activity entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum Severity {
    #[default]
    Info,
    Warning,
    Error,
}

/// A single activity event from the topic's SSE stream.
///
/// The backend records the raw event; each client decides how (and whether)
/// to render it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    /// The raw topic event, flattened to `{ "type": <variant>, … }`
    /// (`tool_completed`, `session_status`, …).
    #[serde(default)]
    pub event: serde_json::Value,
    /// RFC 3339 timestamp for ordering and cross-day sorting
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Monotonic per-topic sequence number assigned by the inspect server's
    /// `ActivityTracker` on push. Used by WS clients to drop duplicate events
    /// after a reconnect / Lagged recovery. Defaults to 0 for entries from
    /// `.jyc/activity.jsonl` that predate the seq field.
    #[serde(default)]
    pub id: u64,
    /// Prose carried by entries written before the raw-event shape. `None`
    /// for new entries; clients render it verbatim when `event` is absent.
    /// ponytail: delete once existing logs have rotated past the old shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// A chat message entry for live display in the dashboard.
///
/// Captured from `TopicEvent::IncomingMessage` and `TopicEvent::ReplySent`
/// by the ActivityTracker and forwarded via `TopicInfo.recent_messages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessageEntry {
    /// Sender label: "user", "ai", or a display name
    pub sender: String,
    /// Message or reply text
    pub text: String,
    /// RFC 3339 timestamp
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Per-topic chat sequence number, carried by both the live row and the
    /// logged row for the same message (`0` for rows logged before the
    /// sequence existed).
    #[serde(default)]
    pub id: u64,
    /// The dashboard client's own id for a message it sent, echoed back so
    /// the client can recognize (and replace) its optimistic copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_msg_id: Option<String>,
}

/// Topic processing status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TopicStatus {
    /// Waiting for semaphore permit
    Queued,
    /// AI processing active
    Processing,
    /// Worker running, waiting for messages
    #[default]
    Idle,
    /// Topic encountered an error
    Error,
}

impl std::fmt::Display for TopicStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Queued => write!(f, "Queued"),
            Self::Processing => write!(f, "Processing"),
            Self::Idle => write!(f, "Idle"),
            Self::Error => write!(f, "Error"),
        }
    }
}

/// Aggregate statistics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalStats {
    /// Number of active workers (holding semaphore permits)
    pub active_workers: usize,
    /// Total number of open topics
    pub total_topics: usize,
    /// Max concurrent workers allowed
    pub max_concurrent: usize,
    /// Number of available worker slots (max_concurrent - active_workers)
    pub available_workers: usize,
    /// Total messages received since startup
    pub messages_received: u64,
    /// Total messages processed since startup
    pub messages_processed: u64,
    /// Total errors since startup
    pub errors: u64,
}

/// Information about an available command (name + description).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandInfo {
    /// Command name including slash (e.g., "/model")
    pub name: String,
    /// Short description (e.g., "Switch AI model for this topic")
    pub description: String,
    /// The command's usage line, derived from the shape and flags it declares
    /// (e.g. "/close [subagent|all] [--force] [--purge]"). The name alone when
    /// the command takes nothing. `serde(default)` so a payload from a server
    /// older than the field deserializes — [`Self::summary`] then falls back to
    /// the description, which is where the usage used to be written by hand.
    #[serde(default)]
    pub usage: String,
    /// True if this command continues into an agent run (e.g. it injects
    /// a prompt via `CommandResult::append_body`). False for instant-reply
    /// commands that never reach the agent — those need no progress
    /// indicator. `serde(default)` so old payloads without the field
    /// deserialize as `false`, preserving the pre-field behaviour where
    /// the channel adapter watcher skipped every registered command.
    #[serde(default)]
    pub continues_to_agent: bool,
    /// Enumerable values for this command's arguments, used by the TUI's
    /// multi-level `/` popup. Empty means the command takes free-text
    /// arguments (or none at all), so the popup offers nothing for it.
    /// Filled per topic by the inspect server — see
    /// `jyc_core::command::command_args` for the value table. Omitted from the
    /// payload when empty: most commands take free text, and this rides on
    /// every topic of every overview poll.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<CommandArg>,
}

impl CommandInfo {
    /// What the popup and `/?` show for this command on one line: the
    /// description, followed by the usage when it says more than the name.
    ///
    /// A usage the server did not send (an older payload) leaves the
    /// description as it is, which is where the usage used to be written by
    /// hand.
    pub fn summary(&self) -> String {
        if self.usage.is_empty() || self.usage == self.name {
            self.description.clone()
        } else {
            format!("{}: {}", self.description, self.usage)
        }
    }
}

/// One enumerable argument value of a command (see [`CommandInfo::args`]).
///
/// Nests to arbitrary depth: `args` holds the values of the *next* argument
/// position, so `/skill on <name>` is an `on` entry whose `args` are the
/// skill names. A leaf entry has empty `args`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandArg {
    /// Literal value as it appears on the command line (e.g. "reset",
    /// "hide", or a full model id like "deepseek/deepseek-chat").
    pub value: String,
    /// Optional one-line hint shown next to the value in the popup.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Values for the next argument position. Empty = this value is last, so
    /// it is left off the wire — a leaf is the common case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<CommandArg>,
}

/// Information about an available model (name only, for model picker UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Full model identifier (e.g., "deepseek/deepseek-chat", "claude-sonnet-4-6")
    pub name: String,
}

// ── Protocol constants ──

/// Extract the tool's primary input value from the `ToolStarted` event's JSON
/// input: `service/tools.rs` for file tools, the command for `bash`, the
/// pattern for `grep`, the URL for `webfetch`. Shared by every surface
/// that displays a tool call (feishu progress card, TUI chat progress tail).
///
/// File paths are reduced to their basename; newlines and excess whitespace are
/// collapsed so a multi-line command stays on one display line. The value comes
/// back **uncapped** — how much of it fits a row is each display layer's own
/// decision (`chat/render.rs` caps its collapsed tail, `feishu/progress.rs`
/// caps its card). Tools without a meaningful argument (or with sensitive args
/// like the reply text) return `None`; callers decide whether to fall back to
/// the raw input or show nothing.
pub fn tool_activity(tool_name: &str, input: Option<&str>) -> Option<String> {
    let get = |key: &str| -> Option<String> {
        serde_json::from_str::<serde_json::Value>(input?)
            .ok()?
            .get(key)?
            .as_str()
            .map(str::to_string)
    };
    let raw = match tool_name {
        // File tools: basename keeps the display short (read_image names its
        // argument "path", the others "file_path").
        "read" | "edit" | "write" | "read_image" => {
            let p = get("file_path").or_else(|| get("path"))?;
            p.rsplit('/').next().unwrap_or(&p).to_string()
        }
        "grep" => get("pattern")?,
        "bash" => get("command")?,
        "webfetch" => get("url")?,
        _ => return None,
    };
    // Collapse internal whitespace runs so the value stays displayable on one
    // logical line; the caller decides whether to cap it.
    Some(raw.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Default TCP port for the inspect server.
pub const DEFAULT_INSPECT_PORT: u16 = 9876;

/// Default bind address for the inspect server.
pub const DEFAULT_INSPECT_BIND: &str = "127.0.0.1:9876";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inspect_state_serialize_roundtrip() {
        let state = InspectState {
            uptime_secs: 3600,
            version: "0.1.10".to_string(),
            channels: vec![ChannelInfo {
                name: "emf".to_string(),
                channel_type: "github".to_string(),
                active_workers: 2,
                max_concurrent: 3,
            }],
            topics: vec![TopicInfo {
                tasks: Default::default(),
                name: "issue-42".to_string(),
                channel: "emf".to_string(),
                pattern: Some("planner".to_string()),
                status: TopicStatus::Processing,
                queued: 2,
                queue_capacity: 10,
                parent: None,
                model: Some("anthropic/claude-opus-4-6".to_string()),
                mode: Some("build".to_string()),
                context_input_tokens: Some(45000),
                context_output_tokens: Some(1200),
                max_tokens: Some(120000),
                context_composition: Vec::new(),
                output_tokens: Some(12345),
                turn_output_tokens: None,
                turn_reasoning_tokens: None,
                total_input_tokens: Some(45000),
                total_cache_hit_tokens: None,
                total_cache_creation_tokens: None,
                avg_call_ms: None,
                call_count: None,
                avg_turn_call_ms: None,
                turn_call_count: None,
                total_reasoning_tokens: None,
                turn_cache_hit_tokens: None,
                turn_input_tokens: None,
                today_all: Vec::new(),
                activity: vec![],
                last_active_at: None,
                skills: vec!["coding-principles".to_string(), "dev-workflow".to_string()],
                available_skills: vec![crate::SkillMeta {
                    name: "coding-principles".to_string(),
                    description: "Four principles for LLM coding agents".to_string(),
                    source_path: std::path::PathBuf::from(
                        "/home/u/.config/jyc/skills/coding-principles",
                    ),
                }],
                recent_messages: vec![],
                thinking_text: None,
                topic_path: None,
                branch: None,
                changed_files: None,
                cost: None,
                commands: vec![],
                subagents: None,
            }],
            stats: GlobalStats {
                active_workers: 2,
                total_topics: 3,
                max_concurrent: 3,
                available_workers: 1,
                messages_received: 156,
                messages_processed: 150,
                errors: 2,
            },
            models: vec![],
        };

        let json = serde_json::to_string(&state).unwrap();
        let parsed: InspectState = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.uptime_secs, 3600);
        assert_eq!(parsed.channels.len(), 1);
        assert_eq!(parsed.channels[0].name, "emf");
        assert_eq!(parsed.channels[0].active_workers, 2);
        assert_eq!(parsed.channels[0].max_concurrent, 3);
        assert_eq!(parsed.topics.len(), 1);
        assert_eq!(parsed.topics[0].status, TopicStatus::Processing);
        assert_eq!(parsed.stats.active_workers, 2);
    }

    /// A payload from an older peer carries no queue figures: the fields
    /// default, so the row is omitted rather than drawn wrong.
    #[test]
    fn test_topic_info_without_queue_figures_parses() {
        let parsed: TopicInfo = serde_json::from_str(
            r#"{"name":"t","channel":"c","pattern":null,"status":"idle","model":null,
                "mode":null,"context_input_tokens":null,"max_tokens":null,"output_tokens":null}"#,
        )
        .unwrap();
        assert_eq!(parsed.queued, 0);
        assert_eq!(parsed.queue_capacity, 0);
    }

    #[test]
    fn test_topic_status_display() {
        assert_eq!(format!("{}", TopicStatus::Queued), "Queued");
        assert_eq!(format!("{}", TopicStatus::Processing), "Processing");
        assert_eq!(format!("{}", TopicStatus::Idle), "Idle");
        assert_eq!(format!("{}", TopicStatus::Error), "Error");
    }

    #[test]
    fn test_inspect_state_default() {
        let state = InspectState::default();
        assert_eq!(state.uptime_secs, 0);
        assert!(state.channels.is_empty());
        assert!(state.topics.is_empty());
        assert_eq!(state.stats.active_workers, 0);
    }

    #[test]
    fn test_topic_status_serde() {
        // TopicStatus serializes to snake_case
        let parsed: TopicStatus = serde_json::from_str(r#""processing""#).unwrap();
        assert_eq!(parsed, TopicStatus::Processing);

        let json = serde_json::to_string(&TopicStatus::Error).unwrap();
        assert_eq!(json, r#""error""#);

        let parsed: TopicStatus = serde_json::from_str(r#""error""#).unwrap();
        assert_eq!(parsed, TopicStatus::Error);
    }

    #[test]
    fn test_severity_serde_roundtrip() {
        let json = serde_json::to_string(&Severity::Info).unwrap();
        assert_eq!(json, r#""info""#);
        let parsed: Severity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Severity::Info);

        let json = serde_json::to_string(&Severity::Warning).unwrap();
        assert_eq!(json, r#""warning""#);
        let parsed: Severity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Severity::Warning);

        let json = serde_json::to_string(&Severity::Error).unwrap();
        assert_eq!(json, r#""error""#);
        let parsed: Severity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Severity::Error);
    }

    #[test]
    fn test_severity_default_is_info() {
        assert_eq!(Severity::default(), Severity::Info);
    }

    #[test]
    fn test_activity_entry_backward_compat_old_jsonl() {
        // Old JSONL entries have `text` but no `event` — should deserialize
        // fine, with the prose preserved for legacy rendering.
        let old_json =
            r#"{"time":"12:34:56","text":"Processing started","timestamp":"2025-01-15T12:34:56Z"}"#;
        let entry: ActivityEntry = serde_json::from_str(old_json).unwrap();
        assert_eq!(entry.text.as_deref(), Some("Processing started"));
        assert_eq!(entry.timestamp.as_deref(), Some("2025-01-15T12:34:56Z"));
        assert!(entry.event.is_null());
    }

    #[test]
    fn test_activity_entry_roundtrip_with_raw_event() {
        let entry = ActivityEntry {
            event: serde_json::json!({"type": "tool_completed", "tool_name": "bash", "success": false}),
            timestamp: Some("2025-01-15T12:34:56Z".to_string()),
            id: 0,
            text: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""type":"tool_completed""#));
        assert!(!json.contains("severity"));
        let parsed: ActivityEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event["tool_name"], "bash");
        assert_eq!(parsed.text, None);
    }

    #[test]
    fn test_activity_entry_id_backward_compat() {
        // Old JSONL entries have no `id` field — should default to 0
        let old_json =
            r#"{"text":"old entry","timestamp":"2025-01-15T12:00:00Z","severity":"info"}"#;
        let entry: ActivityEntry = serde_json::from_str(old_json).unwrap();
        assert_eq!(entry.id, 0);
        assert_eq!(entry.text.as_deref(), Some("old entry"));
    }

    #[test]
    fn test_activity_entry_id_roundtrip() {
        let entry = ActivityEntry {
            event: serde_json::Value::Null,
            text: Some("new entry".to_string()),
            timestamp: Some("2025-01-15T12:00:00Z".to_string()),
            id: 42,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""id":42"#));
        let parsed: ActivityEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 42);
    }

    #[test]
    fn test_chat_message_entry_id_backward_compat() {
        let old_json = r#"{"sender":"user","text":"hi","timestamp":"2025-01-15T12:00:00Z"}"#;
        let entry: ChatMessageEntry = serde_json::from_str(old_json).unwrap();
        assert_eq!(entry.id, 0);
        assert_eq!(entry.sender, "user");
    }

    #[test]
    fn test_channel_info_backward_compat() {
        let old_json = r#"{"name":"emf","channel_type":"github"}"#;
        let ch: ChannelInfo = serde_json::from_str(old_json).unwrap();
        assert_eq!(ch.name, "emf");
        assert_eq!(ch.active_workers, 0);
        assert_eq!(ch.max_concurrent, 0);
    }

    #[test]
    fn test_topic_summary_roundtrip() {
        let summary = TopicSummary {
            name: "issue-42".to_string(),
            channel: "emf".to_string(),
            agent: Some("invoice".to_string()),
            pattern: Some("planner".to_string()),
            status: TopicStatus::Processing,
            context_input_tokens: Some(10000),
            context_output_tokens: Some(500),
            max_tokens: Some(120000),
            last_active_at: Some("2026-01-01T00:00:00Z".to_string()),
            topic_path: None,
            parent: None,
            commands: vec![],
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: TopicSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "issue-42");
        assert_eq!(parsed.status, TopicStatus::Processing);
        assert_eq!(parsed.agent.as_deref(), Some("invoice"));
        // TopicSummary must not serialize activity/messages/thinking fields
        assert!(!json.contains("activity"));
        assert!(!json.contains("recent_messages"));
        assert!(!json.contains("thinking_text"));
    }

    #[test]
    fn test_inspect_overview_roundtrip() {
        let overview = InspectOverview {
            uptime_secs: 100,
            version: "0.1.0".to_string(),
            channels: vec![],
            topics: vec![],
            stats: GlobalStats::default(),
            models: vec![],
        };
        let json = serde_json::to_string(&overview).unwrap();
        // InspectOverview is a struct (not a tagged-enum variant), so no `type` tag
        assert!(json.contains(r#""uptime_secs":100"#));
        assert!(json.contains(r#""version":"0.1.0""#));
        let parsed: InspectOverview = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.uptime_secs, 100);
    }

    #[test]
    fn tool_activity_extracts_basename_for_file_tools() {
        let input =
            r#"{"file_path": "/home/jiny/projects/jyc/crates/jyc-agent/src/service/tools.rs"}"#;
        assert_eq!(
            tool_activity("edit", Some(input)).as_deref(),
            Some("tools.rs")
        );
        assert_eq!(
            tool_activity("read", Some(input)).as_deref(),
            Some("tools.rs")
        );
        assert_eq!(
            tool_activity("write", Some(input)).as_deref(),
            Some("tools.rs")
        );
        // read_image names its argument "path", not "file_path".
        assert_eq!(
            tool_activity("read_image", Some(r#"{"path": "/tmp/x/photo.png"}"#)).as_deref(),
            Some("photo.png")
        );
    }

    #[test]
    fn tool_activity_extracts_command_pattern_url() {
        assert_eq!(
            tool_activity("bash", Some(r#"{"command": "ls -la"}"#)).as_deref(),
            Some("ls -la")
        );
        assert_eq!(
            tool_activity("grep", Some(r#"{"pattern": "fn main"}"#)).as_deref(),
            Some("fn main")
        );
        assert_eq!(
            tool_activity("webfetch", Some(r#"{"url": "https://example.com"}"#)).as_deref(),
            Some("https://example.com")
        );
        // Unknown tools and tools with sensitive args produce nothing.
        assert_eq!(tool_activity("jyc_reply_message", Some("{}")), None);
        assert_eq!(tool_activity("bash", None), None);
        assert_eq!(tool_activity("bash", Some("not json")), None);
    }

    #[test]
    fn tool_activity_flattens_whitespace_and_stays_uncapped() {
        // Newlines collapse to spaces so a multi-line command stays on one
        // display line.
        let multi = r#"{"command": "ls\n   -la"}"#;
        assert_eq!(
            tool_activity("bash", Some(multi)).as_deref(),
            Some("ls -la")
        );
        // Length is left to whichever surface displays it (#812): nothing here
        // is cut.
        let long = "a".repeat(200);
        let input = format!(r#"{{"command": "{long}"}}"#);
        assert_eq!(
            tool_activity("bash", Some(&input)).unwrap().chars().count(),
            200
        );
    }

    /// Money reads to the cent; only an amount too small to show one
    /// escalates to four decimals. Both ends of that rule are load-bearing:
    /// a cent-scale amount must not gain noise, and a sub-cent one must not
    /// round to `$0.00` and read as "nothing spent".
    #[test]
    fn format_amount_keeps_a_cent_and_escalates_below_one() {
        assert_eq!(format_amount(0.0, "USD"), "$0.00");
        assert_eq!(format_amount(0.05, "USD"), "$0.05");
        assert_eq!(format_amount(1.3057, "USD"), "$1.31");
        assert_eq!(format_amount(3.2, "USD"), "$3.20");
        // A turn of 100-odd iterations costs dollars, and those stay on the
        // cent scale rather than showing a fraction of one.
        assert_eq!(format_amount(12.3456, "USD"), "$12.35");
        // Below a cent, four decimals: `$0.0031` not `$0.00`.
        assert_eq!(format_amount(0.0031, "USD"), "$0.0031");
        assert_eq!(format_amount(0.0031, "CNY"), "¥0.0031");
        assert_eq!(format_amount(1.5, "EUR"), "1.50 EUR");
    }

    /// The row runs smallest scope first — turn, today, session — and a
    /// missing turn is left out rather than printed as a zero the data
    /// cannot vouch for.
    #[test]
    fn format_cost_row_runs_small_to_large_and_omits_a_missing_turn() {
        let cost = TopicCost {
            turn: Some(0.0031),
            session: 1.3057,
            today: 0.0521,
            currency: "USD".to_string(),
        };
        assert_eq!(
            format_cost_row(&cost),
            "$0.0031 turn · $0.05 today · $1.31 session"
        );
        let no_turn = TopicCost { turn: None, ..cost };
        assert_eq!(format_cost_row(&no_turn), "$0.05 today · $1.31 session");
    }

    /// A payload that predates the `turn` field — an older daemon's JSON —
    /// deserializes with `None`, and `None` is not written back out, so the
    /// row simply omits the turn instead of claiming `$0.00`.
    #[test]
    fn topic_cost_turn_is_optional_for_older_payloads() {
        let old = r#"{"session": 0.42, "today": 1.23, "currency": "USD"}"#;
        let cost: TopicCost = serde_json::from_str(old).unwrap();
        assert_eq!(cost.turn, None);
        let json = serde_json::to_string(&cost).unwrap();
        assert!(
            !json.contains("turn"),
            "turn should not be serialized: {json}"
        );
    }
}
