//! Tool registry — collects all available tools and provides definitions to the LLM.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;

use super::{Tool, ToolContext, ToolOutput};
use crate::types::ToolDefinition;

/// Registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Remove a tool by name. No-op if the tool is not registered.
    pub fn remove(&mut self, name: &str) {
        self.tools.remove(name);
    }

    /// Get all tool definitions for the LLM.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.to_definition()).collect()
    }

    /// Execute a tool by name.
    pub async fn execute(
        &self,
        name: &str,
        input: Value,
        ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        let tool = self.tools.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "Tool '{}' not found. Available: {:?}",
                name,
                self.tools.keys().collect::<Vec<_>>()
            )
        })?;

        tool.execute(input, ctx).await
    }

    /// Check if a tool exists.
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
