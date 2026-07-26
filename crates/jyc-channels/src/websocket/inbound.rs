//! WebSocket channel inbound adapter and matcher.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage,
    MessageContent, PatternMatch,
};
use std::sync::Mutex as StdMutex;

/// WebSocket channel-specific pattern matching and thread name derivation.
pub struct WebsocketMatcher {
    channel_name: String,
}

impl WebsocketMatcher {
    /// Create a new websocket matcher.
    pub fn new(channel_name: String) -> Self {
        Self { channel_name }
    }
}

impl ChannelMatcher for WebsocketMatcher {
    fn channel_type(&self) -> &str {
        "websocket"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Use the thread name specified by the client (e.g. from the WebSocket
        // protocol's `thread` field). Fall back to the channel name when empty.
        if message.topic.is_empty() {
            self.channel_name.clone()
        } else {
            message.topic.clone()
        }
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        let topic = &message.topic;

        let pattern_name = if !topic.is_empty() {
            // Prefer the pattern whose name matches the client's thread name.
            // This allows per-thread config like `thread_path` to take effect.
            // If no pattern matches the thread name, treat the thread name itself
            // as the pattern name instead of falling back to an arbitrary enabled
            // pattern, which would leak the wrong pattern to ad-hoc threads.
            // If the channel has no enabled patterns at all, the channel is
            // effectively inactive and we return None.
            patterns
                .iter()
                .find(|p| p.enabled && p.name == *topic)
                .map(|p| p.name.clone())
                .or_else(|| {
                    if patterns.iter().any(|p| p.enabled) {
                        Some(topic.clone())
                    } else {
                        None
                    }
                })?
        } else {
            // For empty topic, fall back to the first enabled pattern.
            patterns.iter().find(|p| p.enabled)?.name.clone()
        };

        Some(PatternMatch {
            pattern_name,
            channel: "websocket".to_string(),
            matches: HashMap::new(),
        })
    }
}

/// Client-bound JSON protocol messages.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "patterns")]
    Patterns { patterns: Vec<String> },
    #[serde(rename = "history")]
    History {
        thread: String,
        messages: Vec<HistoryEntry>,
    },
    /// Initial activity batch delivered right after `subscribe`. The entries
    /// are read from the thread's `ThreadEventBus` buffer so the client sees
    /// recent activity events even if they fired before the WS connected.
    #[serde(rename = "activity")]
    Activity {
        thread: String,
        entries: Vec<HistoryEntry>,
    },
    /// Streamed AI reasoning (`ThreadEvent::Thinking`). Pushed live as the
    /// model emits thinking chunks. The client's `thinking_text` UI replaces
    /// its content on each message.
    #[serde(rename = "thinking")]
    Thinking {
        thread: String,
        text: String,
    },
    /// Tool start/complete events (`ThreadEvent::ToolStarted` /
    /// `ThreadEvent::ToolCompleted`). The client appends these to the
    /// activity log.
    #[serde(rename = "tool")]
    Tool {
        thread: String,
        kind: String, // "started" | "completed"
        text: String,
    },
    /// Processing start/complete events (`ThreadEvent::ProcessingStarted`
    /// / `ProcessingCompleted`). The client updates the processing banner.
    #[serde(rename = "process")]
    Process {
        thread: String,
        kind: String, // "started" | "completed" | "failed"
        duration_secs: f64,
    },
    /// Live chat message (`ThreadEvent::IncomingMessage` /
    /// `ThreadEvent::ReplySent`). Replaces the legacy `"reply"` broadcast.
    /// `sender` is `"user"` for inbound, `"ai"` for replies.
    #[serde(rename = "chat")]
    Chat {
        thread: String,
        sender: String,
        text: String,
    },
}

/// A single entry in chat history.
#[derive(Debug, Clone, serde::Serialize)]
struct HistoryEntry {
    sender: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<String>,
}

/// Inbound JSON protocol messages from clients.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "list_patterns")]
    ListPatterns,
    /// Subscribe to a thread.
    ///
    /// `mode` controls the initial sync bundle:
    /// - `"chat"` (default): send `history` + `activity` + start streaming all
    ///   events (thinking, tool, process, chat).
    /// - `"activity"`: only send `activity` and start streaming tool/process
    ///   events (no chat content).
    ///
    /// `mode` defaults to `"chat"` when omitted, preserving backward compat
    /// with existing WS clients that only send `{type:"subscribe", thread}`.
    #[serde(rename = "subscribe")]
    Subscribe {
        thread: String,
        #[serde(default)]
        mode: Option<String>,
    },
    #[serde(rename = "create_thread")]
    CreateThread {
        thread: String,
        path: Option<String>,
    },
    #[serde(rename = "message")]
    Message { thread: String, text: String },
}

/// WebSocket inbound adapter.
///
type OnMessageCallback = Box<dyn Fn(InboundMessage) -> Result<()> + Send + Sync>;

/// Does NOT run its own TCP listener. Instead, it implements
/// `jyc_inspect::server::WebsocketHandler` and is registered with the inspect
/// server, which shares the same port for both JSON queries and WebSocket
/// upgrades.
pub struct WebsocketInboundAdapter {
    channel_name: String,
    /// Live application config for dynamic pattern reading.
    app_config: Option<Arc<ArcSwap<jyc_types::AppConfig>>>,
    /// Broadcast sender — cloned for each new connection via `subscribe()`.
    broadcast_tx: broadcast::Sender<String>,
    /// Message callback — set during `start()`, used by the WebSocket handler.
    on_message: std::sync::Arc<tokio::sync::Mutex<Option<OnMessageCallback>>>,
    /// Workspace directory for loading chat history (default location).
    workspace_dir: Option<PathBuf>,
    /// ThreadManager reference for resolving custom thread_path overrides.
    thread_manager: Arc<StdMutex<Option<Arc<jyc_core::thread_manager::ThreadManager>>>>,
}

impl WebsocketInboundAdapter {
    /// Create a new websocket inbound adapter.
    pub fn new(
        channel_name: String,
        app_config: Option<Arc<ArcSwap<jyc_types::AppConfig>>>,
        broadcast_tx: broadcast::Sender<String>,
    ) -> Self {
        Self {
            channel_name,
            app_config,
            broadcast_tx,
            on_message: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            workspace_dir: None,
            thread_manager: Arc::new(StdMutex::new(None)),
        }
    }

    /// Set the workspace directory for loading chat history.
    pub fn set_workspace_dir(&mut self, dir: PathBuf) {
        self.workspace_dir = Some(dir);
    }

    /// Set the ThreadManager for resolving custom `thread_path` overrides.
    pub fn set_thread_manager(&self, tm: Arc<jyc_core::thread_manager::ThreadManager>) {
        *self.thread_manager.lock().unwrap() = Some(tm);
    }

    /// Read the current enabled pattern names for this channel from the live config.
    fn pattern_names(&self) -> Vec<String> {
        match &self.app_config {
            Some(cfg) => {
                let cfg = cfg.load();
                cfg.channels
                    .get(&self.channel_name)
                    .and_then(|c| c.patterns.as_ref())
                    .map(|p| {
                        p.iter()
                            .filter(|pat| pat.enabled)
                            .map(|pat| pat.name.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            }
            None => Vec::new(),
        }
    }

    /// Return the channel name for this adapter.
    /// Used by the inspect server for path-based handler routing.
    pub fn channel_name(&self) -> &str {
        &self.channel_name
    }
}

#[async_trait::async_trait]
impl jyc_inspect::server::WebsocketHandler for WebsocketInboundAdapter {
    async fn handle(
        &self,
        socket: axum::extract::ws::WebSocket,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let pattern_names: Vec<String> = self.pattern_names();

        let broadcast_rx = self.broadcast_tx.subscribe();
        let channel_name = self.channel_name.clone();
        let on_message = self.on_message.clone();
        let workspace_dir = self.workspace_dir.clone();
        let thread_manager = self.thread_manager.lock().unwrap().clone();

        handle_connection_impl(
            socket,
            addr,
            channel_name,
            pattern_names,
            broadcast_rx,
            on_message,
            workspace_dir,
            thread_manager,
        )
        .await
    }
}

impl ChannelMatcher for WebsocketInboundAdapter {
    fn channel_type(&self) -> &str {
        "websocket"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        WebsocketMatcher::new(self.channel_name.clone()).derive_thread_name(
            message,
            patterns,
            pattern_match,
        )
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        WebsocketMatcher::new(self.channel_name.clone()).match_message(message, patterns)
    }
}

#[async_trait]
impl InboundAdapter for WebsocketInboundAdapter {
    async fn start(
        &self,
        options: InboundAdapterOptions,
        _cancel: CancellationToken,
    ) -> Result<()> {
        // Store the on_message callback so the WebsocketHandler can use it.
        let mut guard = self.on_message.lock().await;
        *guard = Some(options.on_message);
        tracing::info!(channel = %self.channel_name, "WebSocket inbound adapter registered (no independent listener)");
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection_impl(
    socket: axum::extract::ws::WebSocket,
    addr: SocketAddr,
    channel_name: String,
    pattern_names: Vec<String>,
    mut broadcast_rx: broadcast::Receiver<String>,
    on_message: std::sync::Arc<tokio::sync::Mutex<Option<OnMessageCallback>>>,
    workspace_dir: Option<PathBuf>,
    thread_manager: Option<Arc<jyc_core::thread_manager::ThreadManager>>,
) -> anyhow::Result<()> {
    let (mut ws_tx, mut ws_rx) = socket.split();

    tracing::info!(addr = %addr, channel = %channel_name, "WebSocket client connected");

    // After subscribe, this holds the receiver for the thread's event bus.
    // `None` until the client subscribes; select-branch below polls it
    // as `pending()` when `None`.
    let mut event_rx: Option<tokio::sync::mpsc::Receiver<jyc_core::thread_event::ThreadEvent>> =
        None;

    loop {
        // Conditional select branch for the event bus. When `event_rx` is
        // `None`, this future never resolves so it doesn't win the race.
        let event_branch = async {
            match event_rx.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending::<Option<jyc_core::thread_event::ThreadEvent>>().await,
            }
        };

        tokio::select! {
            biased;
            msg = ws_rx.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, addr = %addr, "WebSocket receive error");
                        break;
                    }
                    None => {
                        tracing::info!(addr = %addr, "WebSocket client disconnected");
                        break;
                    }
                };

                if matches!(msg, axum::extract::ws::Message::Close(_)) {
                    tracing::info!(addr = %addr, "WebSocket client closed connection");
                    break;
                }

                let text = match msg.to_text() {
                    Ok(t) => t,
                    Err(_) => continue, // ignore binary frames
                };

                let client_msg: ClientMessage = match serde_json::from_str(text) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, text = %text, "Invalid WebSocket message");
                        continue;
                    }
                };

                match client_msg {
                    ClientMessage::ListPatterns => {
                        let response = ServerMessage::Patterns {
                            patterns: pattern_names.clone(),
                        };
                        let json = serde_json::to_string(&response)?;
                        if let Err(e) = ws_tx.send(axum::extract::ws::Message::Text(json)).await {
                            tracing::warn!(error = %e, addr = %addr, "Failed to send patterns");
                            break;
                        }
                    }
                    ClientMessage::Subscribe { thread, mode } => {
                        let sub_mode = mode.as_deref().unwrap_or("chat").to_string();
                        tracing::info!(
                            addr = %addr,
                            thread = %thread,
                            mode = %sub_mode,
                            "Client subscribed to thread"
                        );

                        // Send chat history (only for chat mode)
                        if sub_mode == "chat" {
                            let history = load_chat_history(
                                &thread,
                                &workspace_dir,
                                &thread_manager,
                            )
                            .await;
                            if !history.is_empty() {
                                let response = ServerMessage::History {
                                    thread: thread.clone(),
                                    messages: history,
                                };
                                let json = serde_json::to_string(&response)?;
                                if let Err(e) = ws_tx
                                    .send(axum::extract::ws::Message::Text(json))
                                    .await
                                {
                                    tracing::warn!(error = %e, addr = %addr, "Failed to send history");
                                    break;
                                }
                            }
                        }

                        // Subscribe to the thread's event bus. The bus replays
                        // buffered events to the new subscriber, providing the
                        // initial `Activity` batch automatically. The bus may
                        // not exist yet if the thread is offline / never had
                        // a worker; in that case we just don't forward events.
                        if let Some(tm) = &thread_manager {
                            if let Some(bus) =
                                tm.get_or_create_event_bus(&thread).await
                            {
                                match bus.subscribe().await {
                                    Ok(rx) => {
                                        event_rx = Some(rx);
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            thread = %thread,
                                            "Failed to subscribe to event bus"
                                        );
                                    }
                                }
                            } else {
                                tracing::debug!(
                                    thread = %thread,
                                    "Event bus unavailable for thread (events disabled or offline)"
                                );
                            }
                        }
                    }
                    ClientMessage::CreateThread { thread, path } => {
                        tracing::info!(
                            addr = %addr,
                            thread = %thread,
                            path = ?path,
                            "WebSocket create_thread"
                        );

                        let mut message = InboundMessage {
                            id: uuid::Uuid::new_v4().to_string(),
                            channel: channel_name.clone(),
                            channel_uid: "websocket".to_string(),
                            sender: "user".to_string(),
                            sender_address: addr.to_string(),
                            recipients: vec![],
                            topic: thread.clone(),
                            content: MessageContent {
                                text: Some(String::new()),
                                html: None,
                                markdown: None,
                            },
                            timestamp: chrono::Utc::now(),
                            thread_refs: None,
                            reply_to_id: None,
                            external_id: None,
                            attachments: vec![],
                            metadata: HashMap::new(),
                            matched_pattern: None,
                        };
                        if let Some(p) = path {
                            message
                                .metadata
                                .insert("thread_path_override".to_string(), serde_json::json!(p));
                        }

                        let guard = on_message.lock().await;
                        if let Some(ref callback) = *guard {
                            if let Err(e) = (callback)(message) {
                                tracing::error!(error = %e, "WebSocket on_message error");
                            }
                        } else {
                            tracing::warn!("WebSocket on_message callback not set — create_thread dropped");
                        }
                    }
                    ClientMessage::Message { thread, text } => {
                        let message = InboundMessage {
                            id: uuid::Uuid::new_v4().to_string(),
                            channel: channel_name.clone(),
                            channel_uid: "websocket".to_string(),
                            sender: "user".to_string(),
                            sender_address: addr.to_string(),
                            recipients: vec![],
                            topic: thread.clone(),
                            content: MessageContent {
                                text: Some(text),
                                html: None,
                                markdown: None,
                            },
                            timestamp: chrono::Utc::now(),
                            thread_refs: None,
                            reply_to_id: None,
                            external_id: None,
                            attachments: vec![],
                            metadata: HashMap::new(),
                            matched_pattern: None,
                        };

                        let guard = on_message.lock().await;
                        if let Some(ref callback) = *guard {
                            if let Err(e) = (callback)(message) {
                                tracing::error!(error = %e, "WebSocket on_message error");
                            }
                        } else {
                            tracing::warn!("WebSocket on_message callback not set — message dropped");
                        }
                    }
                }
            }
            broadcast = broadcast_rx.recv() => {
                match broadcast {
                    Ok(payload) => {
                        // Legacy `reply` broadcast: keep emitting for backward
                        // compat with existing WS clients that listen for
                        // `{type:"reply", ...}`.
                        if let Err(e) = ws_tx.send(axum::extract::ws::Message::Text(payload)).await {
                            tracing::warn!(error = %e, addr = %addr, "Failed to send broadcast");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!(addr = %addr, "Broadcast channel closed");
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Client is slow, just continue
                    }
                }
            }
            event = event_branch => {
                let Some(event) = event else { break; };
                if let Some(msg) = thread_event_to_server_message(event) {
                    let json = match serde_json::to_string(&msg) {
                        Ok(j) => j,
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to serialize WS event");
                            continue;
                        }
                    };
                    if let Err(e) = ws_tx.send(axum::extract::ws::Message::Text(json)).await {
                        tracing::warn!(error = %e, addr = %addr, "Failed to send WS event");
                        break;
                    }
                }
            }
        }
    }

    let _ = ws_tx.send(axum::extract::ws::Message::Close(None)).await;
    tracing::info!(addr = %addr, "WebSocket connection closed");
    Ok(())
}

/// Convert a `ThreadEvent` from the bus into a `ServerMessage` for WS push.
///
/// Returns `None` for events that don't translate to a client-facing message
/// (e.g. `SessionStatus`, `LLMRequestStarted`, `ProcessingProgress`).
fn thread_event_to_server_message(
    event: jyc_core::thread_event::ThreadEvent,
) -> Option<ServerMessage> {
    use jyc_core::thread_event::ThreadEvent;
    match event {
        ThreadEvent::Thinking { thread_name, text, .. } => Some(ServerMessage::Thinking {
            thread: thread_name,
            text,
        }),
        ThreadEvent::ToolStarted {
            thread_name,
            tool_name,
            input,
            timestamp,
            ..
        } => Some(ServerMessage::Tool {
            thread: thread_name,
            kind: "started".to_string(),
            text: format_tool_text(&tool_name, input.as_deref(), Some(&timestamp)),
        }),
        ThreadEvent::ToolCompleted {
            thread_name,
            tool_name,
            success,
            output,
            input,
            timestamp,
            duration_secs,
            ..
        } => Some(ServerMessage::Tool {
            thread: thread_name,
            kind: "completed".to_string(),
            text: format_tool_completed(
                &tool_name,
                success,
                output.as_deref(),
                input.as_deref(),
                Some(&timestamp),
                duration_secs,
            ),
        }),
        ThreadEvent::ProcessingStarted { thread_name, .. } => Some(ServerMessage::Process {
            thread: thread_name,
            kind: "started".to_string(),
            duration_secs: 0.0,
        }),
        ThreadEvent::ProcessingCompleted {
            thread_name,
            success,
            duration_secs,
            ..
        } => Some(ServerMessage::Process {
            thread: thread_name,
            kind: if success { "completed".to_string() } else { "failed".to_string() },
            duration_secs: duration_secs as f64,
        }),
        ThreadEvent::IncomingMessage {
            thread_name,
            sender,
            text,
            ..
        } => Some(ServerMessage::Chat {
            thread: thread_name,
            sender,
            text,
        }),
        ThreadEvent::ReplySent {
            thread_name,
            text,
            ..
        } => Some(ServerMessage::Chat {
            thread: thread_name,
            sender: "ai".to_string(),
            text,
        }),
        // Skip events that don't have a clean client-facing representation.
        ThreadEvent::ProcessingProgress { .. }
        | ThreadEvent::LLMRequestStarted { .. }
        | ThreadEvent::SessionStatus { .. } => None,
    }
}

/// Format a tool start event as a short human-readable label.
fn format_tool_text(
    tool_name: &str,
    input: Option<&str>,
    _timestamp: Option<&chrono::DateTime<chrono::Utc>>,
) -> String {
    // Truncate the input summary to keep WS messages small. The full diff
    // lives in the on-disk activity.jsonl for the TUI to inspect.
    match input {
        Some(s) if s.len() > 80 => format!("{}: {}...", tool_name, &s[..80]),
        Some(s) => format!("{}: {}", tool_name, s),
        None => format!("{}: (running)", tool_name),
    }
}

/// Format a tool completion event as a short human-readable label.
fn format_tool_completed(
    tool_name: &str,
    success: bool,
    output: Option<&str>,
    input: Option<&str>,
    _timestamp: Option<&chrono::DateTime<chrono::Utc>>,
    duration_secs: u64,
) -> String {
    let status = if success { "done" } else { "FAILED" };
    let prefix = format!("{} ({} {}s)", tool_name, status, duration_secs);
    match output {
        Some(s) if !s.is_empty() => format!("{}: {}", prefix, truncate(s, 80)),
        _ => match input {
            Some(s) => format!("{}: {}", prefix, truncate(s, 60)),
            None => prefix,
        },
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
}

/// Load recent chat history messages from JSONL files for a thread.
///
/// Resolves the actual thread directory via ThreadManager for custom
/// `thread_path` configurations. Falls back to `workspace_dir.join(thread)`
/// when no ThreadManager is available or no custom path is configured.
/// Uses the shared `jyc_core::chat_log_store::load_recent_chat_history` for
/// the actual file parsing.
async fn load_chat_history(
    thread: &str,
    workspace_dir: &Option<PathBuf>,
    thread_manager: &Option<Arc<jyc_core::thread_manager::ThreadManager>>,
) -> Vec<HistoryEntry> {
    let max_messages = 100;

    // Resolve the actual thread directory path
    let thread_dir = if let Some(tm) = thread_manager {
        tm.thread_path(thread).await.unwrap_or_else(|| {
            workspace_dir
                .as_ref()
                .map(|d| d.join(thread))
                .unwrap_or_default()
        })
    } else {
        match workspace_dir {
            Some(dir) => dir.join(thread),
            None => return vec![],
        }
    };

    jyc_core::chat_log_store::load_recent_chat_history(&thread_dir, max_messages)
        .into_iter()
        .map(|e| HistoryEntry {
            sender: e.sender,
            text: e.text,
            timestamp: e.timestamp,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_message() -> InboundMessage {
        InboundMessage {
            id: "test".to_string(),
            channel: "websocket".to_string(),
            channel_uid: "user".to_string(),
            sender: "user".to_string(),
            sender_address: "user".to_string(),
            recipients: vec![],
            topic: "Test".to_string(),
            content: MessageContent {
                text: Some("hello".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        }
    }

    #[test]
    fn test_derive_thread_name_uses_topic() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let msg = create_test_message();
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "Test");
    }

    #[test]
    fn test_derive_thread_name_empty_topic_fallback() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let mut msg = create_test_message();
        msg.topic = String::new();
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "my-ws");
    }

    #[test]
    fn test_match_message_by_topic_name() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let mut msg = create_test_message();
        msg.topic = "my-project".to_string();

        let patterns = vec![
            ChannelPattern {
                name: "default".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
            ChannelPattern {
                name: "my-project".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
        ];

        // Should match "my-project" by name, not "default" (first enabled)
        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "my-project");
    }

    #[test]
    fn test_match_message_by_topic_name_skips_disabled() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let mut msg = create_test_message();
        msg.topic = "my-project".to_string();

        let patterns = vec![
            ChannelPattern {
                name: "my-project".to_string(),
                channel: "websocket".to_string(),
                enabled: false,
                ..Default::default()
            },
            ChannelPattern {
                name: "fallback".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
        ];

        // Name match is disabled, so use the topic itself as the pattern name
        // rather than falling back to an arbitrary enabled pattern.
        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "my-project");
    }

    #[test]
    fn test_match_message_uses_topic_when_no_name_match() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let msg = create_test_message(); // topic = "Test", no pattern named "Test"

        let patterns = vec![
            ChannelPattern {
                name: "p1".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
            ChannelPattern {
                name: "p2".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
        ];

        // No name match, use the topic itself as the pattern name so ad-hoc
        // threads do not inherit an unrelated pattern.
        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "Test");
    }

    #[test]
    fn test_match_message_adhoc_topic_does_not_inherit_other_pattern() {
        let matcher = WebsocketMatcher::new("local_dev".to_string());
        let mut msg = create_test_message();
        msg.topic = "adhoc".to_string();

        let patterns = vec![
            ChannelPattern {
                name: "jin".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
            ChannelPattern {
                name: "jyc".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
        ];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "adhoc");
    }

    #[test]
    fn test_match_message_empty_topic_falls_back_to_first_enabled() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let mut msg = create_test_message();
        msg.topic = String::new();

        let patterns = vec![
            ChannelPattern {
                name: "p1".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
            ChannelPattern {
                name: "p2".to_string(),
                channel: "websocket".to_string(),
                enabled: true,
                ..Default::default()
            },
        ];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "p1");
    }

    #[test]
    fn test_match_message_none_when_all_disabled() {
        let matcher = WebsocketMatcher::new("my-ws".to_string());
        let msg = create_test_message();

        let patterns = vec![ChannelPattern {
            name: "p1".to_string(),
            channel: "websocket".to_string(),
            enabled: false,
            ..Default::default()
        }];

        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_load_chat_history_with_workspace_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let thread_dir = tmp.path().join("my-thread");
        tokio::fs::create_dir_all(thread_dir.join(".jyc"))
            .await
            .unwrap();
        // Write a chat history file in the new .jyc location
        tokio::fs::write(
            thread_dir.join(".jyc").join("chat_history_2026-06-30.jsonl"),
            r#"{"ts":"2026-06-30T10:00:00Z","type":"received","matched":true,"sender":"user","channel":"test","topic":"test","from":"user","content":"hello"}"#,
        )
        .await
        .unwrap();

        let history = load_chat_history("my-thread", &Some(tmp.path().to_path_buf()), &None).await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text, "hello");
    }

    #[tokio::test]
    async fn test_load_chat_history_returns_empty_when_no_dir() {
        let history = load_chat_history("nonexistent", &None, &None).await;
        assert!(history.is_empty());
    }

    #[test]
    fn create_thread_message_deserializes() {
        let json = r#"{"type":"create_thread","thread":"my-thread","path":"/tmp/foo"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            ClientMessage::CreateThread { thread, path } => {
                assert_eq!(thread, "my-thread");
                assert_eq!(path, Some("/tmp/foo".to_string()));
            }
            _ => panic!("expected CreateThread variant"),
        }
    }

    #[test]
    fn create_thread_message_without_path_deserializes() {
        let json = r#"{"type":"create_thread","thread":"my-thread"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        match msg {
            ClientMessage::CreateThread { thread, path } => {
                assert_eq!(thread, "my-thread");
                assert_eq!(path, None);
            }
            _ => panic!("expected CreateThread variant"),
        }
    }
}
