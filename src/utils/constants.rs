use std::time::Duration;

// --- Thread Manager ---
pub const DEFAULT_MAX_CONCURRENT_THREADS: usize = 3;
pub const DEFAULT_MAX_QUEUE_SIZE_PER_THREAD: usize = 10;

// --- IMAP Monitor ---
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;
pub const DEFAULT_MAX_RETRIES: usize = 5;
pub const DEFAULT_IMAP_FOLDER: &str = "INBOX";
pub const DEFAULT_IMAP_PORT: u16 = 993;
pub const DEFAULT_AUTH_TIMEOUT_MS: u64 = 30_000;

/// Base delay for exponential backoff on IMAP reconnect
pub const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(5);
/// Maximum delay for exponential backoff
pub const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(300);
/// Threshold for "suspicious jump" in sequence numbers triggering recovery
pub const RECOVERY_JUMP_THRESHOLD: u32 = 50;

// --- SMTP ---
pub const DEFAULT_SMTP_PORT: u16 = 465;
/// Max retries for transient SMTP errors (4xx: 421, 451, 452)
pub const SMTP_MAX_TRANSIENT_RETRIES: u32 = 3;
/// Max retries for connection/timeout/TLS SMTP errors
pub const SMTP_MAX_CONNECTION_RETRIES: u32 = 2;
/// Base delay for SMTP retry backoff (seconds)
pub const SMTP_RETRY_BASE_DELAY_SECS: u64 = 5;
/// Maximum delay for SMTP retry backoff (seconds)
pub const SMTP_RETRY_MAX_DELAY_SECS: u64 = 60;

// --- OpenCode ---
pub const OPENCODE_PORT_RANGE_START: u16 = 49152;
pub const OPENCODE_PORT_RANGE_END: u16 = 49252;
pub const OPENCODE_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
pub const OPENCODE_HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(3);

// --- SSE / Timeout ---
/// Activity-based timeout: silence threshold (default, no tool running)
/// 30 min allows for models with long thinking pauses (e.g., minimax)
pub const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Activity-based timeout: silence threshold when a tool is running
/// 30 min to accommodate models with long thinking pauses
pub const TOOL_ACTIVITY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How often to check for activity timeout
pub const ACTIVITY_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// How often to log progress
pub const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(10);
/// Blocking prompt fallback timeout
pub const BLOCKING_PROMPT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

// --- Heartbeat ---
/// Default interval for heartbeat events (10 minutes)
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Minimum elapsed time before sending first heartbeat (1 minute)
pub const MIN_HEARTBEAT_ELAPSED: Duration = Duration::from_secs(60);
/// Minimum interval between heartbeats (30 seconds) to avoid flooding
pub const MIN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

// --- Context Limits ---
pub const MAX_FILES_IN_CONTEXT: usize = 10;
pub const MAX_BODY_IN_PROMPT: usize = 2000;
pub const MAX_PER_FILE: usize = 400;
pub const MAX_TOTAL_CONTEXT: usize = 2000;
pub const MAX_TOTAL_PROMPT: usize = 6000;
pub const MAX_HISTORY_QUOTE: usize = 6;
/// Max characters per quoted history entry in email replies (truncated with "[truncated]")
pub const MAX_QUOTED_BODY_CHARS: usize = 1024;

// --- Alerting ---
pub const DEFAULT_BATCH_INTERVAL_MINUTES: u64 = 5;
pub const DEFAULT_MAX_ERRORS_PER_BATCH: usize = 50;
pub const DEFAULT_REPLY_TOOL_LOG_TAIL_LINES: usize = 50;
pub const DEFAULT_HEALTH_CHECK_INTERVAL_HOURS: f64 = 24.0;
pub const ALERT_CONTEXT_WINDOW_SIZE: usize = 100;
pub const ALERT_CONTEXT_LINES_PER_ERROR: usize = 10;

// --- Attachments ---
pub const DEFAULT_MAX_ATTACHMENTS_PER_MESSAGE: usize = 10;
pub const DEFAULT_ATTACHMENT_FILENAME_MAX_LEN: usize = 200;

// --- Config ---
pub const DEFAULT_CONFIG_FILENAME: &str = "config.toml";
