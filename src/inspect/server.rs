use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::core::metrics::SharedHealthStats;
use crate::core::thread_manager::ThreadManager;
use crate::inspect::types::*;

/// Shared state accessible by the inspect server.
pub struct InspectContext {
    /// Per-channel thread managers
    pub thread_managers: Vec<Arc<ThreadManager>>,
    /// Channel info (name, type)
    pub channels: Vec<ChannelInfo>,
    /// Shared health stats from MetricsCollector
    pub health_stats: SharedHealthStats,
    /// Max concurrent threads per channel
    pub max_concurrent: usize,
    /// When the monitor started
    pub start_time: Instant,
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
                            tracing::trace!(addr = %addr, "Inspect client connected");
                            let ctx = self.context.clone();
                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_client(stream, ctx).await {
                                    tracing::trace!(error = %e, "Inspect client disconnected");
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
            other => InspectResponse::Error {
                error: format!("unknown method: {other}"),
            },
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
            max_concurrent: 3,
            start_time: Instant::now(),
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

        let state: InspectState = serde_json::from_str(&response).unwrap();
        assert_eq!(state.channels.len(), 1);
        assert_eq!(state.channels[0].name, "emf");
        assert_eq!(state.stats.max_concurrent, 3);
        assert!(!state.version.is_empty());

        cancel.cancel();
        handle.await.unwrap();
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

            let state: InspectState = serde_json::from_str(&response).unwrap();
            assert_eq!(state.channels.len(), 1);
        }

        cancel.cancel();
        handle.await.unwrap();
    }
}
