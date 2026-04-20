use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::inspect::types::{InspectRequest, InspectState};

/// Client for connecting to the jyc inspect server.
pub struct InspectClient {
    addr: String,
}

impl InspectClient {
    pub fn new(addr: &str) -> Self {
        Self {
            addr: addr.to_string(),
        }
    }

    /// Connect and fetch the current state. Opens a new TCP connection each time.
    pub async fn get_state(&self) -> Result<InspectState> {
        let stream = TcpStream::connect(&self.addr)
            .await
            .with_context(|| format!("failed to connect to inspect server at {}", self.addr))?;

        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // Send request
        let request = InspectRequest {
            method: "get_state".to_string(),
        };
        let mut json = serde_json::to_string(&request)?;
        json.push('\n');
        writer.write_all(json.as_bytes()).await?;
        writer.flush().await?;

        // Read response
        let mut response_line = String::new();
        reader
            .read_line(&mut response_line)
            .await
            .context("failed to read response from inspect server")?;

        let state: InspectState =
            serde_json::from_str(response_line.trim()).context("failed to parse inspect state")?;

        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    use crate::inspect::server::{InspectContext, InspectServer};
    use crate::inspect::types::ChannelInfo;

    #[tokio::test]
    async fn test_inspect_client_get_state() {
        let cancel = CancellationToken::new();
        let context = Arc::new(InspectContext {
            thread_managers: vec![],
            channels: vec![ChannelInfo {
                name: "test-ch".to_string(),
                channel_type: "email".to_string(),
            }],
            health_stats: Arc::new(Mutex::new(
                crate::core::metrics::HealthStats::default(),
            )),
            max_concurrent: 5,
            start_time: Instant::now(),
        });

        // Bind to random port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server = InspectServer::new(addr.to_string(), context, cancel.clone());
        let _handle = server.start();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Use client
        let client = InspectClient::new(&addr.to_string());
        let state = client.get_state().await.unwrap();

        assert_eq!(state.channels.len(), 1);
        assert_eq!(state.channels[0].name, "test-ch");
        assert_eq!(state.stats.max_concurrent, 5);

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_inspect_client_connection_refused() {
        // Connect to a port that nothing is listening on
        let client = InspectClient::new("127.0.0.1:1");
        let result = client.get_state().await;
        assert!(result.is_err());
    }
}
