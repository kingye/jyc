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
        // Prefer the pattern whose name matches the client's thread name.
        // This allows per-thread config like `thread_path` to take effect.
        // Fall back to the first enabled pattern if no name match.
        let topic = &message.topic;
        let pattern = if !topic.is_empty() {
            patterns
                .iter()
                .find(|p| p.enabled && p.name == *topic)
                .or_else(|| patterns.iter().find(|p| p.enabled))
        } else {
            patterns.iter().find(|p| p.enabled)
        }?;

        Some(PatternMatch {
            pattern_name: pattern.name.clone(),
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
    #[serde(rename = "subscribe")]
    Subscribe { thread: String },
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
        ws_stream: tokio_tungstenite::WebSocketStream<jyc_inspect::server::PrependStream>,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let pattern_names: Vec<String> = self.pattern_names();

        let broadcast_rx = self.broadcast_tx.subscribe();
        let channel_name = self.channel_name.clone();
        let on_message = self.on_message.clone();
        let workspace_dir = self.workspace_dir.clone();
        let thread_manager = self.thread_manager.lock().unwrap().clone();

        handle_connection_impl(
            ws_stream,
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
async fn handle_connection_impl<S>(
    ws_stream: tokio_tungstenite::WebSocketStream<S>,
    addr: SocketAddr,
    channel_name: String,
    pattern_names: Vec<String>,
    mut broadcast_rx: broadcast::Receiver<String>,
    on_message: std::sync::Arc<tokio::sync::Mutex<Option<OnMessageCallback>>>,
    workspace_dir: Option<PathBuf>,
    thread_manager: Option<Arc<jyc_core::thread_manager::ThreadManager>>,
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync + 'static,
{
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    tracing::info!(addr = %addr, channel = %channel_name, "WebSocket client connected");

    loop {
        tokio::select! {
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

                if msg.is_close() {
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
                        if let Err(e) = ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(json)).await {
                            tracing::warn!(error = %e, addr = %addr, "Failed to send patterns");
                            break;
                        }
                    }
                    ClientMessage::Subscribe { thread } => {
                        tracing::info!(addr = %addr, thread = %thread, "Client subscribed to thread");

                        // Load and send chat history
                        let history = load_chat_history(&thread, &workspace_dir, &thread_manager).await;
                        if !history.is_empty() {
                            let response = ServerMessage::History {
                                thread: thread.clone(),
                                messages: history,
                            };
                            let json = serde_json::to_string(&response)?;
                            if let Err(e) = ws_tx
                                .send(tokio_tungstenite::tungstenite::Message::Text(json))
                                .await
                            {
                                tracing::warn!(error = %e, addr = %addr, "Failed to send history");
                                break;
                            }
                        }
                    }
                    ClientMessage::CreateThread { thread, path } => {
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
                        if let Err(e) = ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(payload)).await {
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
        }
    }

    let _ = ws_tx
        .send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await;
    tracing::info!(addr = %addr, "WebSocket connection closed");
    Ok(())
}

/// Load recent chat history messages from JSONL files for a thread.
///
/// Reads `chat_history_*.jsonl` files in the thread directory, parses each
/// line, and returns up to `max_messages` most recent entries.
/// Reads from `.jyc/` first (new location), falls back to thread root (legacy).
///
/// Resolves the actual thread directory via ThreadManager for custom
/// `thread_path` configurations. Falls back to `workspace_dir.join(thread)`
/// when no ThreadManager is available or no custom path is configured.
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

    if !thread_dir.exists() {
        return vec![];
    }

    // Use the centralized helper that tries .jyc/ first, then root
    let (mut files, _dir) = jyc_core::chat_log_store::list_chat_history_files(&thread_dir);
    files.sort_by(|a, b| b.cmp(a)); // newest first

    let mut entries = Vec::new();
    for file in files {
        if entries.len() >= max_messages {
            break;
        }
        let content = match std::fs::read_to_string(&file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Parse lines in reverse (most recent first within the file)
        let mut file_entries: Vec<HistoryEntry> = content
            .lines()
            .rev()
            .filter_map(|line| {
                let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
                let msg_type = parsed.get("type")?.as_str()?;
                let content = parsed.get("content")?.as_str()?;
                let sender = match msg_type {
                    "received" => "user",
                    "reply" => "ai",
                    _ => return None,
                };
                Some(HistoryEntry {
                    sender: sender.to_string(),
                    text: content.to_string(),
                    timestamp: parsed
                        .get("ts")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                })
            })
            .collect();
        file_entries.reverse(); // restore chronological order
        entries.splice(0..0, file_entries);
    }

    // Keep only the most recent entries (newest at end)
    if entries.len() > max_messages {
        let drain_count = entries.len() - max_messages;
        entries.drain(0..drain_count);
    }
    entries
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

        // Name match is disabled, so fall back to first enabled
        let result = matcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "fallback");
    }

    #[test]
    fn test_match_message_fallback_when_no_name_match() {
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

        // No name match, falls back to first enabled
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
