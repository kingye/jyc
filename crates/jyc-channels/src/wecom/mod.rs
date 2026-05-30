//! WeCom (企业微信) channel implementation for JYC.
//!
//! This module provides inbound and outbound adapters for the WeCom messaging platform.
//! WeCom uses a shared HTTP server (axum) for receiving callback messages and
//! Bot webhook URLs for sending messages.
//!
//! Architecture:
//! - Inbound: Shared axum HTTP server at `/webhook/{channel_name}`, all channels
//!   share one server instance (global `wecom.bind_addr`).
//! - Outbound: POST to Bot webhook URL for each channel.
//! - One channel = one Bot = one fixed thread (similar to WeChat).

pub mod crypto;
pub mod inbound;
pub mod kf_client;
pub mod kf_cursor;
pub mod kf_dedup;
pub mod kf_inbound;
pub mod kf_outbound;
pub mod outbound;
pub mod server;
pub mod token_cache;
