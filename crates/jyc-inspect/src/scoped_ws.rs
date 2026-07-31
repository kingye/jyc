//! `ScopedWsHandler` — wraps a per-channel `WebsocketHandler` and propagates
//! the URL-scoped thread name from `/ws/<channel>/<thread>` to the inner
//! handler.
//!
//! Used by the inspect server to expose a thread-scoped URL for
//! **websocket-type channels**. The inner handler can use the `scoped_thread`
//! parameter to populate per-connection state (e.g. `Message.topic` fallback)
//! without requiring the client to repeat the thread in the payload.
//!
//! For non-websocket channels, the inspect server uses `ThreadProxyHandler`
//! instead — see `thread_proxy.rs`.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;

use crate::server::WebsocketHandler;

/// A wrapper around a channel-specific `WebsocketHandler` that propagates
/// the URL-scoped thread name from `/ws/<channel>/<thread>` to the inner
/// handler.
pub struct ScopedWsHandler {
    inner: Arc<dyn WebsocketHandler<tokio::net::TcpStream>>,
}

impl ScopedWsHandler {
    pub fn new(inner: Arc<dyn WebsocketHandler<tokio::net::TcpStream>>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl WebsocketHandler for ScopedWsHandler {
    async fn handle(
        &self,
        ws_stream: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
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
            _ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
            _addr: SocketAddr,
            _scoped_thread: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn scoped_ws_handler_stores_inner() {
        let stub: Arc<dyn WebsocketHandler> = Arc::new(StubHandler);
        let scoped = ScopedWsHandler::new(stub);
        let _shared: Arc<ScopedWsHandler> = Arc::new(scoped);
    }
}
