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

use crate::server::{PrependStream, WebsocketHandler};

/// A wrapper around a channel-specific `WebsocketHandler` that propagates
/// the URL-scoped thread name from `/ws/<channel>/<thread>` to the inner
/// handler. The inner handler can use the `scoped_thread` parameter to
/// populate per-connection state (e.g. `Message.topic` fallback) without
/// requiring the client to repeat the thread in the payload.
pub struct ScopedWsHandler {
    inner: Arc<dyn WebsocketHandler>,
}

impl ScopedWsHandler {
    pub fn new(inner: Arc<dyn WebsocketHandler>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl WebsocketHandler for ScopedWsHandler {
    async fn handle(
        &self,
        ws_stream: tokio_tungstenite::WebSocketStream<PrependStream>,
        addr: SocketAddr,
        scoped_thread: Option<&str>,
    ) -> anyhow::Result<()> {
        tracing::debug!(
            addr = %addr,
            thread = scoped_thread.unwrap_or("?"),
            "ScopedWsHandler: delegating to inner handler with scoped_thread"
        );

        self.inner.handle(ws_stream, addr, scoped_thread).await
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
            _scoped_thread: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn scoped_ws_handler_stores_inner() {
        // The scoped handler now just wraps the inner handler. The thread
        // name comes from the URL via scoped_thread; we don't store it
        // on the struct.
        let stub: Arc<dyn WebsocketHandler> = Arc::new(StubHandler);
        let scoped = ScopedWsHandler::new(stub);
        // Verify the handler can be constructed and Arc-shared.
        let _shared: Arc<ScopedWsHandler> = Arc::new(scoped);
    }

    #[test]
    fn scoped_ws_handler_accepts_any_inner() {
        // The inner type can be any WebsocketHandler (including the
        // real WebsocketInboundAdapter). ScopedWsHandler is a transparent
        // wrapper.
        let stub: Arc<dyn WebsocketHandler> = Arc::new(StubHandler);
        let _scoped: ScopedWsHandler = ScopedWsHandler::new(stub);
    }
}
