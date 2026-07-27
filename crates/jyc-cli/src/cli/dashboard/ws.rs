//! WebSocket client task backing the dashboard chat pane.

/// Events from the WebSocket client task.
#[derive(Debug)]
pub(super) enum WsEvent {
    Connected,
    Disconnected,
    Message(String),
    Error(String),
}

/// Runs a WebSocket client in a background task with auto-reconnect.
/// Exponential backoff from 1s to 30s max.
pub(super) async fn ws_client_task(
    url: String,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    event_tx: tokio::sync::mpsc::UnboundedSender<WsEvent>,
    auth_token: Option<String>,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let mut backoff = 1u64; // seconds

    'reconnect: loop {
        // Build the request with an optional Authorization header.
        let request = match url.as_str().into_client_request() {
            Ok(mut r) => {
                if let Some(token) = auth_token.as_deref() {
                    let value = format!("Bearer {token}");
                    r.headers_mut().insert(
                        "Authorization",
                        HeaderValue::from_str(&value)
                            .expect("static bearer header value is always valid"),
                    );
                }
                r
            }
            Err(e) => {
                let _ = event_tx.send(WsEvent::Error(format!("Bad URL: {e}")));
                let delay = std::cmp::min(backoff, 30);
                backoff = std::cmp::min(backoff * 2, 30);
                let sleep = tokio::time::sleep(tokio::time::Duration::from_secs(delay));
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        _ = &mut sleep => break,
                        cmd = cmd_rx.recv() => {
                            if cmd.is_none() {
                                break 'reconnect;
                            }
                        }
                    }
                }
                continue 'reconnect;
            }
        };

        // Attempt connection
        let (ws_stream, _) = match tokio_tungstenite::connect_async(request).await {
            Ok(v) => v,
            Err(e) => {
                let _ = event_tx.send(WsEvent::Error(format!("Connect failed: {e}")));
                // Wait for backoff before retrying, but check for clean shutdown
                let delay = std::cmp::min(backoff, 30);
                backoff = std::cmp::min(backoff * 2, 30);
                let sleep = tokio::time::sleep(tokio::time::Duration::from_secs(delay));
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        _ = &mut sleep => break,
                        cmd = cmd_rx.recv() => {
                            // Clean shutdown requested (user closed chat)
                            if cmd.is_none() {
                                break 'reconnect;
                            }
                        }
                    }
                }
                continue 'reconnect;
            }
        };

        // Reset backoff on successful connection
        backoff = 1;
        let _ = event_tx.send(WsEvent::Connected);

        let (mut write, mut read) = ws_stream.split();

        // Main message loop
        let connection_lost = loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            let _ = event_tx.send(WsEvent::Message(text));
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => {
                            break true;
                        }
                        Some(Err(e)) => {
                            let _ = event_tx.send(WsEvent::Error(format!("Read error: {e}")));
                            break true;
                        }
                        None => {
                            break true;
                        }
                        _ => {}
                    }
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(text) => {
                            if let Err(e) = write.send(
                                tokio_tungstenite::tungstenite::Message::Text(text)
                            ).await {
                                let _ = event_tx.send(WsEvent::Error(format!("Send error: {e}")));
                                break true;
                            }
                        }
                        None => break false, // Clean shutdown — cmd channel closed
                    }
                }
            }
        };

        if connection_lost {
            let _ = event_tx.send(WsEvent::Disconnected);
            // Backoff before reconnecting
            let delay = std::cmp::min(backoff, 30);
            backoff = std::cmp::min(backoff * 2, 30);
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
            // Continue reconnection loop
        } else {
            break; // Clean shutdown
        }
    }
}
