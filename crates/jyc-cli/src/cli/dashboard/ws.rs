//! WebSocket client task backing the dashboard chat pane.

use tokio_tungstenite::tungstenite::handshake::client::Request;

/// Events from the WebSocket client task.
#[derive(Debug)]
pub(super) enum WsEvent {
    Connected,
    Disconnected,
    Message(String),
    Error(String),
}

/// Resolves the auth token for a fresh WebSocket connection.
///
/// Closure invoked on each (re)connect attempt: returns the current
/// `Authorization: Bearer …` value to send in the upgrade request, or
/// `None` to skip the header. Reads the on-disk token file each time so
/// `jyc token rotate` takes effect automatically.
pub(super) type TokenResolver = Box<dyn Fn() -> Option<String> + Send + Sync>;

/// Runs a WebSocket client in a background task with auto-reconnect.
/// Exponential backoff from 1s to 30s max.
pub(super) async fn ws_client_task(
    url: String,
    token_resolver: TokenResolver,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    event_tx: tokio::sync::mpsc::UnboundedSender<WsEvent>,
) {
    use futures_util::{SinkExt, StreamExt};

    let mut backoff = 1u64; // seconds

    'reconnect: loop {
        // Build the upgrade request. Re-read the token on every attempt so
        // rotation is picked up without restarting the dashboard.
        let request = match build_request(&url, &*token_resolver) {
            Ok(r) => r,
            Err(e) => {
                let _ = event_tx.send(WsEvent::Error(format!("Bad WS URL: {e}")));
                break;
            }
        };

        // Attempt connection with the auth header (if any).
        let connect_result = tokio_tungstenite::connect_async(request).await;

        let ws_stream = match connect_result {
            Ok((s, _)) => s,
            Err(e) => {
                let _ = event_tx.send(WsEvent::Error(format!("Connect failed: {e}")));
                // Wait for backoff before retry, but check for clean shutdown
                let delay = std::cmp::min(backoff, 30);
                backoff = std::cmp::min(backoff * 2, 30);
                let sleep = tokio::time::sleep(std::time::Duration::from_secs(delay));
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
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            // Continue reconnection loop
        } else {
            break; // Clean shutdown
        }
    }
}

/// Builds a WebSocket upgrade `Request` from `url`, attaching
/// `Authorization: Bearer <token>` when `token_resolver` returns `Some`.
fn build_request(
    url: &str,
    token_resolver: &dyn Fn() -> Option<String>,
) -> anyhow::Result<Request> {
    let mut builder = Request::builder().method("GET").uri(url);
    if let Some(token) = token_resolver() {
        let header_value = format!("Bearer {token}");
        builder = builder.header("Authorization", header_value);
    }
    let request = builder.body(())?;
    Ok(request)
}
