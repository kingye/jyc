//! End-to-end test: InspectClient (reqwest) against a real axum server.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures_util::StreamExt;
use jyc_core::metrics::HealthStats;
use jyc_inspect::client::InspectClient;
use jyc_inspect::server::{DynWebsocketHandler, InspectContext, WebsocketHandler, build_router};
use jyc_types::{ActivityEntry, ChannelInfo, InspectState};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

fn ctx() -> Arc<InspectContext> {
    Arc::new(InspectContext {
        thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
        channels: Arc::new(ArcSwap::from_pointee(vec![ChannelInfo {
            name: "demo".into(),
            channel_type: "email".into(),
            active_workers: 0,
            max_concurrent: 0,
        }])),
        health_stats: Arc::new(Mutex::new(HealthStats::default())),
        activity_map: Arc::new(Mutex::new(HashMap::new())),
        start_time: Instant::now(),
        config_path: None,
        global_config_path: None,
        config: None,
        workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![])),
        websocket_handlers: None,
        reload_callback: None,
        auth_token: Some("topsecret".into()),
        inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(1).0),
    })
}

async fn start_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(ctx());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    (addr, handle)
}

#[tokio::test]
async fn client_get_state_with_token() {
    let (addr, _h) = start_server().await;
    let client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    let state: InspectState = client.get_state().await.unwrap();
    assert!(!state.channels.is_empty());
    assert_eq!(state.channels[0].name, "demo");
}

#[tokio::test]
async fn client_get_state_without_token_401() {
    let (addr, _h) = start_server().await;
    let client = InspectClient::new(&addr.to_string());
    let r = client.get_state().await;
    assert!(r.is_err(), "expected error without token, got {:?}", r);
}

#[tokio::test]
async fn client_get_overview_works() {
    let (addr, _h) = start_server().await;
    let client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    let overview = client.get_overview().await.unwrap();
    assert_eq!(overview.channels.len(), 1);
    assert!(overview.threads.is_empty());
}

#[tokio::test]
async fn client_get_thread_activity_unknown_thread_404() {
    let (addr, _h) = start_server().await;
    let client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    let r: Result<Vec<ActivityEntry>, _> = client
        .get_thread_activity("demo", "no-such-thread", None, None)
        .await;
    assert!(r.is_err());
    let err = format!("{}", r.unwrap_err());
    assert!(err.contains("404"), "expected 404, got: {err}");
}

#[tokio::test]
async fn client_reload_config_no_path_returns_422() {
    let (addr, _h) = start_server().await;
    let client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    let (ok, msg) = client.reload_config().await.unwrap();
    assert!(!ok);
    assert!(msg.contains("no config path") || msg.contains("422"));
}

#[tokio::test]
async fn client_list_patterns_unknown_channel_404() {
    let (addr, _h) = start_server().await;
    let client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    let r = client.list_patterns("nonexistent").await;
    assert!(r.is_err());
}

/// Stub `WebsocketHandler` that records the `scoped_thread` it is invoked
/// with, then keeps the connection open until the client disconnects.
struct RecordingWsHandler {
    scoped_thread: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl WebsocketHandler for RecordingWsHandler {
    async fn handle(
        &self,
        ws: axum::extract::ws::WebSocket,
        _addr: std::net::SocketAddr,
        scoped_thread: Option<&str>,
    ) -> anyhow::Result<()> {
        *self.scoped_thread.lock().await = scoped_thread.map(|s| s.to_string());
        let (_mut_tx, mut rx) = ws.split();
        while let Some(Ok(_)) = rx.next().await {}
        Ok(())
    }
}

/// Regression test: connecting to `/ws/<channel>/<thread>` must propagate the
/// URL thread name to the handler as `scoped_thread`. Previously
/// `ws_upgrade_for_route` always passed `None`, so websocket-channel chat
/// panes (which omit `thread` from the payload) had their messages dropped
/// with "WebSocket Message without thread; ignoring".
#[tokio::test]
async fn ws_thread_route_propagates_scoped_thread() {
    let recorded = Arc::new(Mutex::new(None));
    let handler = Arc::new(RecordingWsHandler {
        scoped_thread: recorded.clone(),
    });
    let mut handlers: HashMap<String, DynWebsocketHandler> = HashMap::new();
    handlers.insert("test_channel".to_string(), handler);

    let context = Arc::new(InspectContext {
        thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
        channels: Arc::new(ArcSwap::from_pointee(vec![ChannelInfo {
            name: "test_channel".into(),
            channel_type: "websocket".into(),
            active_workers: 0,
            max_concurrent: 0,
        }])),
        health_stats: Arc::new(Mutex::new(HealthStats::default())),
        activity_map: Arc::new(Mutex::new(HashMap::new())),
        start_time: Instant::now(),
        config_path: None,
        global_config_path: None,
        config: None,
        workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![])),
        websocket_handlers: Some(handlers),
        reload_callback: None,
        auth_token: Some("topsecret".into()),
        inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(1).0),
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(context);
    let _server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Connect to /ws/<channel>/<thread> with the same auth the dashboard uses.
    let url = format!("ws://{}/ws/test_channel/my-thread", addr);
    let mut request = url.as_str().into_client_request().unwrap();
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str("Bearer topsecret").unwrap(),
    );
    let ws_stream = tokio_tungstenite::connect_async(request).await.unwrap().0;
    let (_mut_write, _mut_read) = ws_stream.split();

    // Poll until the handler has been invoked (it runs right after the
    // upgrade completes).
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    let got = loop {
        let got = recorded.lock().await.clone();
        if got.is_some() || Instant::now() > deadline {
            break got;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(got.as_deref(), Some("my-thread"));
}
