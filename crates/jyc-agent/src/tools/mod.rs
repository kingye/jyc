//! Tool system for the agent.
//!
//! Defines the `Tool` trait and built-in tool implementations.

pub mod builtin;
pub mod mcp_bridge;
pub mod mcp_client;
pub mod registry;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::types::{ImageSource, ToolDefinition};

/// Context provided to tools during execution.
pub struct ToolContext<'a> {
    /// Working directory for the tool.
    pub working_dir: &'a Path,
    /// Additional absolute paths the agent may legitimately read from
    /// outside `working_dir` (currently: a configured absolute
    /// `[attachments.inbound].save_path`). Tools that enforce a path
    /// boundary (e.g. `read_image`) accept paths under any of these
    /// roots in addition to `working_dir`.
    pub additional_read_roots: Vec<PathBuf>,
    /// Side-channel for tools (e.g. `read_image`) that need to inject
    /// additional content blocks into the *next* user turn alongside
    /// the textual tool result. The agent loop drains this after each
    /// batch of tool calls and emits a synthetic user turn carrying the
    /// images. `Mutex<Vec<_>>` to allow tools with `&self` execution to
    /// push without requiring `&mut ToolContext`.
    pub pending_images: Mutex<Vec<ImageSource>>,
}

impl<'a> ToolContext<'a> {
    /// Construct a context with no extra roots and an empty pending-images queue.
    pub fn new(working_dir: &'a Path) -> Self {
        Self {
            working_dir,
            additional_read_roots: Vec::new(),
            pending_images: Mutex::new(Vec::new()),
        }
    }

    /// Construct a context with extra absolute read roots.
    pub fn with_roots(working_dir: &'a Path, additional_read_roots: Vec<PathBuf>) -> Self {
        Self {
            working_dir,
            additional_read_roots,
            pending_images: Mutex::new(Vec::new()),
        }
    }

    /// Drain and return any pending image sources accumulated during the
    /// current tool-execution batch. Called by the agent loop after the
    /// batch completes.
    pub fn take_pending_images(&self) -> Vec<ImageSource> {
        std::mem::take(&mut *self.pending_images.lock().expect("pending_images poisoned"))
    }
}

/// Trait for tools that can be invoked by the agent.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (used in LLM tool_use).
    fn name(&self) -> &str;

    /// Tool description (shown to LLM).
    fn description(&self) -> &str;

    /// JSON Schema for the tool's input parameters.
    fn input_schema(&self) -> Value;

    /// Execute the tool with the given input.
    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput>;

    /// Convert to a ToolDefinition for the LLM.
    fn to_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

/// Output from a tool execution.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// The tool's text output.
    pub content: String,
    /// Whether the execution resulted in an error.
    pub is_error: bool,
}

impl ToolOutput {
    /// Create a successful output.
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// Create an error output.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}
