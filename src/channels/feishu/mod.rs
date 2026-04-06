//! Feishu (Lark) channel implementation for JYC.
//! 
//! This module provides inbound and outbound adapters for the Feishu messaging platform.
//! It uses the openlark SDK for API interactions and WebSocket connections.

pub mod inbound;
pub mod outbound;
pub mod config;
pub mod types;
pub mod client;
pub mod validator;
pub mod websocket;
pub mod formatter;