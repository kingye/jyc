//! Local TUI channel implementation.
//!
//! Provides inbound and outbound adapters for terminal-based AI interaction
//! via ratatui + crossterm. Each `jyc local` instance is an independent
//! process with its own stdin/stdout, workspace, and agent context.

pub mod inbound;
pub mod outbound;

pub use inbound::{LocalInboundAdapter, LocalMatcher};
pub use outbound::LocalOutboundAdapter;
