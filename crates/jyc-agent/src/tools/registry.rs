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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct MockTool {
        name: String,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "mock tool"
        }

        fn input_schema(&self) -> Value {
            Value::Null
        }

        async fn execute(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput> {
            Ok(ToolOutput::success("executed"))
        }
    }

    #[test]
    fn new_registry_is_empty() {
        let reg = ToolRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn default_registry_is_empty() {
        let reg = ToolRegistry::default();
        assert!(reg.is_empty());
    }

    #[test]
    fn register_and_has_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "test".to_string(),
        }));
        assert!(reg.has_tool("test"));
        assert!(!reg.has_tool("missing"));
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    #[test]
    fn remove_existing_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "a".to_string(),
        }));
        reg.remove("a");
        assert!(!reg.has_tool("a"));
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "a".to_string(),
        }));
        reg.remove("nonexistent");
        assert!(reg.has_tool("a"));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn definitions_returns_all_tools() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "tool1".to_string(),
        }));
        reg.register(Box::new(MockTool {
            name: "tool2".to_string(),
        }));
        let defs = reg.definitions();
        assert_eq!(defs.len(), 2);
        assert!(defs.iter().any(|d| d.name == "tool1"));
        assert!(defs.iter().any(|d| d.name == "tool2"));
    }

    #[tokio::test]
    async fn execute_existing_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "mock".to_string(),
        }));
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        let result = reg.execute("mock", Value::Null, &ctx).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content, "executed");
    }

    #[tokio::test]
    async fn execute_missing_tool_returns_error() {
        let reg = ToolRegistry::new();
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        let result = reg.execute("missing", Value::Null, &ctx).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Tool 'missing' not found"));
    }
}
