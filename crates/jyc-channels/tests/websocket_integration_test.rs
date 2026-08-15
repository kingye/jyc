use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use jyc_channels::websocket::inbound::WebsocketInboundAdapter;
use jyc_channels::websocket::outbound::WebsocketOutboundAdapter;
use jyc_core::message_storage::MessageStorage;
use jyc_inspect::server::WebsocketHandler;
use jyc_types::{InboundAdapter, InboundAdapterOptions, InboundMessage};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_websocket_adapter_start_and_handle() {
    let (broadcast_tx, _broadcast_rx) = broadcast::channel(16);
    let tmp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(MessageStorage::new(tmp.path()));
    let outbound = WebsocketOutboundAdapter::new(broadcast_tx, storage);
    let inbound = Arc::new(WebsocketInboundAdapter::new(
        "test_ws".to_string(),
        outbound.broadcast_tx(),
    ));

    // Capture incoming messages
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<InboundMessage>();

    let options = InboundAdapterOptions {
        on_message: Box::new(move |msg: InboundMessage| {
            let _ = msg_tx.send(msg);
            Ok(())
        }),
        on_topic_close: None,
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

    // With the axum-based inspect server, the WebSocketUpgrade extractor
    // performs the HTTP upgrade itself and hands the handler an
    // `axum::extract::ws::WebSocket` (not a raw tungstenite stream).
    // The simplest way to reproduce that flow is to spin up a minimal
    // axum router whose only job is to accept the WS upgrade and hand
    // the resulting WebSocket to the inbound adapter — same as the
    // production WS dispatch in jyc-inspect's server.
    let inbound_for_handler = inbound.clone();
    let app = axum::Router::new().route(
        "/ws",
        axum::routing::get(move |ws: axum::extract::ws::WebSocketUpgrade| {
            let inbound = inbound_for_handler.clone();
            async move {
                ws.on_upgrade(move |socket| async move {
                    let addr = "127.0.0.1:0".parse().unwrap();
                    if let Err(e) = inbound.handle(socket, addr, None).await {
                        tracing::warn!(error = %e, "WebSocket handler failed");
                    }
                })
            }
        }),
    );
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Connect test client
    let url = format!("ws://{}/ws", addr);
    let ws_stream = tokio_tungstenite::connect_async(&url).await.unwrap().0;
    let (mut write, _read) = ws_stream.split();

    // Send a message — the URL is /ws (no topic scope), so the message
    // payload must include `topic`. The `list_patterns` and `subscribe`
    // commands have been replaced by REST endpoints; the WebSocket
    // protocol now only carries the live-message stream.
    let message_text = "Hello from test client";
    let message_msg = format!(
        r#"{{"type":"message","topic":"general","text":"{}"}}"#,
        message_text
    );
    write
        .send(tokio_tungstenite::tungstenite::Message::Text(
            message_msg.into(),
        ))
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
