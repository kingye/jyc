use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use futures_util::{SinkExt, StreamExt};
use jyc_channels::websocket::inbound::WebsocketInboundAdapter;
use jyc_channels::websocket::outbound::WebsocketOutboundAdapter;
use jyc_core::message_storage::MessageStorage;
use jyc_inspect::server::{InspectContext, WebsocketHandler};
use jyc_types::{InboundAdapter, InboundAdapterOptions, InboundMessage};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn make_config() -> jyc_types::AppConfig {
    let patterns = vec![
        jyc_types::ChannelPattern {
            name: "general".to_string(),
            enabled: true,
            rules: jyc_types::PatternRules::default(),
            ..Default::default()
        },
        jyc_types::ChannelPattern {
            name: "coding-help".to_string(),
            enabled: true,
            rules: jyc_types::PatternRules::default(),
            ..Default::default()
        },
        jyc_types::ChannelPattern {
            name: "disabled".to_string(),
            enabled: false,
            rules: jyc_types::PatternRules::default(),
            ..Default::default()
        },
    ];

    let mut channels = HashMap::new();
    channels.insert(
        "test_ws".to_string(),
        jyc_types::ChannelConfig {
            channel_type: "websocket".to_string(),
            inbound: None,
            outbound: None,
            feishu: None,
            gitee: None,
            github: None,
            wechat: None,
            wecom: None,
            wecom_kf: None,
            wecom_bot: None,
            monitor: None,
            patterns: Some(patterns),
            agent: None,
            model: None,
            small_model: None,
            footer: None,
            skills: None,
            disabled_skills: None,
            disabled_tools: None,
            disabled_mcp_servers: None,
            mcps: None,
        },
    );

    jyc_types::AppConfig {
        general: jyc_types::GeneralConfig::default(),
        channels,
        agent: jyc_types::AgentConfig {
            enabled: false,
            mode: "static".to_string(),
            model: None,
            plan_model: None,
            build_model: None,
            small_model: None,
            system_prompt: None,
            max_iterations: 200,
            sse_read_timeout_secs: 120,
            text: None,
            attachments: None,
            providers: HashMap::new(),
            vision: None,
            reset_compression: None,
            auto_reset_threshold: 0.95,
        },
        inspect: None,
        attachments: None,
        wecom: None,
        mcps: Vec::new(),
        scheduler: jyc_types::SchedulerConfig::default(),
    }
}

#[tokio::test]
async fn test_websocket_adapter_start_and_handle() {
    let app_config = make_config();
    let config_arc = Arc::new(ArcSwap::from_pointee(app_config));

    let (broadcast_tx, _broadcast_rx) = broadcast::channel(16);
    let tmp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(MessageStorage::new(tmp.path()));
    let outbound = WebsocketOutboundAdapter::new(broadcast_tx, storage);
    let inbound = Arc::new(WebsocketInboundAdapter::new(
        "test_ws".to_string(),
        Some(config_arc),
        outbound.broadcast_tx(),
    ));

    // Capture incoming messages
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<InboundMessage>();

    let options = InboundAdapterOptions {
        on_message: Box::new(move |msg: InboundMessage| {
            let _ = msg_tx.send(msg);
            Ok(())
        }),
        on_thread_close: None,
        on_error: Box::new(|e| {
            tracing::error!("Inbound error: {e}");
        }),
        attachment_config: None,
    };

    inbound
        .start(options, CancellationToken::new())
        .await
        .unwrap();

    // Spawn the inspect server with auth pointed at an empty temp dir
    // (no token file → auth not configured → allow).
    let token_home = tempfile::TempDir::new().unwrap();
    let (addr, server_handle, cancel) =
        spawn_test_server(inbound.clone(), token_home.path().to_path_buf()).await;

    // Connect test client
    let url = format!("ws://{}/ws/test_ws", addr);
    let ws_stream = tokio_tungstenite::connect_async(&url).await.unwrap().0;
    let (mut write, mut read) = ws_stream.split();

    // List patterns
    let list_msg = r#"{"type":"list_patterns"}"#;
    write
        .send(tokio_tungstenite::tungstenite::Message::Text(
            list_msg.to_string(),
        ))
        .await
        .unwrap();

    // Read patterns response
    let response = read.next().await.unwrap().unwrap();
    let text = response.to_text().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["type"], "patterns");
    let patterns = parsed["patterns"].as_array().unwrap();
    assert_eq!(patterns.len(), 2);
    assert_eq!(patterns[0], "general");
    assert_eq!(patterns[1], "coding-help");

    // Subscribe to a thread
    let subscribe_msg = r#"{"type":"subscribe","thread":"general"}"#;
    write
        .send(tokio_tungstenite::tungstenite::Message::Text(
            subscribe_msg.to_string(),
        ))
        .await
        .unwrap();

    // Send a message
    let message_text = "Hello from test client";
    let message_msg = format!(
        r#"{{"type":"message","thread":"general","text":"{}"}}"#,
        message_text
    );
    write
        .send(tokio_tungstenite::tungstenite::Message::Text(message_msg))
        .await
        .unwrap();

    // Wait for the inbound message to be captured
    let inbound_msg = tokio::time::timeout(std::time::Duration::from_secs(5), msg_rx.recv())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(inbound_msg.channel, "test_ws");
    assert_eq!(inbound_msg.topic, "general");
    assert_eq!(inbound_msg.content.text.unwrap(), message_text);

    // Close connection
    let _ = write
        .send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await;

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

/// Spawn an inspect server on `127.0.0.1:0` with the given `inbound`
/// adapter registered as the websocket handler. `token_data_home`
/// controls what the auth middleware sees — point it at a temp dir
/// where you've generated a token file to test the auth path, or at
/// an empty dir to test the default-install (no-auth) path.
async fn spawn_test_server(
    inbound: Arc<WebsocketInboundAdapter>,
    token_data_home: std::path::PathBuf,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<()>,
    CancellationToken,
) {
    use std::collections::HashMap as StdHashMap;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let mut handlers: StdHashMap<String, Arc<dyn WebsocketHandler>> = StdHashMap::new();
    handlers.insert("test_ws".to_string(), inbound as Arc<dyn WebsocketHandler>);

    let ctx = Arc::new(InspectContext {
        thread_managers: Arc::new(ArcSwap::from_pointee(vec![])),
        channels: Arc::new(ArcSwap::from_pointee(vec![])),
        health_stats: Arc::new(tokio::sync::Mutex::new(
            jyc_core::metrics::HealthStats::default(),
        )),
        activity_map: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        start_time: std::time::Instant::now(),
        config_path: None,
        global_config_path: None,
        config: None,
        workspace_dirs: Arc::new(ArcSwap::from_pointee(vec![])),
        websocket_handlers: Some(handlers),
        reload_callback: None,
        token_data_home: Some(token_data_home),
    });

    let cancel = CancellationToken::new();
    let cancel_for_server = cancel.clone();
    let app = jyc_inspect::server::build_router(ctx);
    let service = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
    let server = axum::serve(listener, service).with_graceful_shutdown(async move {
        cancel_for_server.cancelled().await;
    });
    let server_handle = tokio::spawn(async move {
        let _ = server.await;
    });
    (addr, server_handle, cancel)
}

/// Auth middleware applied to `/ws/*`: when a token file exists at the
/// configured `token_data_home`, the WS upgrade must include a valid
/// `Authorization: Bearer …` header. The three tests below cover the
/// accept / reject paths. (The default-install no-token-file path is
/// already covered by `test_websocket_adapter_start_and_handle` above.)
use jyc_inspect::client::build_ws_upgrade_request;

#[tokio::test]
async fn test_ws_upgrade_auth_correct_bearer_succeeds() {
    let (broadcast_tx, _broadcast_rx) = broadcast::channel(16);
    let tmp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(MessageStorage::new(tmp.path()));
    let outbound = WebsocketOutboundAdapter::new(broadcast_tx, storage);
    let inbound = Arc::new(WebsocketInboundAdapter::new(
        "test_ws".to_string(),
        None,
        outbound.broadcast_tx(),
    ));

    let (_msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel::<InboundMessage>();
    let options = InboundAdapterOptions {
        on_message: Box::new(|_msg| Ok(())),
        on_thread_close: None,
        on_error: Box::new(|e| tracing::error!("Inbound error: {e}")),
        attachment_config: None,
    };
    inbound
        .start(options, CancellationToken::new())
        .await
        .unwrap();

    // Generate a real token at the test's token home, then point the
    // server at the same path.
    let token_home = tempfile::TempDir::new().unwrap();
    let token = jyc_utils::inspect_token::generate_at(token_home.path()).unwrap();

    let (addr, server_handle, cancel) =
        spawn_test_server(inbound.clone(), token_home.path().to_path_buf()).await;

    // Upgrade with the correct Bearer — should succeed (101 Switching Protocols).
    let url = format!("ws://{}/ws/test_ws", addr);
    let req = build_ws_upgrade_request(&url, Some(&token));
    let result = tokio_tungstenite::connect_async(req).await;
    assert!(
        result.is_ok(),
        "WS upgrade with correct Bearer should succeed; got: {result:?}"
    );

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

#[tokio::test]
async fn test_ws_upgrade_auth_missing_bearer_rejected() {
    let (broadcast_tx, _broadcast_rx) = broadcast::channel(16);
    let tmp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(MessageStorage::new(tmp.path()));
    let outbound = WebsocketOutboundAdapter::new(broadcast_tx, storage);
    let inbound = Arc::new(WebsocketInboundAdapter::new(
        "test_ws".to_string(),
        None,
        outbound.broadcast_tx(),
    ));

    let (_msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel::<InboundMessage>();
    let options = InboundAdapterOptions {
        on_message: Box::new(|_msg| Ok(())),
        on_thread_close: None,
        on_error: Box::new(|e| tracing::error!("Inbound error: {e}")),
        attachment_config: None,
    };
    inbound
        .start(options, CancellationToken::new())
        .await
        .unwrap();

    let token_home = tempfile::TempDir::new().unwrap();
    let _token = jyc_utils::inspect_token::generate_at(token_home.path()).unwrap();

    let (addr, server_handle, cancel) =
        spawn_test_server(inbound.clone(), token_home.path().to_path_buf()).await;

    // No Authorization header — should be rejected (401, NOT a 101 upgrade).
    let url = format!("ws://{}/ws/test_ws", addr);
    let req = build_ws_upgrade_request(&url, None);
    let err = tokio_tungstenite::connect_async(req)
        .await
        .expect_err("WS upgrade without Bearer should fail");
    let http = match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => response,
        other => panic!("expected HTTP error (401), got: {other:?}"),
    };
    assert_eq!(http.status(), 401);
    let body_bytes = http.body().as_deref().unwrap_or(&[]);
    let body_str = std::str::from_utf8(body_bytes).unwrap_or("");
    assert!(
        body_str.contains("missing Authorization"),
        "expected 401 body to mention 'missing Authorization', got: {body_str}"
    );

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}

#[tokio::test]
async fn test_ws_upgrade_auth_wrong_bearer_rejected() {
    let (broadcast_tx, _broadcast_rx) = broadcast::channel(16);
    let tmp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(MessageStorage::new(tmp.path()));
    let outbound = WebsocketOutboundAdapter::new(broadcast_tx, storage);
    let inbound = Arc::new(WebsocketInboundAdapter::new(
        "test_ws".to_string(),
        None,
        outbound.broadcast_tx(),
    ));

    let (_msg_tx, _msg_rx) = tokio::sync::mpsc::unbounded_channel::<InboundMessage>();
    let options = InboundAdapterOptions {
        on_message: Box::new(|_msg| Ok(())),
        on_thread_close: None,
        on_error: Box::new(|e| tracing::error!("Inbound error: {e}")),
        attachment_config: None,
    };
    inbound
        .start(options, CancellationToken::new())
        .await
        .unwrap();

    let token_home = tempfile::TempDir::new().unwrap();
    let _token = jyc_utils::inspect_token::generate_at(token_home.path()).unwrap();

    let (addr, server_handle, cancel) =
        spawn_test_server(inbound.clone(), token_home.path().to_path_buf()).await;

    // Wrong Bearer — should be rejected (401, NOT a 101 upgrade).
    let url = format!("ws://{}/ws/test_ws", addr);
    let req = build_ws_upgrade_request(&url, Some("jyc_definitely_not_the_real_token_0000"));
    let err = tokio_tungstenite::connect_async(req)
        .await
        .expect_err("WS upgrade with wrong Bearer should fail");
    let http = match err {
        tokio_tungstenite::tungstenite::Error::Http(response) => response,
        other => panic!("expected HTTP error (401), got: {other:?}"),
    };
    assert_eq!(http.status(), 401);
    let body_bytes = http.body().as_deref().unwrap_or(&[]);
    let body_str = std::str::from_utf8(body_bytes).unwrap_or("");
    assert!(
        body_str.contains("invalid token"),
        "expected 401 body to mention 'invalid token', got: {body_str}"
    );

    cancel.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}
