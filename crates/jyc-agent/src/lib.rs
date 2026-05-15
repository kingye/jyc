//! In-process AI agent for the JYC framework.
//!
//! Replaces the external OpenCode server with a self-contained Rust agent
//! that runs LLM inference and tool execution in-process.

pub mod provider;
pub mod tools;
pub mod types;
pub mod agent_loop;
pub mod service;

pub use service::JycAgentService;
