use anyhow::{Context, Result};

use jyc_types::{
    HealthResponse, InjectMessageRequest, InjectMessageResult, InspectState, ReloadResult,
    ResetSessionRequest, ResetSessionResult,
};

/// Client for connecting to the jyc inspect server over HTTP.
///
/// Holds a long-lived `reqwest::Client` (which maintains its own connection
/// pool) and a base URL derived from the server bind address.
#[derive(Debug, Clone)]
pub struct InspectClient {
    base_url: String,
    http: reqwest::Client,
    token: Option<String>,
}

impl InspectClient {
    /// Create a new client targeting the inspect server at `addr` (e.g. `"127.0.0.1:9876"`).
    ///
    /// Authentication is resolved lazily on each request: the
    /// `JYC_INSPECT_TOKEN` environment variable is checked first, then
    /// `<data_dir>/inspect-token` is read fresh on every request. This means
    /// `jyc token rotate` takes effect immediately on the next request.
    pub fn new(addr: &str) -> Self {
        let base_url = if addr.starts_with("http://") || addr.starts_with("https://") {
            addr.to_string()
        } else {
            format!("http://{addr}")
        };
        Self {
            base_url,
            http: reqwest::Client::builder()
                .build()
                .expect("reqwest client builder should not fail"),
            token: None,
        }
    }

    /// Create a client that sends a specific token on every request.
    ///
    /// Explicit tokens lock in for the lifetime of the client — they do
    /// NOT re-read from the env var or token file. Use `new()` if you want
    /// rotation to take effect automatically.
    pub fn new_with_token(addr: &str, token: impl Into<String>) -> Self {
        let mut c = Self::new(addr);
        c.token = Some(token.into());
        c
    }

    fn token_for_request(&self) -> Option<String> {
        if let Some(t) = &self.token {
            return Some(t.clone());
        }
        // Flag-not-set path: prefer env var over file, matching resolve_dashboard_token
        if let Ok(t) = std::env::var("JYC_INSPECT_TOKEN")
            && !t.is_empty()
        {
            return Some(t);
        }
        jyc_utils::inspect_token::read().ok().flatten()
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        let mut rb = self.http.request(method, &url);
        if let Some(tok) = self.token_for_request() {
            rb = rb.bearer_auth(tok);
        }
        rb
    }

    /// Fetch the current state.
    pub async fn get_state(&mut self) -> Result<InspectState> {
        let resp = self
            .request(reqwest::Method::GET, "/state")
            .send()
            .await
            .context("failed to GET /state")?;
        map_json_ok(resp).await
    }

    /// Send a `reload_config` request to the inspect server.
    pub async fn reload_config(&mut self) -> Result<(bool, String)> {
        let resp = self
            .request(reqwest::Method::POST, "/reload_config")
            .send()
            .await
            .context("failed to POST /reload_config")?;
        map_result::<ReloadResult>(resp, "/reload_config").await
    }

    /// Send a `reset_session` request to the inspect server.
    pub async fn reset_session(&mut self, thread_name: &str) -> Result<(bool, String)> {
        let body = ResetSessionRequest {
            thread_name: thread_name.to_string(),
        };
        let resp = self
            .request(reqwest::Method::POST, "/reset_session")
            .json(&body)
            .send()
            .await
            .context("failed to POST /reset_session")?;
        map_result::<ResetSessionResult>(resp, "/reset_session").await
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
        let body = InjectMessageRequest {
            channel: channel.to_string(),
            thread: thread.to_string(),
            text: text.to_string(),
        };
        let resp = self
            .request(reqwest::Method::POST, "/inject_message")
            .json(&body)
            .send()
            .await
            .context("failed to POST /inject_message")?;
        map_result::<InjectMessageResult>(resp, "/inject_message").await
    }

    /// Health check.
    pub async fn health(&self) -> Result<HealthResponse> {
        let resp = self
            .request(reqwest::Method::GET, "/health")
            .send()
            .await
            .context("failed to GET /health")?;
        map_json_ok(resp).await
    }
}

trait ExtractSuccessMessage {
    fn success_and_message(self) -> (bool, String);
}

impl ExtractSuccessMessage for ReloadResult {
    fn success_and_message(self) -> (bool, String) {
        (self.success, self.message)
    }
}

impl ExtractSuccessMessage for ResetSessionResult {
    fn success_and_message(self) -> (bool, String) {
        (self.success, self.message)
    }
}

impl ExtractSuccessMessage for InjectMessageResult {
    fn success_and_message(self) -> (bool, String) {
        (self.success, self.message)
    }
}

/// Decode a 2xx response as JSON. Non-2xx becomes an `anyhow::Error` with the
/// response body's `error` field (or status text) when available.
async fn map_json_ok<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    let text = resp.text().await.context("failed to read response body")?;
    if !status.is_success() {
        let msg = extract_error_field(&text).unwrap_or_else(|| format!("HTTP {status}: {text}"));
        anyhow::bail!("{msg}");
    }
    serde_json::from_str(&text).with_context(|| format!("failed to parse response body: {text}"))
}

/// Decode a 2xx response as a `(success, message)` tuple from a struct that
/// has those fields. Used by the action endpoints.
async fn map_result<T>(resp: reqwest::Response, endpoint: &str) -> Result<(bool, String)>
where
    T: serde::de::DeserializeOwned + ExtractSuccessMessage,
{
    let status = resp.status();
    let text = resp.text().await.context("failed to read response body")?;
    if !status.is_success() {
        let msg = extract_error_field(&text).unwrap_or_else(|| format!("HTTP {status}: {text}"));
        anyhow::bail!("{endpoint}: {msg}");
    }
    let parsed: T = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {endpoint} response: {text}"))?;
    Ok(parsed.success_and_message())
}

fn extract_error_field(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str().map(|s| s.to_string()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    use crate::server::{InspectContext, build_router};
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
            // Point at a unique nonexistent path so the test never reads
            // the real platform token file. See server.rs `test_context`.
            token_data_home: Some(nonexistent_token_home_path()),
        })
    }

    fn nonexistent_token_home_path() -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "jyc-inspect-test-nonexistent-{}-{}",
            std::process::id(),
            nonce
        ))
    }

    async fn spawn_test_server(
        context: Arc<InspectContext>,
        cancel: CancellationToken,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = build_router(context);
        let service = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
        let server = axum::serve(listener, service).with_graceful_shutdown(async move {
            cancel.cancelled().await;
        });
        let handle = tokio::spawn(async move {
            let _ = server.await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (addr.to_string(), handle)
    }

    #[tokio::test]
    async fn test_inspect_client_get_state() {
        let cancel = CancellationToken::new();
        let context = test_context();
        let (addr, handle) = spawn_test_server(context, cancel.clone()).await;

        let mut client = InspectClient::new(&addr);
        let state = client.get_state().await.unwrap();
        assert_eq!(state.channels.len(), 1);
        assert_eq!(state.channels[0].name, "test-ch");

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_inspect_client_health() {
        let cancel = CancellationToken::new();
        let context = test_context();
        let (addr, handle) = spawn_test_server(context, cancel.clone()).await;

        let client = InspectClient::new(&addr);
        let resp = client.health().await.unwrap();
        assert_eq!(resp.status, "ok");

        cancel.cancel();
        handle.await.unwrap();
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
            token_data_home: Some(nonexistent_token_home_path()),
        });

        let cancel = CancellationToken::new();
        let (addr, handle) = spawn_test_server(context, cancel.clone()).await;

        let mut client = InspectClient::new(&addr);
        let (success, message) = client.reset_session("test-thread").await.unwrap();
        assert!(success, "reset should succeed: {message}");
        assert!(message.contains("session deleted"));
        assert!(!jyc_dir.join("agent-session.json").exists());

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_inspect_client_inject_message_no_channel() {
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
            token_data_home: Some(nonexistent_token_home_path()),
        });

        let cancel = CancellationToken::new();
        let (addr, handle) = spawn_test_server(context, cancel.clone()).await;

        let mut client = InspectClient::new(&addr);
        let err = client
            .inject_message("nonexistent", "thread", "hello")
            .await
            .expect_err("inject should fail for unknown channel");
        assert!(
            err.to_string().contains("no thread manager found"),
            "unexpected error: {err}"
        );

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_inspect_client_connection_refused() {
        let mut client = InspectClient::new("127.0.0.1:1");
        let result = client.get_state().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_inspect_client_with_explicit_token() {
        let cancel = CancellationToken::new();
        let context = test_context();
        let (addr, handle) = spawn_test_server(context, cancel.clone()).await;

        // No token file is configured on the server, so any token (or
        // none) is accepted — auth is opt-in via file presence.
        let mut client = InspectClient::new_with_token(&addr, "any-token-here");
        let state = client.get_state().await.unwrap();
        assert_eq!(state.channels.len(), 1);

        // Also works without any Authorization header.
        let mut client = InspectClient::new(&addr);
        let state = client.get_state().await.unwrap();
        assert_eq!(state.channels.len(), 1);

        cancel.cancel();
        handle.await.unwrap();
    }

    #[test]
    fn test_base_url_construction() {
        let c = InspectClient::new("127.0.0.1:9876");
        assert_eq!(c.base_url, "http://127.0.0.1:9876");

        let c = InspectClient::new("http://localhost:9876");
        assert_eq!(c.base_url, "http://localhost:9876");
    }
}
