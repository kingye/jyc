use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use futures_util::{SinkExt, StreamExt};
use jyc_channels::websocket::inbound::WebsocketInboundAdapter;
use jyc_channels::websocket::outbound::WebsocketOutboundAdapter;
use jyc_core::message_storage::MessageStorage;
use jyc_inspect::server::WebsocketHandler;
use jyc_types::{InboundAdapter, InboundAdapterOptions, InboundMessage};
use tokio::io::AsyncReadExt;
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

    // Bind a local TCP listener to simulate the inspect server
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        let (mut stream, client_addr) = listener.accept().await.unwrap();

        // Read the first byte to detect protocol (same logic as inspect server)
        let mut first_byte = [0u8; 1];
        let n = stream.read_exact(&mut first_byte).await.unwrap();
        assert_eq!(n, 1);
        assert_eq!(first_byte[0], b'G');

        // Prepend the byte back and perform WebSocket handshake
        let stream = jyc_inspect::server::PrependStream::new(stream, vec![first_byte[0]]);
        let ws_stream = tokio_tungstenite::accept_async(stream).await.unwrap();

        inbound.handle(ws_stream, client_addr).await.unwrap();
    });

    // Connect test client
    let url = format!("ws://{}/ws", addr);
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

    // Wait for server to shut down
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
}
