//! `ScopedWsHandler` — wraps a per-channel `WebsocketInboundAdapter` and
//! auto-sends a `subscribe {thread}` message immediately on connection.
//!
//! Used by the inspect server to expose a thread-scoped URL
//! (`/ws/<channel>/<thread>`) for **websocket-type channels** — so the
//! dashboard doesn't have to send `subscribe` itself. The wrapped adapter
//! is unaware of the URL scope; it just sees a `subscribe` as the first
//! client message and proceeds with its existing protocol (load history,
//! process incoming messages, etc.).
//!
//! For non-websocket channels, the inspect server uses `ThreadProxyHandler`
//! instead — see `thread_proxy.rs`.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::SinkExt;

use crate::server::{PrependStream, WebsocketHandler};

/// A wrapper around a channel-specific `WebsocketInboundAdapter` that
/// auto-injects a `subscribe` message on connect, binding the WebSocket
/// session to a specific thread.
pub struct ScopedWsHandler {
    inner: Arc<dyn WebsocketHandler>,
    thread: String,
}

impl ScopedWsHandler {
    pub fn new(inner: Arc<dyn WebsocketHandler>, thread: String) -> Self {
        Self { inner, thread }
    }
}

#[async_trait]
impl WebsocketHandler for ScopedWsHandler {
    async fn handle(
        &self,
        mut ws_stream: tokio_tungstenite::WebSocketStream<PrependStream>,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        // Pre-send a subscribe message so the inner adapter binds the
        // session to our thread. The inner adapter treats this like any
        // other client message and loads history for the thread.
        let subscribe_msg = serde_json::json!({
            "type": "subscribe",
            "thread": self.thread,
        })
        .to_string();
        ws_stream
            .send(tokio_tungstenite::tungstenite::Message::Text(subscribe_msg))
            .await?;

        tracing::debug!(
            addr = %addr,
            thread = %self.thread,
            "ScopedWsHandler: pre-sent subscribe, delegating to inner handler"
        );

        // Delegate the rest of the connection to the wrapped handler.
        self.inner.handle(ws_stream, addr).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct StubHandler;

    #[async_trait]
    impl WebsocketHandler for StubHandler {
        async fn handle(
            &self,
            _ws: tokio_tungstenite::WebSocketStream<PrependStream>,
            _addr: SocketAddr,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn scoped_ws_handler_stores_thread() {
        let stub: Arc<dyn WebsocketHandler> = Arc::new(StubHandler);
        let scoped = ScopedWsHandler::new(stub, "my-thread".to_string());
        assert_eq!(scoped.thread, "my-thread");
    }

    #[test]
    fn scoped_ws_handler_accepts_any_inner() {
        // The inner type can be any WebsocketHandler (including the
        // real WebsocketInboundAdapter). ScopedWsHandler is a transparent
        // wrapper.
        let stub: Arc<dyn WebsocketHandler> = Arc::new(StubHandler);
        let _scoped: ScopedWsHandler = ScopedWsHandler::new(stub, "x".to_string());
    }
}
