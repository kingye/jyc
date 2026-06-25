use std::collections::HashMap;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use jyc_channels::websocket::{WebsocketInboundAdapter, WebsocketOutboundAdapter};
use jyc_core::message_storage::MessageStorage;
use jyc_inspect::server::WebsocketHandler;
use jyc_types::{
    ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage, MessageContent,
    OutboundAdapter,
};

#[tokio::test]
async fn test_websocket_adapter_start_and_handle() {
    let (broadcast_tx, _broadcast_rx) = broadcast::channel(16);
    let tmp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(MessageStorage::new(tmp.path()));
    let outbound = WebsocketOutboundAdapter::new(broadcast_tx.clone(), storage);

    let patterns = vec![
        ChannelPattern {
            name: "general".to_string(),
            channel: "websocket".to_string(),
            enabled: true,
            ..Default::default()
        },
        ChannelPattern {
            name: "coding-help".to_string(),
            channel: "websocket".to_string(),
            enabled: true,
            ..Default::default()
        },
        ChannelPattern {
            name: "disabled".to_string(),
            channel: "websocket".to_string(),
            enabled: false,
            ..Default::default()
        },
    ];

    let inbound = Arc::new(WebsocketInboundAdapter::new(
        "test-ws".to_string(),
        patterns,
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

    let cancel = CancellationToken::new();

    // Start the inbound adapter (sets the on_message callback)
    inbound.start(options, cancel.clone()).await.unwrap();

    // Bind a local TCP listener to simulate the inspect server
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Spawn a task that accepts a connection and hands it to the handler
    let handler = inbound.clone();
    let server_handle = tokio::spawn(async move {
        let (mut stream, client_addr) = listener.accept().await.unwrap();

        // Read the first byte to detect protocol (same logic as inspect server)
        let mut first_byte = [0u8; 1];
        let n = tokio::io::AsyncReadExt::read(&mut stream, &mut first_byte)
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(first_byte[0], b'G');

        // Prepend the byte back and perform WebSocket handshake
        let stream = jyc_inspect::server::PrependStream::new(stream, vec![first_byte[0]]);
        let ws_stream = tokio_tungstenite::accept_async(stream).await.unwrap();

        handler.handle(ws_stream, client_addr).await.unwrap();
    });

    // Give the server a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Connect test client
    let url = format!("ws://{}/ws", addr);
    let (ws_stream, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut write, mut read) = ws_stream.split();

    // Send list_patterns request
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

    // Verify the message was routed
    let received = tokio::time::timeout(tokio::time::Duration::from_secs(5), msg_rx.recv())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(received.channel, "test-ws");
    assert_eq!(received.topic, "general");
    assert_eq!(received.content.text.as_ref().unwrap(), message_text);
    assert_eq!(received.sender, "user");

    // Close connection
    let _ = write
        .send(tokio_tungstenite::tungstenite::Message::Close(None))
        .await;

    // Wait for server to shut down
    let _ = tokio::time::timeout(tokio::time::Duration::from_secs(5), server_handle).await;
}

#[tokio::test]
async fn test_websocket_broadcast_reply() {
    let (broadcast_tx, mut broadcast_rx) = broadcast::channel(16);
    let tmp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(MessageStorage::new(tmp.path()));
    let outbound = WebsocketOutboundAdapter::new(broadcast_tx, storage);

    let message = InboundMessage {
        id: "test".to_string(),
        channel: "websocket".to_string(),
        channel_uid: "user".to_string(),
        sender: "user".to_string(),
        sender_address: "user".to_string(),
        recipients: vec![],
        topic: "general".to_string(),
        content: MessageContent {
            text: Some("hello".to_string()),
            html: None,
            markdown: None,
        },
        timestamp: chrono::Utc::now(),
        thread_refs: None,
        reply_to_id: None,
        external_id: None,
        attachments: vec![],
        metadata: HashMap::new(),
        matched_pattern: None,
    };

    // Send reply should broadcast
    let result = outbound
        .send_reply(
            &message,
            "AI reply",
            std::path::Path::new("/tmp"),
            "msg_001",
            None,
        )
        .await;
    assert!(result.is_ok());

    let sent = broadcast_rx.recv().await.expect("should receive broadcast");
    let parsed: serde_json::Value = serde_json::from_str(&sent).unwrap();
    assert_eq!(parsed["type"], "reply");
    assert_eq!(parsed["thread"], "general");
    assert_eq!(parsed["text"], "AI reply");
}
