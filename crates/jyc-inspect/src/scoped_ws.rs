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

use async_trait::async_trait;

use crate::server::{DynWebsocketHandler, WebsocketHandler};

/// A wrapper around a channel-specific `WebsocketHandler` that propagates
/// the URL-scoped thread name from `/ws/<channel>/<thread>` to the inner
/// handler.
pub struct ScopedWsHandler {
    inner: DynWebsocketHandler,
}

impl ScopedWsHandler {
    pub fn new(inner: DynWebsocketHandler) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl WebsocketHandler for ScopedWsHandler {
    async fn handle(
        &self,
        ws: axum::extract::ws::WebSocket,
        addr: SocketAddr,
        scoped_thread: Option<&str>,
    ) -> anyhow::Result<()> {
        tracing::debug!(
            addr = %addr,
            thread = scoped_thread.unwrap_or("?"),
            "ScopedWsHandler: delegating to inner handler with scoped_thread"
        );
        self.inner.handle(ws, addr, scoped_thread).await
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
            _ws: axum::extract::ws::WebSocket,
            _addr: SocketAddr,
            _scoped_thread: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn scoped_ws_handler_stores_inner() {
        let stub: DynWebsocketHandler = std::sync::Arc::new(StubHandler);
        let scoped = ScopedWsHandler::new(stub);
        let _shared: std::sync::Arc<ScopedWsHandler> = std::sync::Arc::new(scoped);
    }
}
