//! WeChat channel implementation for JYC.
//!
//! This module provides inbound and outbound adapters for the WeChat messaging platform
//! via the OpenILink WebSocket Bridge. Both inbound and outbound messages share the
//! same WebSocket connection. One bot corresponds to one fixed thread.
//!
//! Architecture differs from Feishu:
//! - Inbound + outbound share ONE WebSocket connection (vs Feishu's WS inbound + HTTP outbound)
//! - One bot = one fixed thread (vs Feishu's multi-chat architecture)
//! - Pure text messages only in v1 (rich media can be added later)

pub mod inbound;
pub mod outbound;
pub mod websocket;
