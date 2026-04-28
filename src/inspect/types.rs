use serde::{Deserialize, Serialize};

// ── Protocol ──

/// Request sent by the dashboard client to the inspect server.
#[derive(Debug, Serialize, Deserialize)]
pub struct InspectRequest {
    pub method: String,
    /// Optional parameters for the method (unused by `get_state`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// Response sent by the inspect server to the dashboard client.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InspectResponse {
    State(InspectState),
    Error {
        error: String,
    },
    /// Result of a `reload_config` request.
    ReloadResult {
        success: bool,
        message: String,
    },
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
}

/// Information about a configured channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// Channel name from config (e.g., "emf", "work")
    pub name: String,
    /// Channel type: "email", "feishu", "github"
    pub channel_type: String,
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
}

/// Severity level for an activity entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Default for Severity {
    fn default() -> Self {
        Self::Info
    }
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

/// Thread processing status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// Waiting for semaphore permit
    Queued,
    /// AI processing active
    Processing,
    /// Worker running, waiting for messages
    Idle,
    /// Question tool waiting for user reply
    WaitingForAnswer,
    /// Thread encountered an error
    Error,
}

impl Default for ThreadStatus {
    fn default() -> Self {
        Self::Idle
    }
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
    /// Total messages received since startup
    pub messages_received: u64,
    /// Total messages processed since startup
    pub messages_processed: u64,
    /// Total errors since startup
    pub errors: u64,
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
    fn test_inspect_request_serialize() {
        let req = InspectRequest {
            method: "get_state".to_string(),
            params: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("get_state"));

        let parsed: InspectRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.method, "get_state");

        // Backward compat: old requests without params still parse
        let old_json = r#"{"method":"get_state"}"#;
        let parsed: InspectRequest = serde_json::from_str(old_json).unwrap();
        assert_eq!(parsed.method, "get_state");
        assert!(parsed.params.is_none());
    }

    #[test]
    fn test_inspect_state_serialize_roundtrip() {
        let state = InspectState {
            uptime_secs: 3600,
            version: "0.1.10".to_string(),
            channels: vec![ChannelInfo {
                name: "emf".to_string(),
                channel_type: "github".to_string(),
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
            }],
            stats: GlobalStats {
                active_workers: 2,
                total_threads: 3,
                max_concurrent: 3,
                messages_received: 156,
                messages_processed: 150,
                errors: 2,
            },
        };

        let json = serde_json::to_string(&state).unwrap();
        let parsed: InspectState = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.uptime_secs, 3600);
        assert_eq!(parsed.channels.len(), 1);
        assert_eq!(parsed.channels[0].name, "emf");
        assert_eq!(parsed.threads.len(), 1);
        assert_eq!(parsed.threads[0].status, ThreadStatus::Processing);
        assert_eq!(parsed.stats.active_workers, 2);
    }

    #[test]
    fn test_inspect_response_error() {
        let resp = InspectResponse::Error {
            error: "unknown method".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("unknown method"));
        assert!(json.contains(r#""type":"error""#));
    }

    #[test]
    fn test_inspect_response_reload_result() {
        let resp = InspectResponse::ReloadResult {
            success: true,
            message: "config reloaded".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"reload_result""#));
        assert!(json.contains("config reloaded"));

        let parsed: InspectResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            InspectResponse::ReloadResult { success, message } => {
                assert!(success);
                assert_eq!(message, "config reloaded");
            }
            _ => panic!("expected ReloadResult"),
        }
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
}
