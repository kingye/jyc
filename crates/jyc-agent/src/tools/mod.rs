//! Tool system for the agent.
//!
//! Defines the `Tool` trait and built-in tool implementations.

pub mod builtin;
pub mod registry;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

use crate::types::ToolDefinition;

/// Context provided to tools during execution.
pub struct ToolContext<'a> {
    /// Working directory for the tool.
    pub working_dir: &'a Path,
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
