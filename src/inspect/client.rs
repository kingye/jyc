use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::inspect::types::{InspectRequest, InspectResponse, InspectState};

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

    /// Fetch the current state, reusing the existing connection if possible.
    pub async fn get_state(&mut self) -> Result<InspectState> {
        // Try on existing connection first
        if self.conn.is_some() {
            match self.send_request().await {
                Ok(state) => return Ok(state),
                Err(_) => {
                    // Connection broken, drop and reconnect
                    self.conn = None;
                }
            }
        }

        // Connect (or reconnect)
        self.connect().await?;
        self.send_request().await
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

    async fn send_request(&mut self) -> Result<InspectState> {
        let conn = self.conn.as_mut().context("not connected")?;

        // Send request
        let request = InspectRequest {
            method: "get_state".to_string(),
            params: None,
        };
        let mut json = serde_json::to_string(&request)?;
        json.push('\n');
        conn.writer.write_all(json.as_bytes()).await?;
        conn.writer.flush().await?;

        // Read response
        let mut response_line = String::new();
        let bytes = conn
            .reader
            .read_line(&mut response_line)
            .await
            .context("failed to read response")?;

        if bytes == 0 {
            anyhow::bail!("server closed connection");
        }

        let resp: InspectResponse =
            serde_json::from_str(response_line.trim()).context("failed to parse inspect response")?;

        match resp {
            InspectResponse::State(state) => Ok(state),
            InspectResponse::Error { error } => anyhow::bail!("server error: {error}"),
            InspectResponse::ReloadResult { .. } => anyhow::bail!("unexpected reload_result for get_state"),
        }
    }

    /// Send a `reload_config` command to the inspect server.
    pub async fn reload_config(&mut self) -> Result<(bool, String)> {
        // Ensure connected
        if self.conn.is_none() {
            self.connect().await?;
        }

        let conn = self.conn.as_mut().context("not connected")?;

        let request = InspectRequest {
            method: "reload_config".to_string(),
            params: None,
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

        let resp: InspectResponse =
            serde_json::from_str(response_line.trim()).context("failed to parse inspect response")?;

        match resp {
            InspectResponse::ReloadResult { success, message } => Ok((success, message)),
            InspectResponse::Error { error } => Ok((false, error)),
            InspectResponse::State(_) => anyhow::bail!("unexpected state for reload_config"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    use crate::inspect::server::{InspectContext, InspectServer};
    use crate::inspect::types::ChannelInfo;

    fn test_context() -> Arc<InspectContext> {
        Arc::new(InspectContext {
            thread_managers: vec![],
            channels: vec![ChannelInfo {
                name: "test-ch".to_string(),
                channel_type: "email".to_string(),
            }],
            health_stats: Arc::new(Mutex::new(
                crate::core::metrics::HealthStats::default(),
            )),
            activity_map: Arc::new(Mutex::new(HashMap::new())),
            max_concurrent: 5,
            start_time: Instant::now(),
            config_path: None,
            config: None,
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
        assert_eq!(state.stats.max_concurrent, 5);

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
}
