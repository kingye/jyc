use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use jyc_types::{
    ActivityEntry, ChatMessageEntry, InspectOverview, InspectRequest, InspectResponse, InspectState,
};

/// Client for connecting to the jyc inspect server.
///
/// Maintains a persistent TCP connection and reuses it across polls.
/// Automatically reconnects if the connection drops.
pub struct InspectClient {
    addr: String,
    conn: Option<Connection>,
}

struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl InspectClient {
    pub fn new(addr: &str) -> Self {
        Self {
            addr: addr.to_string(),
            conn: None,
        }
    }

    /// Fetch the current full state (with activity, recent_messages, thinking_text).
    /// Retained for backward compatibility — new clients should prefer `get_overview`
    /// for the polling loop and `get_thread_activity` / `get_thread_chat` for
    /// per-thread fetches.
    pub async fn get_state(&mut self) -> Result<InspectState> {
        let resp = self.send_request("get_state", None).await?;
        match resp {
            InspectResponse::State(state) => Ok(state),
            InspectResponse::Error { error } => anyhow::bail!("server error: {error}"),
            other => Err(unexpected("get_state", &other)),
        }
    }

    /// Fetch the slim overview payload (thread list + status, no activity/messages).
    /// Used by the dashboard's polling loop to keep payloads small.
    pub async fn get_overview(&mut self) -> Result<InspectOverview> {
        let resp = self.send_request("get_state_overview", None).await?;
        match resp {
            InspectResponse::Overview(overview) => Ok(overview),
            InspectResponse::Error { error } => anyhow::bail!("server error: {error}"),
            other => Err(unexpected("get_state_overview", &other)),
        }
    }

    /// Fetch recent activity entries for a single thread from `.jyc/activity.jsonl`.
    ///
    /// - `since`: optional RFC 3339 timestamp; only entries with timestamp >= since are returned.
    /// - `limit`: maximum number of entries to return (default 180).
    pub async fn get_thread_activity(
        &mut self,
        channel: &str,
        thread: &str,
        since: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<ActivityEntry>> {
        let params = serde_json::json!({
            "channel": channel,
            "thread": thread,
            "since": since,
            "limit": limit,
        });
        let resp = self
            .send_request("get_thread_activity", Some(params))
            .await?;
        match resp {
            InspectResponse::ActivityHistory { entries } => Ok(entries),
            InspectResponse::Error { error } => Err(anyhow::anyhow!("server error: {error}")),
            other => Err(unexpected("get_thread_activity", &other)),
        }
    }

    /// Fetch recent chat messages for a single thread from `chat_history_*.jsonl`.
    ///
    /// - `since`: optional RFC 3339 timestamp; only entries with timestamp >= since are returned.
    /// - `limit`: maximum number of entries to return (default 100).
    pub async fn get_thread_chat(
        &mut self,
        channel: &str,
        thread: &str,
        since: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<ChatMessageEntry>> {
        let params = serde_json::json!({
            "channel": channel,
            "thread": thread,
            "since": since,
            "limit": limit,
        });
        let resp = self.send_request("get_thread_chat", Some(params)).await?;
        match resp {
            InspectResponse::ChatHistory { entries } => Ok(entries),
            InspectResponse::Error { error } => Err(anyhow::anyhow!("server error: {error}")),
            other => Err(unexpected("get_thread_chat", &other)),
        }
    }

    /// Send a `reload_config` command to the inspect server.
    pub async fn reload_config(&mut self) -> Result<(bool, String)> {
        let resp = self.send_request("reload_config", None).await?;
        match resp {
            InspectResponse::ReloadResult { success, message } => Ok((success, message)),
            InspectResponse::Error { error } => Ok((false, error)),
            other => Err(unexpected("reload_config", &other)),
        }
    }

    /// Send a `reset_session` command to the inspect server.
    pub async fn reset_session(&mut self, thread_name: &str) -> Result<(bool, String)> {
        let params = serde_json::json!({ "thread_name": thread_name });
        let resp = self.send_request("reset_session", Some(params)).await?;
        match resp {
            InspectResponse::ResetSessionResult { success, message } => Ok((success, message)),
            InspectResponse::Error { error } => Ok((false, error)),
            other => Err(unexpected("reset_session", &other)),
        }
    }

    /// Inject a message into a thread for AI processing.
    ///
    /// The server creates a synthetic `InboundMessage` and enqueues it via
    /// `ThreadManager::enqueue()`, following the same path as cross-thread
    /// message injection from the `jyc_send_to_thread` tool.
    pub async fn inject_message(
        &mut self,
        channel: &str,
        thread: &str,
        text: &str,
    ) -> Result<(bool, String)> {
        let params = serde_json::json!({
            "channel": channel,
            "thread": thread,
            "text": text,
        });
        let resp = self.send_request("inject_message", Some(params)).await?;
        match resp {
            InspectResponse::InjectMessageResult { success, message } => Ok((success, message)),
            InspectResponse::Error { error } => Ok((false, error)),
            other => Err(unexpected("inject_message", &other)),
        }
    }

    /// Send a request and return the raw response. Reuses the persistent connection.
    async fn send_request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<InspectResponse> {
        // Try on existing connection first
        if let Some(conn) = self.conn.as_mut() {
            match Self::write_and_read(conn, method, params.clone()).await {
                Ok(resp) => return Ok(resp),
                Err(_) => {
                    // Connection broken, drop and reconnect
                    self.conn = None;
                }
            }
        }

        // Connect (or reconnect)
        self.connect().await?;
        let conn = self.conn.as_mut().context("not connected")?;
        Self::write_and_read(conn, method, params).await
    }

    async fn connect(&mut self) -> Result<()> {
        let stream = TcpStream::connect(&self.addr)
            .await
            .with_context(|| format!("failed to connect to inspect server at {}", self.addr))?;

        let (reader, writer) = stream.into_split();
        self.conn = Some(Connection {
            reader: BufReader::new(reader),
            writer,
        });
        Ok(())
    }

    async fn write_and_read(
        conn: &mut Connection,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<InspectResponse> {
        let request = InspectRequest {
            method: method.to_string(),
            params,
        };
        let mut json = serde_json::to_string(&request)?;
        json.push('\n');
        conn.writer.write_all(json.as_bytes()).await?;
        conn.writer.flush().await?;

        let mut response_line = String::new();
        let bytes = conn
            .reader
            .read_line(&mut response_line)
            .await
            .context("failed to read response")?;

        if bytes == 0 {
            anyhow::bail!("server closed connection");
        }

        serde_json::from_str(response_line.trim()).context("failed to parse inspect response")
    }
}

/// Build a descriptive error message for an unexpected response variant.
fn unexpected(method: &str, resp: &InspectResponse) -> anyhow::Error {
    let variant = match resp {
        InspectResponse::State(_) => "state",
        InspectResponse::Overview(_) => "overview",
        InspectResponse::Error { .. } => "error",
        InspectResponse::ReloadResult { .. } => "reload_result",
        InspectResponse::ResetSessionResult { .. } => "reset_session_result",
        InspectResponse::InjectMessageResult { .. } => "inject_message_result",
        InspectResponse::ActivityHistory { .. } => "activity_history",
        InspectResponse::ChatHistory { .. } => "chat_history",
    };
    anyhow::anyhow!("unexpected {variant} response for {method}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    use crate::server::{InspectContext, InspectServer};
    use arc_swap::ArcSwap;
    use jyc_types::ChannelInfo;

    fn test_context() -> Arc<InspectContext> {
        Arc::new(InspectContext {
            thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
            channels: Arc::new(ArcSwap::from_pointee(vec![ChannelInfo {
                name: "test-ch".to_string(),
                channel_type: "email".to_string(),
                active_workers: 0,
                max_concurrent: 0,
            }])),
            health_stats: Arc::new(Mutex::new(jyc_core::metrics::HealthStats::default())),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            start_time: Instant::now(),
            config_path: None,
            global_config_path: None,
            config: None,
            workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![])),
            websocket_handlers: None,
            reload_callback: None,
            inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(256).0),
        })
    }

    #[tokio::test]
    async fn test_inspect_client_get_state() {
        let cancel = CancellationToken::new();
        let context = test_context();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), context, cancel.clone());
        let _handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = InspectClient::new(&addr.to_string());
        let state = client.get_state().await.unwrap();

        assert_eq!(state.channels.len(), 1);
        assert_eq!(state.channels[0].name, "test-ch");
        assert_eq!(state.stats.max_concurrent, 0);

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_inspect_client_get_overview() {
        let cancel = CancellationToken::new();
        let context = test_context();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), context, cancel.clone());
        let _handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = InspectClient::new(&addr.to_string());
        let overview = client.get_overview().await.unwrap();

        assert_eq!(overview.channels.len(), 1);
        assert_eq!(overview.threads.len(), 0);
        assert_eq!(overview.stats.max_concurrent, 0);

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_inspect_client_reuses_connection() {
        let cancel = CancellationToken::new();
        let context = test_context();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), context, cancel.clone());
        let _handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = InspectClient::new(&addr.to_string());

        // Multiple requests should reuse the same connection
        for _ in 0..5 {
            let state = client.get_state().await.unwrap();
            assert_eq!(state.channels.len(), 1);
        }

        // Connection should be established
        assert!(client.conn.is_some());

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_inspect_client_reconnects_after_disconnect() {
        let cancel = CancellationToken::new();
        let context = test_context();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), context.clone(), cancel.clone());
        let handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = InspectClient::new(&addr.to_string());
        let state = client.get_state().await.unwrap();
        assert_eq!(state.channels.len(), 1);

        // Kill server
        cancel.cancel();
        handle.await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Connection is broken — drop it so next call reconnects
        client.conn = None;

        // Restart server
        let cancel2 = CancellationToken::new();
        let server2 = InspectServer::new(addr.to_string(), context, cancel2.clone());
        let _handle2 = server2.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Should reconnect automatically
        let state = client.get_state().await.unwrap();
        assert_eq!(state.channels.len(), 1);

        cancel2.cancel();
    }

    #[tokio::test]
    async fn test_inspect_client_connection_refused() {
        let mut client = InspectClient::new("127.0.0.1:1");
        let result = client.get_state().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_inspect_client_reset_session() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().to_path_buf();
        let thread_name = "test-thread";
        let jyc_dir = workspace_dir.join(thread_name).join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("agent-session.json"),
            r#"{"created_at":"2026-01-01","total_input_tokens":100,"total_output_tokens":50,"max_input_tokens":1000}"#,
        )
        .await
        .unwrap();

        let context = Arc::new(InspectContext {
            thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
            channels: Arc::new(ArcSwap::from_pointee(vec![ChannelInfo {
                name: "test-ch".to_string(),
                channel_type: "email".to_string(),
                active_workers: 0,
                max_concurrent: 0,
            }])),
            health_stats: Arc::new(Mutex::new(jyc_core::metrics::HealthStats::default())),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            start_time: Instant::now(),
            config_path: None,
            global_config_path: None,
            config: None,
            workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![workspace_dir])),
            websocket_handlers: None,
            reload_callback: None,
            inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(256).0),
        });

        let cancel = CancellationToken::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), context, cancel.clone());
        let _handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = InspectClient::new(&addr.to_string());
        let (success, message) = client.reset_session("test-thread").await.unwrap();

        assert!(success, "reset should succeed: {message}");
        assert!(message.contains("session deleted"));
        assert!(!jyc_dir.join("agent-session.json").exists());

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_inspect_client_inject_message_no_channel() {
        // Server with no thread managers — inject should fail
        let context = Arc::new(InspectContext {
            thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
            channels: Arc::new(ArcSwap::from_pointee(vec![])),
            health_stats: Arc::new(Mutex::new(jyc_core::metrics::HealthStats::default())),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            start_time: Instant::now(),
            config_path: None,
            global_config_path: None,
            config: None,
            workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![])),
            websocket_handlers: None,
            reload_callback: None,
            inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(256).0),
        });

        let cancel = CancellationToken::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), context, cancel.clone());
        let _handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = InspectClient::new(&addr.to_string());
        let (success, message) = client
            .inject_message("nonexistent", "thread", "hello")
            .await
            .unwrap();

        assert!(!success, "inject should fail for unknown channel");
        assert!(message.contains("no thread manager found"));

        cancel.cancel();
    }

    /// Overview payload should be much smaller than the full state payload
    /// because it strips activity/messages/thinking from each thread.
    #[tokio::test]
    async fn test_overview_payload_is_smaller_than_state() {
        let cancel = CancellationToken::new();
        let context = test_context();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), context, cancel.clone());
        let _handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client = InspectClient::new(&addr.to_string());

        let state_json = serde_json::to_string(&client.get_state().await.unwrap()).unwrap();
        let overview_json = serde_json::to_string(&client.get_overview().await.unwrap()).unwrap();

        // Overview should never be larger than the full state for the same data.
        assert!(
            overview_json.len() <= state_json.len(),
            "overview ({}) should be <= state ({})",
            overview_json.len(),
            state_json.len()
        );

        cancel.cancel();
    }
}
