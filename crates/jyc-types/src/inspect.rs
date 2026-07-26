use serde::{Deserialize, Serialize};

// ── HTTP API ──
//
// The inspect server speaks HTTP (see `jyc-inspect`). Endpoints:
//
//   GET  /health          → HealthResponse
//   GET  /state           → InspectState
//   POST /reload_config   → ReloadResult
//   POST /reset_session   body: ResetSessionRequest → ResetSessionResult
//   POST /inject_message  body: InjectMessageRequest → InjectMessageResult
//   GET  /ws[/<channel>]  → WebSocket upgrade (chat)
//
// All non-loopback requests require `Authorization: Bearer <token>`.

/// Body for `POST /reset_session`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ResetSessionRequest {
    pub thread_name: String,
}

/// Body for `POST /inject_message`.
#[derive(Debug, Serialize, Deserialize)]
pub struct InjectMessageRequest {
    pub channel: String,
    pub thread: String,
    pub text: String,
}

/// Response from `POST /reload_config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadResult {
    pub success: bool,
    pub message: String,
}

/// Response from `POST /reset_session`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetSessionResult {
    pub success: bool,
    pub message: String,
}

/// Response from `POST /inject_message`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectMessageResult {
    pub success: bool,
    pub message: String,
}

/// Response from `GET /thread/{channel}/{name}/history` — persisted chat
/// history loaded from `chat_history_*.jsonl` on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadHistoryResponse {
    /// Channel name (echoed from path)
    pub channel: String,
    /// Thread name (echoed from path)
    pub thread: String,
    /// Chat messages ordered oldest-first, capped at the server's limit (100).
    pub messages: Vec<ChatMessageEntry>,
}

/// Response from `GET /thread/{channel}/{name}/activity` — recent activity
/// events for a thread from the in-memory `activity_map` (mirroring the
/// thread's ThreadEventBus subscriptions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadActivityResponse {
    /// Channel name (echoed from path)
    pub channel: String,
    /// Thread name (echoed from path)
    pub thread: String,
    /// Activity entries ordered oldest-first, capped at the server's limit (50).
    pub entries: Vec<ActivityEntry>,
}

/// Response from `GET /health`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
}

impl HealthResponse {
    /// Canonical "ok" response.
    pub fn ok() -> Self {
        Self {
            status: "ok".to_string(),
        }
    }
}

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
    /// Active threads across all channels
    pub threads: Vec<ThreadInfo>,
    /// Aggregate statistics
    pub stats: GlobalStats,
    /// Available commands (name + description), populated from server-side CommandRegistry
    #[serde(default)]
    pub commands: Vec<CommandInfo>,
    /// Available models (name only), populated from agent config providers
    #[serde(default)]
    pub models: Vec<ModelInfo>,
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

/// Information about an active thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadInfo {
    /// Thread name (e.g., "issue-42", "pr-43", "support-ticket")
    pub name: String,
    /// Channel this thread belongs to
    pub channel: String,
    /// Pattern that created this thread (from `.jyc/pattern`)
    pub pattern: Option<String>,
    /// Current processing status
    pub status: ThreadStatus,
    /// AI model in use (from model-override or default)
    pub model: Option<String>,
    /// Current mode (plan/build)
    pub mode: Option<String>,
    /// Current input tokens used in this session
    pub input_tokens: Option<u64>,
    /// Max input tokens for this session
    pub max_tokens: Option<u64>,
    /// Recent activity events (newest first, max ~20)
    #[serde(default)]
    pub activity: Vec<ActivityEntry>,
    /// Last activity timestamp (RFC 3339), if known
    #[serde(default)]
    pub last_active_at: Option<String>,
    /// Skills loaded for this thread
    #[serde(default)]
    pub skills: Vec<String>,
    /// Recent chat messages (incoming + replies) for live dashboard display
    #[serde(default)]
    pub recent_messages: Vec<ChatMessageEntry>,
    /// Latest AI thinking/reasoning text for live dashboard display.
    /// Set while the thread is processing and thinking is enabled; cleared on completion.
    #[serde(default)]
    pub thinking_text: Option<String>,
    /// Filesystem path for this thread (may differ from workspace/name when
    /// a pattern's `thread_path` override is active).
    #[serde(default)]
    pub thread_path: Option<std::path::PathBuf>,
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

/// A single activity event from the thread's SSE stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    /// Human-readable description
    pub text: String,
    /// RFC 3339 timestamp for ordering and cross-day sorting
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Severity level (defaults to Info for backward compat)
    #[serde(default)]
    pub severity: Severity,
}

/// A chat message entry for live display in the dashboard.
///
/// Captured from `ThreadEvent::IncomingMessage` and `ThreadEvent::ReplySent`
/// by the ActivityTracker and forwarded via `ThreadInfo.recent_messages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessageEntry {
    /// Sender label: "user", "ai", or a display name
    pub sender: String,
    /// Message or reply text
    pub text: String,
    /// RFC 3339 timestamp
    #[serde(default)]
    pub timestamp: Option<String>,
}

/// Thread processing status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ThreadStatus {
    /// Waiting for semaphore permit
    Queued,
    /// AI processing active
    Processing,
    /// Worker running, waiting for messages
    #[default]
    Idle,
    /// Question tool waiting for user reply
    WaitingForAnswer,
    /// Thread encountered an error
    Error,
}

impl std::fmt::Display for ThreadStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Queued => write!(f, "Queued"),
            Self::Processing => write!(f, "Processing"),
            Self::Idle => write!(f, "Idle"),
            Self::WaitingForAnswer => write!(f, "Waiting"),
            Self::Error => write!(f, "Error"),
        }
    }
}

/// Aggregate statistics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalStats {
    /// Number of active workers (holding semaphore permits)
    pub active_workers: usize,
    /// Total number of open threads
    pub total_threads: usize,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandInfo {
    /// Command name including slash (e.g., "/model")
    pub name: String,
    /// Short description (e.g., "Switch AI model for this thread")
    pub description: String,
}

/// Information about an available model (name only, for model picker UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Full model identifier (e.g., "deepseek/deepseek-chat", "claude-sonnet-4-6")
    pub name: String,
}

// ── Protocol constants ──

/// Default TCP port for the inspect server.
pub const DEFAULT_INSPECT_PORT: u16 = 9876;

/// Default bind address for the inspect server.
pub const DEFAULT_INSPECT_BIND: &str = "127.0.0.1:9876";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_response_ok() {
        let resp = HealthResponse::ok();
        assert_eq!(resp.status, "ok");

        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"status":"ok"}"#);

        let parsed: HealthResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, "ok");
    }

    #[test]
    fn test_reset_session_request_serde() {
        let req = ResetSessionRequest {
            thread_name: "issue-42".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"thread_name":"issue-42"}"#);

        let parsed: ResetSessionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.thread_name, "issue-42");
    }

    #[test]
    fn test_inject_message_request_serde() {
        let req = InjectMessageRequest {
            channel: "emf".to_string(),
            thread: "pr-43".to_string(),
            text: "please review".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""channel":"emf""#));
        assert!(json.contains(r#""thread":"pr-43""#));
        assert!(json.contains(r#""text":"please review""#));

        let parsed: InjectMessageRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.channel, "emf");
        assert_eq!(parsed.thread, "pr-43");
        assert_eq!(parsed.text, "please review");
    }

    #[test]
    fn test_reload_result_serde() {
        let resp = ReloadResult {
            success: true,
            message: "configuration reloaded".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""success":true"#));
        assert!(json.contains(r#""message":"configuration reloaded""#));

        let parsed: ReloadResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.message, "configuration reloaded");
    }

    #[test]
    fn test_reset_session_result_serde() {
        let ok = ResetSessionResult {
            success: true,
            message: "session deleted".to_string(),
        };
        let json = serde_json::to_string(&ok).unwrap();
        let parsed: ResetSessionResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.message, "session deleted");

        let fail = ResetSessionResult {
            success: false,
            message: "missing thread_name param".to_string(),
        };
        let json_fail = serde_json::to_string(&fail).unwrap();
        let parsed_fail: ResetSessionResult = serde_json::from_str(&json_fail).unwrap();
        assert!(!parsed_fail.success);
        assert_eq!(parsed_fail.message, "missing thread_name param");
    }

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
            threads: vec![ThreadInfo {
                name: "issue-42".to_string(),
                channel: "emf".to_string(),
                pattern: Some("planner".to_string()),
                status: ThreadStatus::Processing,
                model: Some("anthropic/claude-opus-4-6".to_string()),
                mode: Some("build".to_string()),
                input_tokens: Some(45000),
                max_tokens: Some(120000),
                activity: vec![],
                last_active_at: None,
                skills: vec!["coding-principles".to_string(), "dev-workflow".to_string()],
                recent_messages: vec![],
                thinking_text: None,
                thread_path: None,
            }],
            stats: GlobalStats {
                active_workers: 2,
                total_threads: 3,
                max_concurrent: 3,
                available_workers: 1,
                messages_received: 156,
                messages_processed: 150,
                errors: 2,
            },
            commands: vec![],
            models: vec![],
        };

        let json = serde_json::to_string(&state).unwrap();
        let parsed: InspectState = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.uptime_secs, 3600);
        assert_eq!(parsed.channels.len(), 1);
        assert_eq!(parsed.channels[0].name, "emf");
        assert_eq!(parsed.channels[0].active_workers, 2);
        assert_eq!(parsed.channels[0].max_concurrent, 3);
        assert_eq!(parsed.threads.len(), 1);
        assert_eq!(parsed.threads[0].status, ThreadStatus::Processing);
        assert_eq!(parsed.stats.active_workers, 2);
    }

    #[test]
    fn test_thread_status_display() {
        assert_eq!(format!("{}", ThreadStatus::Queued), "Queued");
        assert_eq!(format!("{}", ThreadStatus::Processing), "Processing");
        assert_eq!(format!("{}", ThreadStatus::Idle), "Idle");
        assert_eq!(format!("{}", ThreadStatus::WaitingForAnswer), "Waiting");
        assert_eq!(format!("{}", ThreadStatus::Error), "Error");
    }

    #[test]
    fn test_inspect_state_default() {
        let state = InspectState::default();
        assert_eq!(state.uptime_secs, 0);
        assert!(state.channels.is_empty());
        assert!(state.threads.is_empty());
        assert_eq!(state.stats.active_workers, 0);
    }

    #[test]
    fn test_thread_status_serde() {
        // ThreadStatus serializes to snake_case
        let json = serde_json::to_string(&ThreadStatus::WaitingForAnswer).unwrap();
        assert_eq!(json, r#""waiting_for_answer""#);

        let parsed: ThreadStatus = serde_json::from_str(r#""processing""#).unwrap();
        assert_eq!(parsed, ThreadStatus::Processing);

        let json = serde_json::to_string(&ThreadStatus::Error).unwrap();
        assert_eq!(json, r#""error""#);

        let parsed: ThreadStatus = serde_json::from_str(r#""error""#).unwrap();
        assert_eq!(parsed, ThreadStatus::Error);
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
        // Old JSONL entries have `time` field but no `severity` — should deserialize fine
        let old_json =
            r#"{"time":"12:34:56","text":"Processing started","timestamp":"2025-01-15T12:34:56Z"}"#;
        let entry: ActivityEntry = serde_json::from_str(old_json).unwrap();
        assert_eq!(entry.text, "Processing started");
        assert_eq!(entry.severity, Severity::Info);
        assert_eq!(entry.timestamp.as_deref(), Some("2025-01-15T12:34:56Z"));
    }

    #[test]
    fn test_activity_entry_with_severity() {
        let entry = ActivityEntry {
            text: "Failed (5s)".to_string(),
            timestamp: Some("2025-01-15T12:34:56Z".to_string()),
            severity: Severity::Error,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""severity":"error""#));
        let parsed: ActivityEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.severity, Severity::Error);
    }

    #[test]
    fn test_channel_info_backward_compat() {
        let old_json = r#"{"name":"emf","channel_type":"github"}"#;
        let ch: ChannelInfo = serde_json::from_str(old_json).unwrap();
        assert_eq!(ch.name, "emf");
        assert_eq!(ch.active_workers, 0);
        assert_eq!(ch.max_concurrent, 0);
    }
}
