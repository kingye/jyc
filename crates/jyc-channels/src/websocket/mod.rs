//! WebSocket channel implementation.
//!
//! Provides inbound and outbound adapters for WebSocket-based AI interaction.
//! Runs inside `jyc monitor` and accepts connections from `jyc dashboard` chat panes.

pub mod inbound;
pub mod outbound;

pub use inbound::{WebsocketInboundAdapter, WebsocketMatcher};
pub use outbound::WebsocketOutboundAdapter;
