//! MCP client — dynamically load tools from external MCP servers.
//!
//! Connects to local (subprocess) or remote (HTTP) MCP servers via the rmcp
//! protocol, calls `list_tools()`, and wraps each discovered tool as a
//! jyc-agent `Tool` implementation for the agent loop.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use serde_json::Value;
use tracing;

use jyc_types::McpServerConfig;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService, serve_client};
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};

use crate::tools::{Tool, ToolContext, ToolOutput};

/// Load all tools from a set of MCP server configurations.
///
/// Connects to each MCP server, calls `list_tools()`, and wraps each
/// discovered tool as an `McpToolWrapper`. Failed connections are logged
/// and skipped (graceful degradation).
pub async fn load_mcp_tools(cfgs: &[McpServerConfig]) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();

    for cfg in cfgs {
        match connect_and_list_tools(cfg).await {
            Ok(mut discovered) => {
                tracing::info!(
                    mcp_name = %cfg.name,
                    tool_count = discovered.len(),
                    "Loaded MCP tools"
                );
                tools.append(&mut discovered);
            }
            Err(e) => {
                tracing::warn!(
                    mcp_name = %cfg.name,
                    error = %e,
                    "Failed to load MCP tools, skipping"
                );
            }
        }
    }

    tools
}

/// Connect to an MCP server and list its tools.
async fn connect_and_list_tools(cfg: &McpServerConfig) -> Result<Vec<Box<dyn Tool>>> {
    let service: RunningService<RoleClient, ()> = match &cfg.kind {
        jyc_types::McpServerKind::Local {
            command,
            environment,
        } => {
            let mut cmd = tokio::process::Command::new(&command[0]);
            if command.len() > 1 {
                cmd.args(&command[1..]);
            }
            for (k, v) in environment {
                cmd.env(k, v);
            }
            cmd.stdin(std::process::Stdio::piped());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::inherit());

            let transport = TokioChildProcess::new(cmd)
                .map_err(|e| anyhow::anyhow!("failed to start MCP subprocess: {}", e))
                .context("TokioChildProcess::new failed")?;

            serve_client((), transport)
                .await
                .map_err(|e| anyhow::anyhow!("failed to connect to MCP server via stdio: {}", e))?
        }
        jyc_types::McpServerKind::Remote {
            url,
            enabled,
            auth_header,
            custom_headers,
        } => {
            if !enabled {
                anyhow::bail!("remote MCP '{}' is disabled", cfg.name);
            }

            let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
            if let Some(token) = auth_header {
                config = config.auth_header(token.clone());
            }
            if !custom_headers.is_empty() {
                let headers: Result<HashMap<HeaderName, HeaderValue>> = custom_headers
                    .iter()
                    .map(|(k, v)| {
                        let name = HeaderName::from_str(k)
                            .map_err(|e| anyhow::anyhow!("invalid header name '{}': {}", k, e))?;
                        let value = HeaderValue::from_str(v)
                            .map_err(|e| anyhow::anyhow!("invalid header value '{}': {}", v, e))?;
                        Ok((name, value))
                    })
                    .collect();
                config = config.custom_headers(headers?);
            }

            let transport = StreamableHttpClientTransport::from_config(config);

            serve_client((), transport)
                .await
                .map_err(|e| anyhow::anyhow!("failed to connect to MCP server via HTTP: {}", e))?
        }
    };

    let service = Arc::new(service);

    let rmcp_tools: Vec<rmcp::model::Tool> = service
        .list_all_tools()
        .await
        .map_err(|e| anyhow::anyhow!("failed to list MCP tools: {}", e))?;

    // Apply enabled_tools whitelist if configured
    let filtered_rmcp_tools =
        filter_tools_by_whitelist(rmcp_tools, cfg.enabled_tools.as_ref(), &cfg.name);

    let tools: Vec<Box<dyn Tool>> = filtered_rmcp_tools
        .into_iter()
        .map(|t| {
            let wrapper = McpToolWrapper {
                server_name: cfg.name.clone(),
                tool_name: t.name.to_string(),
                description: t.description.unwrap_or_default().to_string(),
                input_schema: serde_json::Value::Object((*t.input_schema).clone()),
                service: service.clone(),
            };
            Box::new(wrapper) as Box<dyn Tool>
        })
        .collect();

    Ok(tools)
}

/// Filter rmcp tools by an optional whitelist of tool names.
///
/// When `whitelist` is `Some`, only tools whose names appear in the list are retained.
/// Returns the filtered vector and optionally logs how many were removed.
fn filter_tools_by_whitelist(
    tools: Vec<rmcp::model::Tool>,
    whitelist: Option<&Vec<String>>,
    server_name: &str,
) -> Vec<rmcp::model::Tool> {
    match whitelist {
        Some(list) => {
            let before = tools.len();
            let filtered: Vec<_> = tools
                .into_iter()
                .filter(|t| list.iter().any(|w| w == t.name.as_ref()))
                .collect();
            let after = filtered.len();
            if after < before {
                tracing::info!(
                    mcp_name = %server_name,
                    before = before,
                    after = after,
                    "Filtered MCP tools by enabled_tools whitelist"
                );
            }
            filtered
        }
        None => tools,
    }
}

/// Wrapper that implements the jyc-agent `Tool` trait for a remote MCP tool.
///
/// When executed, it calls the remote MCP server via the rmcp peer connection.
struct McpToolWrapper {
    /// Name of the MCP server (for logging)
    server_name: String,
    /// Name of the tool on the remote server
    tool_name: String,
    /// Human-readable description
    description: String,
    /// JSON Schema for tool input
    input_schema: Value,
    /// Active rmcp service connection (shared across all tools from this server)
    service: Arc<RunningService<RoleClient, ()>>,
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn source(&self) -> Option<&str> {
        Some(&self.server_name)
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        tracing::debug!(
            server = %self.server_name,
            tool = %self.tool_name,
            "Calling MCP tool"
        );

        let mut params = CallToolRequestParams::new(self.tool_name.clone());
        params.arguments = Some(match input {
            Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("input".to_string(), other);
                map
            }
        });

        // RunningService derefs to Peer<RoleClient> which has call_tool
        match self.service.call_tool(params).await {
            Ok(result) => {
                // Extract text content from the result.
                // Non-text content (images, resources) is logged but not included.
                let mut texts = Vec::new();
                for c in &result.content {
                    if let Some(t) = c.as_text() {
                        texts.push(t.text.clone());
                    } else {
                        tracing::warn!(
                            server = %self.server_name,
                            tool = %self.tool_name,
                            "MCP tool returned non-text content, ignoring"
                        );
                    }
                }
                let content = texts.join("\n");

                Ok(ToolOutput::success(content))
            }
            Err(e) => Ok(ToolOutput::error(format!(
                "MCP tool '{}' error: {}",
                self.tool_name, e
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_tool(name: &str) -> rmcp::model::Tool {
        rmcp::model::Tool::new(
            name.to_string(),
            format!("Description for {}", name),
            serde_json::Map::new(),
        )
    }

    #[test]
    fn filter_tools_by_whitelist_with_none_returns_all() {
        let tools = vec![
            create_test_tool("tool_a"),
            create_test_tool("tool_b"),
            create_test_tool("tool_c"),
        ];
        let result = filter_tools_by_whitelist(tools, None, "test_server");
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn filter_tools_by_whitelist_filters_correctly() {
        let tools = vec![
            create_test_tool("tool_a"),
            create_test_tool("tool_b"),
            create_test_tool("tool_c"),
        ];
        let whitelist = vec!["tool_a".to_string(), "tool_c".to_string()];
        let result = filter_tools_by_whitelist(tools, Some(&whitelist), "test_server");
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|t| t.name == "tool_a"));
        assert!(result.iter().any(|t| t.name == "tool_c"));
        assert!(!result.iter().any(|t| t.name == "tool_b"));
    }

    #[test]
    fn filter_tools_by_whitelist_empty_list_returns_nothing() {
        let tools = vec![create_test_tool("tool_a"), create_test_tool("tool_b")];
        let whitelist = vec![];
        let result = filter_tools_by_whitelist(tools, Some(&whitelist), "test_server");
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn filter_tools_by_whitelist_nonexistent_tools_returns_nothing() {
        let tools = vec![create_test_tool("tool_a"), create_test_tool("tool_b")];
        let whitelist = vec!["nonexistent".to_string()];
        let result = filter_tools_by_whitelist(tools, Some(&whitelist), "test_server");
        assert_eq!(result.len(), 0);
    }
}
