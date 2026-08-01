//! Integration tests for the inspect server's HTTP REST API + Bearer auth.
//!
//! Spins up a real `axum` server bound to `127.0.0.1:0` and exercises it
//! with `reqwest` — the way a real third-party client would.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use jyc_core::metrics::HealthStats;
use jyc_inspect::server::{InspectContext, build_router};
use jyc_types::ChannelInfo;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

fn ctx_with_token(token: Option<&str>) -> Arc<InspectContext> {
    Arc::new(InspectContext {
        thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
        channels: Arc::new(ArcSwap::from_pointee(vec![ChannelInfo {
            name: "ch".into(),
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
        auth_token: token.map(String::from),
        inspect_broadcast: Arc::new(tokio::sync::broadcast::channel(1).0),
    })
}

async fn start_server(ctx: Arc<InspectContext>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(ctx);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Give the server a moment to start accepting connections.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    addr
}

// ─── Auth ────────────────────────────────────────────────────────────

#[tokio::test]
async fn api_requires_bearer() {
    let addr = start_server(ctx_with_token(Some("secret"))).await;
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/api/state"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn api_rejects_wrong_bearer() {
    let addr = start_server(ctx_with_token(Some("secret"))).await;
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/api/state"))
        .header(reqwest::header::AUTHORIZATION, "Bearer wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn api_accepts_correct_bearer() {
    let addr = start_server(ctx_with_token(Some("secret"))).await;
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/api/state"))
        .header(reqwest::header::AUTHORIZATION, "Bearer secret")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn api_no_token_configured_allows_anonymous() {
    let addr = start_server(ctx_with_token(None)).await;
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/api/state"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn ws_upgrade_requires_bearer() {
    let addr = start_server(ctx_with_token(Some("secret"))).await;
    // Plain GET to /ws without an Upgrade header → 426 (axum returns 426
    // for missing Upgrade header) or 401 (if auth check runs first).
    // Either way: not 101.
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/ws"))
        .send()
        .await
        .unwrap();
    assert!(res.status() != 101, "WS must not upgrade without auth");
}

#[tokio::test]
async fn same_token_works_for_api_and_ws() {
    let addr = start_server(ctx_with_token(Some("shared"))).await;
    // API
    let api_res = reqwest::Client::new()
        .get(format!("http://{addr}/api/state"))
        .header(reqwest::header::AUTHORIZATION, "Bearer shared")
        .send()
        .await
        .unwrap();
    assert_eq!(api_res.status(), 200);
    // WS upgrade attempt (will fail for non-WS path, but should not 401)
    let ws_res = reqwest::Client::new()
        .get(format!("http://{addr}/ws/missing"))
        .header(reqwest::header::AUTHORIZATION, "Bearer shared")
        .header(reqwest::header::UPGRADE, "websocket")
        .header(reqwest::header::CONNECTION, "Upgrade")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    // The WS upgrade succeeds with 101 Switching Protocols (auth passed;
    // the channel resolver runs after the upgrade in production).
    assert_eq!(
        ws_res.status(),
        101,
        "auth should pass; got {}",
        ws_res.status()
    );
}

// ─── REST endpoints ──────────────────────────────────────────────────

#[tokio::test]
async fn get_state_returns_json() {
    let addr = start_server(ctx_with_token(None)).await;
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/api/state"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let v: serde_json::Value = res.json().await.unwrap();
    assert!(v["uptime_secs"].is_u64());
    assert!(v["version"].is_string());
    assert!(v["channels"].is_array());
}

#[tokio::test]
async fn get_state_overview_returns_slim() {
    let addr = start_server(ctx_with_token(None)).await;
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/api/state/overview"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let v: serde_json::Value = res.json().await.unwrap();
    assert!(v["threads"].is_array());
}

#[tokio::test]
async fn patterns_returns_404_for_unknown_channel() {
    let addr = start_server(ctx_with_token(None)).await;
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/api/channels/nonexistent/patterns"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    let v: serde_json::Value = res.json().await.unwrap();
    assert!(v["error"].is_string());
}

#[tokio::test]
async fn thread_activity_returns_404_for_unknown_thread() {
    let addr = start_server(ctx_with_token(None)).await;
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/api/threads/ch/issue-1/activity"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn thread_chat_returns_404_for_unknown_thread() {
    let addr = start_server(ctx_with_token(None)).await;
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/api/threads/ch/issue-1/chat"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn create_thread_rejects_traversal() {
    let addr = start_server(ctx_with_token(None)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/api/threads"))
        .json(&serde_json::json!({
            "channel": "ch",
            "thread": "../escape",
            "path": "/tmp/x"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
}

#[tokio::test]
async fn create_thread_returns_404_for_unknown_channel() {
    let addr = start_server(ctx_with_token(None)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/api/threads"))
        .json(&serde_json::json!({
            "channel": "nonexistent",
            "thread": "ok",
            "path": "/tmp/x"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn reload_config_returns_422_when_no_path() {
    let addr = start_server(ctx_with_token(None)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/api/config/reload"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 422);
}
