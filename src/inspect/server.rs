use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use arc_swap::ArcSwap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::types::AppConfig;
use crate::core::activity_log_store::ActivityLogStore;
use crate::core::metrics::SharedHealthStats;
use crate::core::thread_event::ThreadEvent;
use crate::core::thread_manager::ThreadManager;
use crate::inspect::types::*;

/// Max activity entries kept per thread.
const MAX_ACTIVITY_ENTRIES: usize = 60;

/// Per-thread activity buffer, shared between the activity tracker and the server.
pub type SharedActivityMap = Arc<Mutex<HashMap<String, ThreadActivityState>>>;

/// Per-thread activity state: bounded event log + processing flag.
#[derive(Debug, Default)]
pub struct ThreadActivityState {
    pub entries: VecDeque<ActivityEntry>,
    pub is_processing: bool,
    pub has_error: bool,
    pub last_active_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Shared state accessible by the inspect server.
pub struct InspectContext {
    /// Per-channel thread managers
    pub thread_managers: Vec<Arc<ThreadManager>>,
    /// Channel info (name, type)
    pub channels: Vec<ChannelInfo>,
    /// Shared health stats from MetricsCollector
    pub health_stats: SharedHealthStats,
    /// Per-thread activity logs from SSE events
    pub activity_map: SharedActivityMap,
    /// Max concurrent threads per channel
    pub max_concurrent: usize,
    /// When the monitor started
    pub start_time: Instant,
    /// Path to the config file (for reload)
    pub config_path: Option<PathBuf>,
    /// Swappable application config (for live reload)
    pub config: Option<Arc<ArcSwap<AppConfig>>>,
    /// Per-channel workspace directories (parallel to channels)
    pub workspace_dirs: Vec<PathBuf>,
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
    pub fn new(
        bind_addr: String,
        context: Arc<InspectContext>,
        cancel: CancellationToken,
    ) -> Self {
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
                                if let Err(e) = Self::handle_client(stream, ctx).await {
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
    ) -> anyhow::Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
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

    async fn handle_request(
        request: &InspectRequest,
        context: &InspectContext,
    ) -> InspectResponse {
        match request.method.as_str() {
            "get_state" => {
                let state = Self::build_state(context).await;
                InspectResponse::State(state)
            }
            "reload_config" => Self::handle_reload_config(context).await,
            other => InspectResponse::Error {
                error: format!("unknown method: {other}"),
            },
        }
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

        // Load and validate new config
        let new_config = match crate::config::load_config(config_path) {
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

        let errors = crate::config::validation::validate_config(&new_config);
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

        InspectResponse::ReloadResult {
            success: true,
            message: "configuration reloaded".to_string(),
        }
    }

    async fn build_state(context: &InspectContext) -> InspectState {
        let uptime = context.start_time.elapsed().as_secs();

        // Collect threads from all thread managers
        let mut threads = Vec::new();
        let mut total_threads = 0;
        let mut active_workers = 0;

        for tm in &context.thread_managers {
            let tm_threads = tm.list_threads().await;
            total_threads += tm_threads.len();
            let stats = tm.get_stats().await;
            active_workers += stats.active_workers;
            threads.extend(tm_threads);
        }

        // Merge activity logs and status into threads
        let activity_map = context.activity_map.lock().await;
        for thread in &mut threads {
            if let Some(state) = activity_map.get(&thread.name) {
                thread.activity = state.entries.iter().cloned().collect();
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
        let stats = GlobalStats {
            active_workers,
            total_threads,
            max_concurrent: context.max_concurrent,
            messages_received: health.messages_received,
            messages_processed: health.messages_processed,
            errors: health.errors,
        };
        drop(health);

        InspectState {
            uptime_secs: uptime,
            version: env!("CARGO_PKG_VERSION").to_string(),
            channels: context.channels.clone(),
            threads,
            stats,
        }
    }
}

/// Background task that subscribes to thread event buses and buffers
/// activity entries for the inspect server.
pub struct ActivityTracker;

impl ActivityTracker {
    /// Start tracking activity for all thread managers.
    /// Periodically discovers new threads and subscribes to their event buses.
    /// Persists activity entries to `.jyc/activity.jsonl` per thread.
    /// On startup, loads historical activity from disk.
    pub fn start(
        thread_managers: Vec<Arc<ThreadManager>>,
        activity_map: SharedActivityMap,
        workspace_dirs: Vec<PathBuf>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        assert_eq!(
            thread_managers.len(),
            workspace_dirs.len(),
            "thread_managers and workspace_dirs must have the same length"
        );
        tokio::spawn(async move {
            // Load historical activity from disk for all existing threads
            for (tm_index, tm) in thread_managers.iter().enumerate() {
                let workspace_dir = &workspace_dirs[tm_index];
                let threads = tm.list_threads().await;
                for thread in &threads {
                    let thread_path = workspace_dir.join(&thread.name);
                    if let Ok(entries) = ActivityLogStore::load_recent(&thread_path, MAX_ACTIVITY_ENTRIES) {
                        if !entries.is_empty() {
                            let mut map = activity_map.lock().await;
                            let state = map.entry(thread.name.clone()).or_default();
                            state.entries = entries.into_iter().collect();
                            state.is_processing = false;
                            if let Some(last) = state.entries.back() {
                                if let Some(ref ts) = last.timestamp {
                                    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                                        state.last_active_at = Some(dt.with_timezone(&chrono::Utc));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let mut subscribed: HashSet<String> = HashSet::new();
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Discover new threads and subscribe to their event buses
                        for (tm_index, tm) in thread_managers.iter().enumerate() {
                            let workspace_dir = workspace_dirs.get(tm_index);
                            let threads = tm.list_threads().await;
                            for thread in threads {
                                if subscribed.contains(&thread.name) {
                                    continue;
                                }
                                if let Some(bus) = tm.get_event_bus(&thread.name).await {
                                    if let Ok(mut rx) = bus.subscribe().await {
                                        subscribed.insert(thread.name.clone());
                                        let map = activity_map.clone();
                                        let name = thread.name.clone();
                                        let thread_path = workspace_dir.map(|d| d.join(&name));
                                        let cancel_inner = cancel.clone();
                                        tokio::spawn(async move {
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
                                                                    | ThreadEvent::Heartbeat { .. }
                                                                );
                                                                let is_completed = matches!(
                                                                    &event,
                                                                    ThreadEvent::ProcessingCompleted { .. }
                                                                );
                                                                 let entry = event_to_activity(&event);
                                                                 let is_error = entry.severity == Severity::Error;
                                                                 if let Some(ref path) = thread_path {
                                                                     if let Err(e) = ActivityLogStore::append(path, &entry) {
                                                                         tracing::warn!(error = %e, thread = %name, "Failed to persist activity entry");
                                                                     }
                                                                 }
                                                                 let mut map = map.lock().await;
                                                                 let state = map.entry(name.clone()).or_default();
                                                                 state.entries.push_back(entry);
                                                                 state.last_active_at = Some(event.timestamp());
                                                                 if state.entries.len() > MAX_ACTIVITY_ENTRIES {
                                                                     state.entries.pop_front();
                                                                 }
                                                                 if is_processing {
                                                                     state.is_processing = true;
                                                                     state.has_error = false;
                                                                 } else if is_completed {
                                                                     state.is_processing = false;
                                                                 }
                                                                 if is_error {
                                                                     state.has_error = true;
                                                                 }
                                                            }
                                                            None => break,
                                                        }
                                                    }
                                                    _ = cancel_inner.cancelled() => break,
                                                }
                                            }
                                        });
                                    }
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
        ThreadEvent::ToolStarted { tool_name, input, .. } => {
            match input {
                Some(inp) => format!("Tool: {tool_name} — {inp}"),
                None => format!("Tool: {tool_name} (running)"),
            }
        }
        ThreadEvent::ToolCompleted {
            tool_name,
            success,
            duration_secs,
            output,
            ..
        } => {
            if *success {
                format!("Tool: {tool_name} (done, {duration_secs}s)")
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
        ThreadEvent::Heartbeat {
            elapsed_secs,
            activity,
            ..
        } => {
            format!("Heartbeat: {activity} ({elapsed_secs}s)")
        }
        ThreadEvent::Thinking { text, full_length, .. } => {
            let oneline = text.replace('\n', " ");
            if *full_length > text.len() {
                format!("Thinking: {oneline}...")
            } else {
                format!("Thinking: {oneline}")
            }
        }
        ThreadEvent::SessionStatus { status_type, attempt, message, .. } => {
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
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Mutex;

    fn test_context() -> Arc<InspectContext> {
        Arc::new(InspectContext {
            thread_managers: vec![],
            channels: vec![
                ChannelInfo {
                    name: "emf".to_string(),
                    channel_type: "github".to_string(),
                },
            ],
            health_stats: Arc::new(Mutex::new(
                crate::core::metrics::HealthStats::default(),
            )),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            max_concurrent: 3,
            start_time: Instant::now(),
            config_path: None,
            config: None,
            workspace_dirs: vec![],
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

        let server = InspectServer::new(
            addr.to_string(),
            ctx,
            cancel.clone(),
        );
        let handle = server.start();

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connect and send request
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        writer.write_all(b"{\"method\":\"get_state\"}\n").await.unwrap();
        writer.flush().await.unwrap();

        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        let resp: InspectResponse = serde_json::from_str(&response).unwrap();
        match resp {
            InspectResponse::State(state) => {
                assert_eq!(state.channels.len(), 1);
                assert_eq!(state.channels[0].name, "emf");
                assert_eq!(state.stats.max_concurrent, 3);
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
        assert!(entry.text.contains("ERROR"), "Expected ERROR label, got: {}", entry.text);
        assert!(entry.text.contains("SMTP 535 authentication failed"), "Expected error message, got: {}", entry.text);
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
        assert!(entry.text.contains("ERROR (attempt #3)"), "Expected ERROR with attempt, got: {}", entry.text);
        assert!(entry.text.contains("server overload"), "Expected error message, got: {}", entry.text);
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

        writer.write_all(b"{\"method\":\"unknown\"}\n").await.unwrap();
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
            writer.write_all(b"{\"method\":\"get_state\"}\n").await.unwrap();
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
mode = "opencode"
"#;
        std::fs::write(&config_path, config_toml).unwrap();

        let initial_config = crate::config::load_config(&config_path).unwrap();
        let config_swap = Arc::new(ArcSwap::from_pointee(initial_config));

        let ctx = Arc::new(InspectContext {
            thread_managers: vec![],
            channels: vec![],
            health_stats: Arc::new(Mutex::new(crate::core::metrics::HealthStats::default())),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            max_concurrent: 3,
            start_time: Instant::now(),
            config_path: Some(config_path.clone()),
            config: Some(config_swap.clone()),
            workspace_dirs: vec![],
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

        writer.write_all(b"{\"method\":\"reload_config\"}\n").await.unwrap();
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
        let updated_toml = config_toml.replace("max_concurrent_threads = 5", "max_concurrent_threads = 10");
        std::fs::write(&config_path, updated_toml).unwrap();

        writer.write_all(b"{\"method\":\"reload_config\"}\n").await.unwrap();
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
}
