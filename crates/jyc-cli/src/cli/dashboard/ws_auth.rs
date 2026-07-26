//! Build an authenticated WebSocket upgrade request.
//!
//! Re-exported from `jyc_inspect::client::build_authenticated_ws_request`
//! for historical reasons — the helper was originally defined here. The
//! canonical implementation lives in `jyc-inspect` so the integration
//! test in `crates/jyc-channels/tests/websocket_integration_test.rs`
//! can exercise the same code path that ships to users.

pub use jyc_inspect::client::build_authenticated_ws_request;
