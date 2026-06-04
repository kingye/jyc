//! WeCom Smart Robot (wecom_bot) channel implementation.
//!
//! Provides inbound and outbound adapters for WeCom Smart Robot via
//! WebSocket long connection (`wss://openws.work.weixin.qq.com`).
//!
//! ## Architecture
//!
//! - `client`: WebSocket client with auto-reconnect, heartbeat, and shared sender
//! - `inbound`: Adapter that receives messages/events and routes them
//! - `outbound`: Adapter that sends replies and proactive messages
//! - `types`: Message/event structure definitions for the WebSocket protocol
//!
//! ## Usage
//!
//! ```toml
//! [channels.my_wecom_bot]
//! type = "wecom_bot"
//! [channels.my_wecom_bot.wecom_bot]
//! bot_id = "your_bot_id"
//! secret = "${WECOM_BOT_SECRET}"
//! ```
//!
//! Reference:
//! - doc 101463: Smart Robot WebSocket Long Connection
//! - doc 100719: Receiving Messages (JSON format)
//! - doc 101031: Passive Reply Messages (including streaming)

pub mod client;
pub mod inbound;
pub mod media;
pub mod outbound;
pub mod types;

pub use client::{ConnectionState, ServerMessage, WecomBotWsClient};
pub use inbound::{WecomBotInboundAdapter, WecomBotMatcher};
pub use outbound::WecomBotOutboundAdapter;
pub use types::*;
