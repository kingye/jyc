//! WebSocket client task backing the dashboard chat pane.

/// How often the client pings the server to prove the path is alive.
const WS_PING_INTERVAL: tokio::time::Duration = tokio::time::Duration::from_secs(15);

/// How long the socket may stay silent before it is treated as dead.
///
/// A broken tunnel — a VPN switching the network under it, a dropped ssh
/// forward — leaves the TCP connection half-open: no FIN arrives, so the
/// read never returns and the client cannot tell a dead link from an idle
/// topic. The server answers every ping with a pong (tungstenite does it at
/// the protocol layer, no application code involved), so silence for longer
/// than this window means the path is gone.
const WS_LIVENESS_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(45);

/// Heartbeat timings for the WebSocket client.
///
/// Split out from the constants so a test can use a much shorter window than
/// a human would ever wait for.
#[derive(Debug, Clone, Copy)]
pub(super) struct Heartbeat {
    /// How often to ping the server.
    pub(super) ping_interval: tokio::time::Duration,
    /// How long the socket may stay silent before it counts as dead.
    pub(super) liveness_timeout: tokio::time::Duration,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self {
            ping_interval: WS_PING_INTERVAL,
            liveness_timeout: WS_LIVENESS_TIMEOUT,
        }
    }
}

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
    heartbeat: Heartbeat,
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
                            .expect("token is hex so always a valid header value"),
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

        // Liveness watch: ping every interval, and treat the socket as dead
        // when nothing at all comes back for the whole timeout window. Any
        // frame counts — the pong that answers our ping included.
        let mut ping = tokio::time::interval(heartbeat.ping_interval);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is due immediately; spend it here so the first ping
        // goes out one interval after the connection opened.
        ping.tick().await;
        let mut last_frame = tokio::time::Instant::now();

        // Main message loop
        let connection_lost = loop {
            tokio::select! {
                msg = read.next() => {
                    if msg.as_ref().is_some_and(|m| m.is_ok()) {
                        last_frame = tokio::time::Instant::now();
                    }
                    match msg {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            let _ = event_tx.send(WsEvent::Message(text.to_string()));
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
                                tokio_tungstenite::tungstenite::Message::Text(text.into())
                            ).await {
                                let _ = event_tx.send(WsEvent::Error(format!("Send error: {e}")));
                                break true;
                            }
                        }
                        None => break false, // Clean shutdown — cmd channel closed
                    }
                }
                _ = ping.tick() => {
                    if last_frame.elapsed() >= heartbeat.liveness_timeout {
                        let _ = event_tx.send(WsEvent::Error(format!(
                            "No data from the server for {:?} — the connection is likely dead",
                            heartbeat.liveness_timeout
                        )));
                        break true;
                    }
                    if let Err(e) = write.send(
                        tokio_tungstenite::tungstenite::Message::Ping(Vec::new().into())
                    ).await {
                        let _ = event_tx.send(WsEvent::Error(format!("Ping failed: {e}")));
                        break true;
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::time::Duration;
    use tokio::net::TcpListener;

    /// Heartbeat timings short enough to keep a test quick, long enough that
    /// a loaded machine does not trip them by accident.
    fn fast_heartbeat() -> Heartbeat {
        Heartbeat {
            ping_interval: Duration::from_millis(50),
            liveness_timeout: Duration::from_millis(200),
        }
    }

    /// The events the client emits within `window`.
    async fn collect_events(
        event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<WsEvent>,
        window: Duration,
    ) -> Vec<WsEvent> {
        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            match tokio::time::timeout(deadline - now, event_rx.recv()).await {
                Ok(Some(event)) => events.push(event),
                Ok(None) | Err(_) => break,
            }
        }
        events
    }

    /// A server that completes the handshake and then never reads again.
    ///
    /// Tungstenite answers a ping when it *reads* the frame, so a server that
    /// stops reading leaves the socket open and silent — the shape a broken
    /// tunnel has: no FIN, no data, no error.
    #[tokio::test]
    async fn a_silent_server_is_reported_as_a_dead_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("handshake");
            std::future::pending::<()>().await;
        });

        let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let client = tokio::spawn(ws_client_task(
            format!("ws://{addr}/"),
            cmd_rx,
            event_tx,
            None,
            fast_heartbeat(),
        ));

        let events = collect_events(&mut event_rx, Duration::from_secs(3)).await;
        let errors: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                WsEvent::Error(err) => Some(err.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            errors
                .iter()
                .any(|err| err.contains("No data from the server")),
            "a silent server must be reported as a dead link: {errors:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, WsEvent::Disconnected)),
            "a silent server must end the connection: {events:?}"
        );

        client.abort();
        server.abort();
    }

    /// The other half of the contract: a server that answers the pings keeps
    /// the connection, however idle the topic is.
    #[tokio::test]
    async fn a_live_server_keeps_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("handshake");
            // Reading the frames is what makes tungstenite answer the pings.
            while let Some(Ok(_)) = ws.next().await {}
        });

        let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let client = tokio::spawn(ws_client_task(
            format!("ws://{addr}/"),
            cmd_rx,
            event_tx,
            None,
            fast_heartbeat(),
        ));

        let events = collect_events(&mut event_rx, Duration::from_millis(800)).await;
        assert!(
            matches!(events.first(), Some(WsEvent::Connected)),
            "the client must report the connection: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, WsEvent::Disconnected | WsEvent::Error(_))),
            "a live server must not be dropped: {events:?}"
        );

        client.abort();
        server.abort();
    }
}
