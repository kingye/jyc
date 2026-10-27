//! End-to-end test: InspectClient (reqwest) against a real axum server.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures_util::StreamExt;
use jyc_core::agent::{AgentResult, AgentService};
use jyc_core::message_storage::MessageStorage;
use jyc_core::metrics::{HealthStats, MetricsHandle};
use jyc_core::static_agent::StaticAgentService;
use jyc_core::topic_event::TopicEvent;
use jyc_core::topic_manager::{TopicManager, TopicManagers};
use jyc_inspect::client::InspectClient;
use jyc_inspect::server::websocket::outbound::WebsocketOutboundAdapter;
use jyc_inspect::server::{DynWebsocketHandler, InspectContext, WebsocketHandler, build_router};
use jyc_types::{
    ActivityEntry, AppConfig, ChannelInfo, InboundMessage, InspectState, MessageContent,
    PatternMatch, QueueItem, load_config_from_str,
};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

fn ctx() -> Arc<InspectContext> {
    ctx_with_managers(
        Arc::new(TopicManagers::new()),
        PathBuf::from("/nonexistent/agents"),
    )
}

fn ctx_with_managers(managers: Arc<TopicManagers>, agents_dir: PathBuf) -> Arc<InspectContext> {
    Arc::new(InspectContext {
        topic_managers: managers,
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
        ws_shutdown: tokio_util::sync::CancellationToken::new(),
        agents_dir,
    })
}

fn test_config() -> Arc<AppConfig> {
    Arc::new(
        load_config_from_str(
            r#"
[general]
[channels.test]
type = "email"
[channels.test.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.test.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
"#,
        )
        .unwrap(),
    )
}

fn make_tm(tmp: &TempDir, workspace: &Path, channel: &str) -> Arc<TopicManager> {
    make_tm_with_agent(
        tmp,
        workspace,
        channel,
        10,
        Arc::new(StaticAgentService::new("ok")),
    )
}

fn make_tm_with_agent(
    tmp: &TempDir,
    workspace: &Path,
    channel: &str,
    max_queue_size: usize,
    agent: Arc<dyn AgentService>,
) -> Arc<TopicManager> {
    let storage = Arc::new(MessageStorage::new(workspace));
    Arc::new(TopicManager::new(
        1,
        max_queue_size,
        storage.clone(),
        Arc::new(WebsocketOutboundAdapter::new(
            tokio::sync::broadcast::channel(4).0,
            storage,
        )),
        agent,
        tokio_util::sync::CancellationToken::new(),
        workspace.join("templates"),
        Arc::new(ArcSwap::new(test_config())),
        channel.to_string(),
        "websocket".to_string(),
        tmp.path().to_path_buf(),
        workspace.to_path_buf(),
        MetricsHandle::noop(),
    ))
}

struct BlockingAgent {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl AgentService for BlockingAgent {
    async fn base_url(&self) -> anyhow::Result<String> {
        Ok(String::new())
    }

    async fn process(
        &self,
        _message: &InboundMessage,
        _topic_name: &str,
        _topic_path: &Path,
        _message_dir: &str,
        _pending_rx: &mut tokio::sync::mpsc::Receiver<QueueItem>,
        _topic_cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<AgentResult> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(AgentResult {
            reply_delivered: true,
            reply_text: Some("ok".to_string()),
        })
    }

    async fn reset_session(&self, _topic_path: &Path, _topic_name: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

async fn seed_topic(tm: &Arc<TopicManager>, channel: &str, topic: &str) {
    tm.enqueue(
        InboundMessage {
            id: "seed".to_string(),
            channel: channel.to_string(),
            channel_uid: "seed".to_string(),
            sender: "user".to_string(),
            sender_address: "user@example.com".to_string(),
            recipients: vec![],
            topic: topic.to_string(),
            content: MessageContent {
                text: Some("seed".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            references: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        },
        topic.to_string(),
        PatternMatch {
            pattern_name: "invoice".to_string(),
            channel: channel.to_string(),
            matches: HashMap::new(),
        },
        None,
        false,
        None,
    )
    .await;
}

struct RegisteredState(String);

impl Drop for RegisteredState {
    fn drop(&mut self) {
        jyc_types::state_dir::unregister(&self.0);
    }
}

async fn start_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    start_server_with(ctx()).await
}

async fn start_server_with(
    ctx: Arc<InspectContext>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(ctx);
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
    assert!(overview.topics.is_empty());
}

#[tokio::test]
async fn client_get_topic_activity_unknown_topic_404() {
    let (addr, _h) = start_server().await;
    let client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    let r: Result<Vec<ActivityEntry>, _> = client
        .get_topic_activity("demo", "no-such-topic", None, None)
        .await;
    assert!(r.is_err());
    let err = format!("{}", r.unwrap_err());
    assert!(err.contains("404"), "expected 404, got: {err}");
}

#[tokio::test]
async fn client_sends_messages_to_an_agents_own_topic_in_order() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let agents_dir = tmp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();

    let tm = make_tm(&tmp, &workspace, "agents");
    let managers = Arc::new(TopicManagers::new());
    managers.set(vec![tm.clone()]);
    tm.set_managers(&managers);

    let topic = "api-send-default";
    let _registered = RegisteredState(topic.to_string());
    seed_topic(&tm, "agents", topic).await;
    let bus = tm.get_or_create_event_bus(topic).await.unwrap();
    let mut events = bus.subscribe().await.unwrap();

    let (addr, _h) = start_server_with(ctx_with_managers(managers, agents_dir)).await;
    let client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    for message in ["first scheduled", "second scheduled"] {
        let (ok, response) = client.send_to_topic(topic, None, message).await.unwrap();
        assert!(ok, "{response}");
    }

    let mut texts = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while texts.len() < 2 {
            match events.recv().await {
                Some(TopicEvent::IncomingMessage { text, .. })
                    if text == "first scheduled" || text == "second scheduled" =>
                {
                    texts.push(text);
                }
                Some(_) => {}
                None => break,
            }
        }
    })
    .await
    .expect("both messages should reach the topic");
    assert_eq!(texts, ["first scheduled", "second scheduled"]);
}

#[tokio::test]
async fn client_sends_to_an_explicit_topic() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let agents_dir = tmp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();

    let tm = make_tm(&tmp, &workspace, "agents");
    let managers = Arc::new(TopicManagers::new());
    managers.set(vec![tm.clone()]);
    tm.set_managers(&managers);

    let topic = "api-send-explicit";
    let _registered = RegisteredState(topic.to_string());
    seed_topic(&tm, "agents", topic).await;
    let bus = tm.get_or_create_event_bus(topic).await.unwrap();
    let mut events = bus.subscribe().await.unwrap();

    let (addr, _h) = start_server_with(ctx_with_managers(managers, agents_dir)).await;
    let client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    let (ok, response) = client
        .send_to_topic("some-agent", Some(topic), "explicit topic")
        .await
        .unwrap();
    assert!(ok, "{response}");

    let mut text = None;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while text.is_none() {
            match events.recv().await {
                Some(TopicEvent::IncomingMessage { text: body, .. })
                    if body == "explicit topic" =>
                {
                    text = Some(body);
                }
                Some(_) => {}
                None => break,
            }
        }
    })
    .await
    .expect("the message should reach the named topic");
    assert_eq!(text.as_deref(), Some("explicit topic"));
}

#[tokio::test]
async fn send_returns_429_when_the_queue_is_full() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let agents_dir = tmp.path().join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let agent = Arc::new(BlockingAgent {
        started: started.clone(),
        release: release.clone(),
    });
    let tm = make_tm_with_agent(&tmp, &workspace, "agents", 1, agent);
    let managers = Arc::new(TopicManagers::new());
    managers.set(vec![tm.clone()]);
    tm.set_managers(&managers);

    let topic = "api-send-full";
    let _registered = RegisteredState(topic.to_string());
    seed_topic(&tm, "agents", topic).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .expect("the seed message should occupy the worker");

    let (addr, _h) = start_server_with(ctx_with_managers(managers, agents_dir)).await;
    let http = reqwest::Client::new();
    let send = |message: &'static str| {
        http.post(format!("http://{addr}/api/send"))
            .json(&serde_json::json!({ "agent": topic, "message": message }))
            .send()
    };

    let first = send("first").await.unwrap();
    assert_eq!(first.status(), 200);
    let second = send("second").await.unwrap();
    assert_eq!(second.status(), 429);
    let body: serde_json::Value = second.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("queue is full")),
        "{body}"
    );

    release.notify_waiters();
}

#[tokio::test]
async fn client_send_rejects_unknown_target() {
    let (addr, _h) = start_server().await;
    let client = InspectClient::with_token(&addr.to_string(), Some("topsecret"));
    let (ok, response) = client
        .send_to_topic("missing", None, "hello")
        .await
        .unwrap();
    assert!(!ok);
    assert!(response.contains("missing"), "{response}");
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

/// Stub `WebsocketHandler` that records the `scoped_topic` it is invoked
/// with, then keeps the connection open until the client disconnects.
struct RecordingWsHandler {
    scoped_topic: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl WebsocketHandler for RecordingWsHandler {
    async fn handle(
        &self,
        ws: axum::extract::ws::WebSocket,
        _addr: std::net::SocketAddr,
        scoped_topic: Option<&str>,
    ) -> anyhow::Result<()> {
        *self.scoped_topic.lock().await = scoped_topic.map(|s| s.to_string());
        let (_mut_tx, mut rx) = ws.split();
        while let Some(Ok(_)) = rx.next().await {}
        Ok(())
    }
}

/// Regression test: connecting to `/ws/<channel>/<topic>` must propagate the
/// URL topic name to the handler as `scoped_topic`. Previously
/// `ws_upgrade_for_route` always passed `None`, so websocket-channel chat
/// panes (which omit `topic` from the payload) had their messages dropped
/// with "WebSocket Message without topic; ignoring".
#[tokio::test]
async fn ws_topic_route_propagates_scoped_topic() {
    let recorded = Arc::new(Mutex::new(None));
    let handler = Arc::new(RecordingWsHandler {
        scoped_topic: recorded.clone(),
    });
    let mut handlers: HashMap<String, DynWebsocketHandler> = HashMap::new();
    handlers.insert("test_channel".to_string(), handler);

    let context = Arc::new(InspectContext {
        topic_managers: Arc::new(TopicManagers::new()),
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
        ws_shutdown: tokio_util::sync::CancellationToken::new(),
        agents_dir: std::path::PathBuf::from("/nonexistent/agents"),
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_router(context);
    let _server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Connect to /ws/<channel>/<topic> with the same auth the dashboard uses.
    let url = format!("ws://{}/ws/test_channel/my-topic", addr);
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
    assert_eq!(got.as_deref(), Some("my-topic"));
}
