//! OpeniLink (WeChat) channel implementation for JYC.
//!
//! This module provides inbound and outbound adapters for the OpeniLink
//! messaging platform, which bridges WeChat messages through a Hub server.
//! It uses WebSocket for receiving messages and HTTP API for sending replies.

pub mod types;
pub mod client;
pub mod inbound;
pub mod outbound;
pub mod websocket;
pub mod formatter;
