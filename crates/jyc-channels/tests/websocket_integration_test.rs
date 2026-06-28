use arc_swap::ArcSwap;
use futures_util::{SinkExt, StreamExt};
use jyc_channels::websocket::inbound::WebsocketInboundAdapter;
use jyc_channels::websocket::outbound::WebsocketOutboundAdapter;
use jyc_types::{InboundAdapter, InboundAdapterOptions, InboundMessage};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;

#[tokio::test]
async fn test_websocket_adapter_start_and_handle() {
    // Create a minimal AppConfig with websocket patterns
    let config_str = r#"
[agent]
mode = "agent"
system_prompt = "test"

[[channels.test_ws]]
type = "websocket"

[[channels.test_ws.patterns]]
name = "general"
enabled = true

[[channels.test_ws.patterns]]
name = "coding-help"
enabled = true

[[channels.test_ws.patterns]]
name = "disabled"
enabled = false
"#;
    let app_config: jyc_types::AppConfig = toml::from_str(config_str).unwrap();
    let config_arc = Arc::new(ArcSwap::from_pointee(app_config));

    let outbound = WebsocketOutboundAdapter::new_for_test();
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
        .start(options, tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();

    // Create a client connection
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws_stream = accept_async(stream).await.unwrap();
        let handler = inbound.clone();
        jyc_inspect::server::WebsocketHandler::handle(
            &handler,
            ws_stream,
            "127.0.0.1:0".parse().unwrap(),
        )
        .await
        .unwrap();
    });

    // Connect client
    let client_url = format!("ws://127.0.0.1:{}", port);
    let ws_stream = tokio_tungstenite::connect_async(&client_url)
        .await
        .unwrap()
        .0;
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
    assert_eq!(
        inbound_msg.thread_refs.as_ref().unwrap(),
        &["general".to_string()]
    );
    assert_eq!(inbound_msg.content.text.unwrap(), message_text);

    server_task.abort();
}
