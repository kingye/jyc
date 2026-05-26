//! WebSocket connection handler for WeChat OpenILink Bridge.
//!
//! Manages the lifecycle of a single WebSocket connection:
//! connect → receive events → parse JSON → convert to InboundMessage → callback.
//! Also provides a sender channel for outbound messages on the same connection.
//!
//! Auto-reconnect with exponential backoff and CancellationToken support.
