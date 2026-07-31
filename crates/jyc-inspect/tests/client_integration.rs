//! End-to-end test: InspectClient (reqwest) against a real axum server.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use jyc_core::metrics::HealthStats;
use jyc_inspect::client::InspectClient;
use jyc_inspect::server::{InspectContext, build_router};
use jyc_types::{ActivityEntry, ChannelInfo, InspectState};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

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
    let mut client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    let state: InspectState = client.get_state().await.unwrap();
    assert!(!state.channels.is_empty());
    assert_eq!(state.channels[0].name, "demo");
}

#[tokio::test]
async fn client_get_state_without_token_401() {
    let (addr, _h) = start_server().await;
    let mut client = InspectClient::new(&addr.to_string());
    let r = client.get_state().await;
    assert!(r.is_err(), "expected error without token, got {:?}", r);
}

#[tokio::test]
async fn client_get_overview_works() {
    let (addr, _h) = start_server().await;
    let mut client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    let overview = client.get_overview().await.unwrap();
    assert_eq!(overview.channels.len(), 1);
    assert!(overview.threads.is_empty());
}

#[tokio::test]
async fn client_get_thread_activity_unknown_thread_404() {
    let (addr, _h) = start_server().await;
    let mut client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
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
    let mut client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    let (ok, msg) = client.reload_config().await.unwrap();
    assert!(!ok);
    assert!(msg.contains("no config path") || msg.contains("422"));
}

#[tokio::test]
async fn client_list_patterns_unknown_channel_404() {
    let (addr, _h) = start_server().await;
    let mut client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    let r = client.list_patterns("nonexistent").await;
    assert!(r.is_err());
}
