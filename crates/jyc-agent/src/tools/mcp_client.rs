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
use futures::StreamExt;
use futures::stream;
use http::{HeaderName, HeaderValue};
use jyc_types::OAuthClientCredentialsConfig;
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
    // One shared HTTP client for OAuth token fetches across all MCPs;
    // gives us connection pooling without a per-call builder.
    let http = reqwest::Client::new();

    // Load MCPs concurrently with bounded parallelism (`4`). Without
    // this the loop was sequential, so N slow MCPs added up to the
    // sum of their latencies and blocked agent-loop startup
    // proportional to N. The cap keeps a flood of MCPs from
    // overwhelming the local box (file descriptors, sockets, OAuth
    // fetches).
    let load_mcp = |(i, cfg): (usize, &McpServerConfig)| {
        let cfg = cfg.clone();
        let http = &http;
        let timeout_ms = cfg.timeout_ms.unwrap_or(10_000);
        async move {
            // Per-server timeout: a hung MCP (subprocess that never
            // speaks the protocol, unresponsive HTTP endpoint,
            // OAuth endpoint that hangs) must not block agent-loop
            // startup. On timeout we drop the future — for
            // `TokioChildProcess` this signals SIGKILL via Drop on
            // the child handle.
            match tokio::time::timeout(
                std::time::Duration::from_millis(timeout_ms),
                connect_and_list_tools(&cfg, http),
            )
            .await
            {
                Ok(Ok(discovered)) => {
                    tracing::info!(
                        mcp_name = %cfg.name,
                        tool_count = discovered.len(),
                        timeout_ms,
                        "Loaded MCP tools"
                    );
                    Some((i, cfg.name, discovered))
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        mcp_name = %cfg.name,
                        timeout_ms,
                        error = %e,
                        "Failed to load MCP tools, skipping"
                    );
                    None
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        mcp_name = %cfg.name,
                        timeout_ms,
                        "MCP load timed out, skipping"
                    );
                    None
                }
            }
        }
    };

    let mut results: Vec<(usize, String, Vec<Box<dyn Tool>>)> =
        stream::iter(cfgs.iter().enumerate().map(load_mcp))
            .buffer_unordered(4)
            .filter_map(|r| async move { r })
            .collect()
            .await;

    // Sort by config order so the registered tool list is deterministic.
    results.sort_by_key(|(i, _, _)| *i);

    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    for (_i, _name, mut discovered) in results {
        tools.append(&mut discovered);
    }
    tools
}

/// Connect to an MCP server and list its tools.
async fn connect_and_list_tools(
    cfg: &McpServerConfig,
    http: &reqwest::Client,
) -> Result<Vec<Box<dyn Tool>>> {
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
            oauth,
        } => {
            if !enabled {
                anyhow::bail!("remote MCP '{}' is disabled", cfg.name);
            }

            let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());

            // Validation rejects both being set, so only one branch is taken.
            let bearer = match oauth {
                Some(oauth_cfg) => Some(fetch_oauth_token(&cfg.name, oauth_cfg, http).await?),
                None => auth_header.clone(),
            };
            if let Some(token) = bearer {
                config = config.auth_header(token);
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

/// Fetch an OAuth2 access token using the client_credentials grant.
///
/// POSTs `application/x-www-form-urlencoded` to `cfg.token_endpoint` with
/// `grant_type=client_credentials&client_id=…&client_secret=…&scope=…` and
/// parses the JSON response for `access_token`.
///
/// `expires_in` is parsed but ignored — the token is fetched once at MCP
/// connect time and reused for the life of the connection. To pick up a
/// rotated token, restart jyc. ponytail: token refresh skipped, restart on
/// expiry; add when token rotation matters.
async fn fetch_oauth_token(
    mcp_name: &str,
    cfg: &OAuthClientCredentialsConfig,
    http: &reqwest::Client,
) -> Result<String> {
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".to_string(), "client_credentials".to_string()),
        ("client_id".to_string(), cfg.client_id.clone()),
        ("client_secret".to_string(), cfg.client_secret.clone()),
    ];
    if !cfg.scopes.is_empty() {
        form.push(("scope".to_string(), cfg.scopes.join(" ")));
    }

    let response = http
        .post(&cfg.token_endpoint)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .with_context(|| {
            format!(
                "OAuth token request to '{}' failed for MCP '{}'",
                cfg.token_endpoint, mcp_name
            )
        })?;

    let status = response.status();
    let body = response.text().await.with_context(|| {
        format!(
            "failed to read OAuth token response body for MCP '{}'",
            mcp_name
        )
    })?;

    if !status.is_success() {
        anyhow::bail!(
            "OAuth token endpoint '{}' returned HTTP {} for MCP '{}': {}",
            cfg.token_endpoint,
            status.as_u16(),
            mcp_name,
            body
        );
    }

    let parsed: serde_json::Value = serde_json::from_str(&body).with_context(|| {
        format!(
            "OAuth token response is not valid JSON for MCP '{}'",
            mcp_name
        )
    })?;

    parsed
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "OAuth token response missing 'access_token' field for MCP '{}'",
                mcp_name
            )
        })
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

    #[tokio::test]
    async fn fetch_oauth_token_sends_client_credentials_and_parses_access_token() {
        use wiremock::matchers::{body_string, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string(
                "grant_type=client_credentials&client_id=id&client_secret=secret",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"access_token":"abc123","token_type":"Bearer","expires_in":3600}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let cfg = OAuthClientCredentialsConfig {
            client_id: "id".to_string(),
            client_secret: "secret".to_string(),
            token_endpoint: format!("{}/oauth/token", server.uri()),
            scopes: vec![],
        };
        let http = reqwest::Client::new();
        let token = fetch_oauth_token("test-mcp", &cfg, &http)
            .await
            .expect("token fetch");
        assert_eq!(token, "abc123");
    }

    #[tokio::test]
    async fn fetch_oauth_token_includes_scopes_in_form_body() {
        use wiremock::matchers::{body_string, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string(
                "grant_type=client_credentials&client_id=id&client_secret=secret&scope=read+write",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"access_token":"xyz"}"#))
            .expect(1)
            .mount(&server)
            .await;

        let cfg = OAuthClientCredentialsConfig {
            client_id: "id".to_string(),
            client_secret: "secret".to_string(),
            token_endpoint: format!("{}/oauth/token", server.uri()),
            scopes: vec!["read".to_string(), "write".to_string()],
        };
        let http = reqwest::Client::new();
        let token = fetch_oauth_token("test-mcp", &cfg, &http).await.unwrap();
        assert_eq!(token, "xyz");
    }

    #[tokio::test]
    async fn fetch_oauth_token_returns_error_on_http_failure() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string(r#"{"error":"invalid_client"}"#),
            )
            .mount(&server)
            .await;

        let cfg = OAuthClientCredentialsConfig {
            client_id: "bad".to_string(),
            client_secret: "bad".to_string(),
            token_endpoint: format!("{}/oauth/token", server.uri()),
            scopes: vec![],
        };
        let http = reqwest::Client::new();
        let err = fetch_oauth_token("test-mcp", &cfg, &http)
            .await
            .expect_err("must fail on 401");
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn fetch_oauth_token_returns_error_on_non_json_body() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>oops</html>"))
            .mount(&server)
            .await;

        let cfg = OAuthClientCredentialsConfig {
            client_id: "id".to_string(),
            client_secret: "secret".to_string(),
            token_endpoint: format!("{}/oauth/token", server.uri()),
            scopes: vec![],
        };
        let http = reqwest::Client::new();
        let err = fetch_oauth_token("test-mcp", &cfg, &http)
            .await
            .expect_err("must fail on non-JSON body");
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[tokio::test]
    async fn fetch_oauth_token_returns_error_when_access_token_missing() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token_type":"Bearer"}"#))
            .mount(&server)
            .await;

        let cfg = OAuthClientCredentialsConfig {
            client_id: "id".to_string(),
            client_secret: "secret".to_string(),
            token_endpoint: format!("{}/oauth/token", server.uri()),
            scopes: vec![],
        };
        let http = reqwest::Client::new();
        let err = fetch_oauth_token("test-mcp", &cfg, &http)
            .await
            .expect_err("must fail when access_token field missing");
        assert!(err.to_string().contains("access_token"));
    }

    #[tokio::test]
    async fn fetch_oauth_token_returns_error_on_connection_refused() {
        let cfg = OAuthClientCredentialsConfig {
            client_id: "id".to_string(),
            client_secret: "secret".to_string(),
            // Reserved port that should always refuse connections immediately.
            token_endpoint: "http://127.0.0.1:1/oauth/token".to_string(),
            scopes: vec![],
        };
        let http = reqwest::Client::new();
        let err = fetch_oauth_token("test-mcp", &cfg, &http)
            .await
            .expect_err("must fail when endpoint is unreachable");
        assert!(err.to_string().contains("OAuth token request"));
    }

    /// Round-trip test against jyc-mcp's own reply server over an in-process
    /// duplex pipe. Validates the rmcp client API (`serve_client`,
    /// `list_all_tools`, `call_tool`) against the current rmcp major — fails
    /// if an rmcp upgrade breaks the client handshake or tool-call path.
    #[tokio::test]
    async fn rmcp_client_round_trip_with_inprocess_server() {
        use rmcp::ServiceExt;

        let (a, b) = tokio::io::duplex(1 << 16);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);

        let server_handle = tokio::spawn(async move {
            let service = jyc_mcp::reply_tool::ReplyToolHandler
                .serve((ar, aw))
                .await
                .expect("server init failed");
            service.waiting().await.ok();
        });

        let service = serve_client((), (br, bw))
            .await
            .expect("client connect failed");

        let tools = service.list_all_tools().await.expect("list tools failed");
        assert!(
            tools.iter().any(|t| t.name.as_ref() == "reply_message"),
            "reply_message must be listed, got: {tools:?}"
        );

        // Empty message -> clean error response; no reply files are written.
        let mut params = CallToolRequestParams::new("reply_message");
        params.arguments = Some(
            serde_json::json!({ "message": "" })
                .as_object()
                .expect("static object")
                .clone(),
        );
        let result = service.call_tool(params).await.expect("call tool failed");
        assert!(
            result.is_error == Some(true) || !result.content.is_empty(),
            "expected an error/content response, got: {result:?}"
        );

        server_handle.abort();
    }
}
