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
        on_close_event: None,
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

/// Verify that when the inspect server's shutdown token is cancelled, an
/// open WebSocket connection is force-closed by the handler's `select!`
/// arm — preventing axum's `with_graceful_shutdown` from waiting forever
/// on the still-open connection (which used to leave zombie `jyc serve`
/// processes after `/deploy`).
#[tokio::test]
async fn test_websocket_adapter_force_closes_on_shutdown() {
    use jyc_types::InboundAdapter;

    let (broadcast_tx, _broadcast_rx) = broadcast::channel(16);
    let tmp = tempfile::TempDir::new().unwrap();
    let storage = Arc::new(MessageStorage::new(tmp.path()));
    let outbound = WebsocketOutboundAdapter::new(broadcast_tx, storage);

    let shutdown = CancellationToken::new();
    let mut inbound = WebsocketInboundAdapter::new("test_ws".to_string(), outbound.broadcast_tx());
    inbound.set_ws_shutdown(shutdown.clone());
    let inbound = Arc::new(inbound);

    inbound
        .start(
            InboundAdapterOptions {
                on_message: Box::new(|_| Ok(())),
                on_topic_close: None,
                on_close_event: None,
                on_error: Box::new(|e| tracing::error!("{e}")),
                attachment_config: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let inbound_for_handler = inbound.clone();
    let app = axum::Router::new().route(
        "/ws",
        axum::routing::get(move |ws: axum::extract::ws::WebSocketUpgrade| {
            let inbound = inbound_for_handler.clone();
            async move {
                ws.on_upgrade(move |socket| async move {
                    let sock_addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
                    let _ = inbound.handle(socket, sock_addr, None).await;
                })
            }
        }),
    );
    // axum::serve without with_graceful_shutdown runs forever (waits for
    // listener error). Hook the shutdown token so axum returns when fired.
    let shutdown_for_axum = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown_for_axum.cancelled().await })
            .await
            .unwrap();
    });

    // Connect a client and hold the connection open (don't send anything).
    // When shutdown fires, drop the entire client stream so axum sees
    // both halves of the TCP socket close at once. Without this, with_graceful_shutdown
    // blocks indefinitely waiting for the open connection to drain.
    let url = format!("ws://{}/ws", addr);
    let client_stream = tokio_tungstenite::connect_async(&url).await.unwrap().0;
    let shutdown_for_client = shutdown.clone();
    let _client_guard = tokio::spawn(async move {
        shutdown_for_client.cancelled().await;
        drop(client_stream);
    });

    // Give the connection a moment to establish before cancelling.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Without the fix, axum waits forever on the still-open WS connection.
    // With the fix, the handler's `select!` arm fires, breaks the loop, and
    // the post-loop Close frame closes the server side. The client guard
    // drops the client side. axum then sees both halves closed and exits.
    shutdown.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), server_handle).await;
    assert!(
        result.is_ok(),
        "Server should exit within 5s of ws_shutdown cancellation, but timed out \
         (axum was waiting on the still-open WebSocket connection)"
    );
}
