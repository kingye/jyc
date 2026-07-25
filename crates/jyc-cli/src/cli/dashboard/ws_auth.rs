//! Build an authenticated WebSocket upgrade request.
//!
//! Mirrors `InspectClient`'s auth behavior: reads `JYC_INSPECT_TOKEN`
//! first, then falls back to the on-disk `<data_dir>/inspect-token`.
//! Returns a `Request<()>` ready to pass to `tokio_tungstenite::connect_async`.
//!
//! The dashboard's two direct WebSocket clients (chat pane reconnect
//! loop and `create_thread_via_websocket`) both go through this helper
//! so they pick up the same `Authorization: Bearer …` header as the
//! HTTP `InspectClient` once a token file is configured.

use http::Request;
use http::header::AUTHORIZATION;

/// Build a GET upgrade request for `url`, attaching
/// `Authorization: Bearer <token>` if a token resolves.
///
/// Returns the bare request when no token is configured (default
/// install) — the server allows unauthenticated requests in that case.
pub fn build_authenticated_ws_request(url: &str) -> Request<()> {
    let mut req = Request::builder()
        .method("GET")
        .uri(url)
        .body(())
        .expect("WS upgrade request URI is always valid");
    if let Some(token) = jyc_inspect::client::resolve_token() {
        let value = format!("Bearer {token}")
            .parse()
            .expect("Bearer header value is always valid ASCII");
        req.headers_mut().insert(AUTHORIZATION, value);
    }
    req
}
