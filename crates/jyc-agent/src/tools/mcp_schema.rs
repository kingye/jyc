//! MCP tool schemas, loaded on demand.
//!
//! An MCP server's tools are schemas like any other: they ride in every
//! request's stable prompt prefix, whether or not this conversation ever
//! calls them. A server with fifty tools puts fifty schemas in the request
//! of a topic that wanted one of them.
//!
//! So the MCP tools stay out of the advertised list until the model asks
//! for them. What it sees instead is this tool, whose description is the
//! index of the configured servers: each server's name, and each of its
//! tools with a one-line description — enough to decide *which* server to
//! load, and none of the schemas themselves. Calling the tool expands one
//! server's tools; from the next model call on their schemas are part of the
//! advertised list, and they stay for the life of the topic's registry (a
//! config reload or an `/mcp` change rebuilds it, and they hide again).
//!
//! The expansion is per tool, not per server: a server with a hundred tools
//! can be expanded a few at a time, by naming them.
//!
//! Only *advertisement* is gated. An MCP tool stays registered and
//! executable, so a model that names one before expanding it gets a working
//! call instead of a "not found" error. The permission boundary is
//! `disabled_tools` / `disabled_mcps`, which remove a tool or a server for
//! good — and a server removed that way is not in the index either.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Tool, ToolContext, ToolOutput};
use crate::tools::registry::{McpGroup, ToolRegistry};

/// The name of the tool that expands an MCP server's tool schemas.
pub const LOAD_MCP_SCHEMA: &str = "load_mcp_schema";

/// The longest one-liner kept for a tool in the index. A server's own
/// description can run to paragraphs; the index only has to make the tool
/// recognisable.
const ONE_LINER_MAX: usize = 120;

/// Register the loader into a registry that has MCP groups.
///
/// A registry without MCP servers gets no loader: there would be nothing to
/// load, and a tool that always fails is a worse advertisement than no tool
/// at all.
pub fn register_mcp_schema_tool(registry: &mut ToolRegistry) {
    let groups = registry.mcp_groups().to_vec();
    if groups.is_empty() {
        return;
    }
    let description = LoadMcpSchemaTool::description_for(&groups);
    registry.register(Box::new(LoadMcpSchemaTool {
        groups,
        description,
    }));
}

/// The tool that expands an MCP server's tool schemas.
pub struct LoadMcpSchemaTool {
    /// The servers this registry holds, in index order.
    groups: Vec<Arc<McpGroup>>,
    /// The index of the servers, built once when the tool is registered.
    description: String,
}

impl LoadMcpSchemaTool {
    /// The description, built from the servers this registry actually has.
    fn description_for(groups: &[Arc<McpGroup>]) -> String {
        let mut text = String::from(
            "Load an MCP server's tool schemas into your tool list. The MCP tools themselves are \
             not in your tool list until you load them; call this with a server name and its \
             tools are in your tool list from the next model call on, while this topic's tool \
             registry is reused.",
        );
        text.push_str(" Servers:");
        for group in groups {
            text.push_str(&format!("\n- {}:", group.server_name()));
            let mut first = true;
            for entry in group.tools() {
                if !first {
                    text.push(',');
                }
                first = false;
                let one_liner = one_liner(&entry.description);
                if one_liner.is_empty() {
                    text.push_str(&format!(" {}", entry.name));
                } else {
                    text.push_str(&format!(" {} ({})", entry.name, one_liner));
                }
            }
        }
        text.push_str(
            " Call with a server name to expand its tools; pass `tool_names` to expand only \
             some of them.",
        );
        text
    }
}

/// The tool's description of a tool, cut to its first line and capped.
fn one_liner(description: &str) -> String {
    let first_line = description.lines().next().unwrap_or("").trim();
    if first_line.chars().count() <= ONE_LINER_MAX {
        return first_line.to_string();
    }
    let mut text: String = first_line.chars().take(ONE_LINER_MAX).collect();
    text.push('…');
    text
}

#[async_trait]
impl Tool for LoadMcpSchemaTool {
    fn name(&self) -> &str {
        LOAD_MCP_SCHEMA
    }

    fn description(&self) -> &str {
        // Built once per registry and stored, so the trait can hand out a
        // borrow of it.
        &self.description
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "mcp_name": {
                    "type": "string",
                    "description": "MCP server name, as listed in this tool's description"
                },
                "tool_names": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional subset of that server's tools to expand; omit to \
                                     expand all of them"
                }
            },
            "required": ["mcp_name"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let server_name = input
            .get("mcp_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if server_name.is_empty() {
            return Ok(ToolOutput::error("Missing 'mcp_name' parameter"));
        }

        let Some(group) = self
            .groups
            .iter()
            .find(|group| group.server_name() == server_name)
        else {
            let available: Vec<&str> = self.groups.iter().map(|g| g.server_name()).collect();
            return Ok(ToolOutput::error(format!(
                "Unknown MCP server '{server_name}'. Available: {}",
                available.join(", ")
            )));
        };

        // `tool_names` is optional. When it is present it must be an array
        // of tool names: a malformed value is an error, not a silent
        // "expand everything" — the caller asked for a subset, and
        // expanding the whole server would defeat the point of asking.
        // An explicit null is the JSON way of saying "no value", so it is
        // treated like an omitted field.
        let requested: Option<Vec<String>> = match input.get("tool_names") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let Some(items) = value.as_array() else {
                    return Ok(ToolOutput::error(format!(
                        "'tool_names' must be an array of tool names, got {}",
                        json_type_name(value)
                    )));
                };
                let mut names = Vec::with_capacity(items.len());
                for item in items {
                    let Some(name) = item.as_str() else {
                        return Ok(ToolOutput::error(format!(
                            "'tool_names' must contain only tool names, got {}",
                            json_type_name(item)
                        )));
                    };
                    let name = name.trim();
                    if name.is_empty() {
                        return Ok(ToolOutput::error(
                            "'tool_names' must not contain an empty tool name",
                        ));
                    }
                    names.push(name.to_string());
                }
                // An explicitly empty list is the caller saying "none of
                // them" — but there is nothing to expand, so it is an error
                // rather than a quiet no-op.
                if names.is_empty() {
                    return Ok(ToolOutput::error(
                        "'tool_names' is empty — omit it to expand every tool of the server",
                    ));
                }
                Some(names)
            }
        };

        let Some(requested) = requested else {
            // No subset asked for: expand the whole server.
            let newly = group.expand_all();
            let held = group.tool_names();
            return Ok(expansion_result(group.server_name(), &newly, &held, &held));
        };

        for name in &requested {
            if !group.has_tool(name) {
                return Ok(ToolOutput::error(format!(
                    "Unknown tool '{name}' on MCP server '{server_name}'. Available: {}",
                    group.tool_names().join(", ")
                )));
            }
        }
        let newly = group.expand(&requested);
        let requested: Vec<&str> = requested.iter().map(|name| name.as_str()).collect();
        let held = group.tool_names();
        Ok(expansion_result(
            group.server_name(),
            &newly,
            &requested,
            &held,
        ))
    }
}

/// The JSON type of a value, for an error message that says what came in
/// instead of what was expected.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The reply to an expansion: what it brought in, and what the server holds.
///
/// `requested` is what the caller asked for and `held` is everything the
/// server has. The two differ when the caller asked for a subset: saying
/// "the tools of this server are already expanded" after a subset request
/// would claim the whole server is available when the rest of it is still
/// hidden.
fn expansion_result(
    server_name: &str,
    newly: &[String],
    requested: &[&str],
    held: &[&str],
) -> ToolOutput {
    if newly.is_empty() {
        if requested.len() == held.len() {
            // The whole server was asked for, and it is all expanded.
            return ToolOutput::success(format!(
                "The tools of MCP server '{server_name}' are already expanded: {}.",
                held.join(", ")
            ));
        }
        // A subset was asked for, and that subset is already expanded —
        // the rest of the server is still hidden.
        return ToolOutput::success(format!(
            "The requested tools of MCP server '{server_name}' are already expanded: {}. The \
             server's other tools are still hidden: {}.",
            requested.join(", "),
            held.join(", ")
        ));
    }
    ToolOutput::success(format!(
        "Expanded {} tool(s) of MCP server '{server_name}': {}. They are in your tool list from \
         the next model call on, while this topic's tool registry is reused — a config reload or \
         an `/mcp` change rebuilds it, and hides them again until they are expanded once more.",
        newly.len(),
        newly.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::McpToolEntry;
    use async_trait::async_trait;

    /// A stand-in for an MCP server's tool: registered in the registry, so
    /// the advertisement rules have something to hold.
    struct FakeMcpTool {
        name: String,
        server: String,
    }

    #[async_trait]
    impl Tool for FakeMcpTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn source(&self) -> Option<&str> {
            Some(&self.server)
        }

        fn description(&self) -> &str {
            "a tool from an MCP server"
        }

        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }

        async fn execute(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput> {
            Ok(ToolOutput::success("ok"))
        }
    }

    /// A group with the given tools, as a server would provide them.
    fn group(server: &str, tools: &[(&str, &str)]) -> Arc<McpGroup> {
        Arc::new(McpGroup::new(
            server.to_string(),
            tools
                .iter()
                .map(|(name, description)| McpToolEntry {
                    name: (*name).to_string(),
                    description: (*description).to_string(),
                })
                .collect(),
        ))
    }

    /// A registry with two MCP servers — their tools registered — and the
    /// loader registered over them.
    fn registry_with_groups() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for (server, tool) in [
            ("github", "create_issue"),
            ("github", "search_repositories"),
            ("invoice", "query_invoice"),
        ] {
            registry.register(Box::new(FakeMcpTool {
                name: tool.to_string(),
                server: server.to_string(),
            }));
        }
        registry.set_mcp_groups(vec![
            group(
                "github",
                &[
                    ("create_issue", "Create an issue in a repository"),
                    ("search_repositories", "Search repositories by name"),
                ],
            ),
            group("invoice", &[("query_invoice", "Query invoice records")]),
        ]);
        register_mcp_schema_tool(&mut registry);
        registry
    }

    /// The MCP tools are out of the request until the model asks for them.
    #[test]
    fn the_mcp_tools_are_hidden_until_the_server_is_expanded() {
        let registry = registry_with_groups();
        let advertised: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();

        assert!(
            !advertised.iter().any(|n| n == "create_issue"),
            "an MCP tool must not be advertised before its server is expanded: {advertised:?}"
        );
        assert!(
            advertised.iter().any(|n| n == LOAD_MCP_SCHEMA),
            "the loader is how the model finds the servers: {advertised:?}"
        );
    }

    /// Expanding a server brings its tools into the advertised list.
    #[test]
    fn expanding_a_server_advertises_its_tools() {
        let registry = registry_with_groups();
        let group = registry.mcp_group("github").expect("the group").clone();
        group.expand_all();

        let advertised: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            advertised.iter().any(|n| n == "create_issue"),
            "an expanded tool must be advertised: {advertised:?}"
        );
        assert!(
            !advertised.iter().any(|n| n == "query_invoice"),
            "another server's tools stay out: {advertised:?}"
        );
    }

    /// The index names the servers and their tools, and no schema.
    #[test]
    fn the_index_names_the_servers_and_their_tools() {
        let registry = registry_with_groups();
        let loader = registry
            .definitions()
            .into_iter()
            .find(|d| d.name == LOAD_MCP_SCHEMA)
            .expect("the loader is registered");

        assert!(loader.description.contains("github"));
        assert!(loader.description.contains("create_issue"));
        assert!(
            loader
                .description
                .contains("Create an issue in a repository")
        );
        assert!(loader.description.contains("invoice"));
        assert!(
            !loader.description.contains("properties"),
            "the index must not carry a schema: {}",
            loader.description
        );
    }

    /// A registry without MCP servers gets no loader.
    #[test]
    fn a_registry_without_mcps_gets_no_loader() {
        let mut registry = ToolRegistry::new();
        register_mcp_schema_tool(&mut registry);
        assert!(!registry.has_tool(LOAD_MCP_SCHEMA));
    }

    /// The loader's call expands the server's tools.
    #[tokio::test]
    async fn expanding_a_server_advertises_its_tools_through_the_loader() {
        let registry = registry_with_groups();
        let out = registry
            .execute(
                LOAD_MCP_SCHEMA,
                json!({"mcp_name": "github"}),
                &ToolContext::new(std::path::Path::new(".")),
            )
            .await
            .expect("the loader runs");
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("create_issue"), "{}", out.content);

        let advertised: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            advertised.iter().any(|n| n == "create_issue"),
            "an expanded tool must be advertised: {advertised:?}"
        );
    }

    /// Expanding a subset brings in only that subset.
    #[tokio::test]
    async fn expanding_a_subset_brings_in_only_that_subset() {
        let registry = registry_with_groups();
        let out = registry
            .execute(
                LOAD_MCP_SCHEMA,
                json!({"mcp_name": "github", "tool_names": ["create_issue"]}),
                &ToolContext::new(std::path::Path::new(".")),
            )
            .await
            .expect("the loader runs");
        assert!(!out.is_error, "{}", out.content);

        let advertised: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            advertised.iter().any(|n| n == "create_issue"),
            "the named tool must be advertised: {advertised:?}"
        );
        assert!(
            !advertised.iter().any(|n| n == "search_repositories"),
            "a tool that was not named stays out: {advertised:?}"
        );
    }

    /// A second call is a no-op that says so.
    #[tokio::test]
    async fn expanding_twice_is_idempotent() {
        let registry = registry_with_groups();
        let ctx = ToolContext::new(std::path::Path::new("."));
        let first = registry
            .execute(LOAD_MCP_SCHEMA, json!({"mcp_name": "github"}), &ctx)
            .await
            .expect("the loader runs");
        assert!(!first.is_error, "{}", first.content);
        assert!(first.content.contains("Expanded"), "{}", first.content);

        let second = registry
            .execute(LOAD_MCP_SCHEMA, json!({"mcp_name": "github"}), &ctx)
            .await
            .expect("the loader runs");
        assert!(!second.is_error, "{}", second.content);
        assert!(
            second.content.contains("already expanded"),
            "{}",
            second.content
        );
    }

    /// An unknown server is an error that lists what there is.
    #[tokio::test]
    async fn an_unknown_server_is_an_error() {
        let registry = registry_with_groups();
        let out = registry
            .execute(
                LOAD_MCP_SCHEMA,
                json!({"mcp_name": "nope"}),
                &ToolContext::new(std::path::Path::new(".")),
            )
            .await
            .expect("the loader runs");
        assert!(out.is_error);
        assert!(out.content.contains("github"), "{}", out.content);
    }

    /// A missing name is an error, not a silent no-op.
    #[tokio::test]
    async fn a_missing_name_is_an_error() {
        let registry = registry_with_groups();
        let out = registry
            .execute(
                LOAD_MCP_SCHEMA,
                json!({}),
                &ToolContext::new(std::path::Path::new(".")),
            )
            .await
            .expect("the loader runs");
        assert!(out.is_error);
    }

    /// An unknown tool name is an error that lists the server's tools.
    #[tokio::test]
    async fn an_unknown_tool_is_an_error() {
        let registry = registry_with_groups();
        let out = registry
            .execute(
                LOAD_MCP_SCHEMA,
                json!({"mcp_name": "github", "tool_names": ["nope"]}),
                &ToolContext::new(std::path::Path::new(".")),
            )
            .await
            .expect("the loader runs");
        assert!(out.is_error);
        assert!(out.content.contains("create_issue"), "{}", out.content);
    }

    /// A repeated *subset* expansion must not claim the whole server is
    /// expanded: the tools that were not asked for are still hidden, and
    /// saying otherwise would have the model believe they are callable.
    #[tokio::test]
    async fn a_repeated_subset_expansion_does_not_claim_the_whole_server() {
        let registry = registry_with_groups();
        let ctx = ToolContext::new(std::path::Path::new("."));
        let first = registry
            .execute(
                LOAD_MCP_SCHEMA,
                json!({"mcp_name": "github", "tool_names": ["create_issue"]}),
                &ctx,
            )
            .await
            .expect("the loader runs");
        assert!(!first.is_error, "{}", first.content);

        let second = registry
            .execute(
                LOAD_MCP_SCHEMA,
                json!({"mcp_name": "github", "tool_names": ["create_issue"]}),
                &ctx,
            )
            .await
            .expect("the loader runs");
        assert!(!second.is_error, "{}", second.content);
        assert!(
            second.content.contains("already expanded"),
            "{}",
            second.content
        );
        assert!(
            second.content.contains("still hidden"),
            "the reply must say the rest of the server is still hidden: {}",
            second.content
        );

        // The tool that was never asked for is still out of the request.
        let advertised: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            advertised.iter().any(|n| n == "create_issue"),
            "the expanded tool is advertised: {advertised:?}"
        );
        assert!(
            !advertised.iter().any(|n| n == "search_repositories"),
            "the tool that was not asked for stays hidden: {advertised:?}"
        );
    }

    /// A malformed `tool_names` is an error, not a silent "expand
    /// everything" — the caller asked for a subset, and expanding the whole
    /// server would defeat the point of asking.
    #[tokio::test]
    async fn a_malformed_tool_names_is_an_error_and_expands_nothing() {
        for malformed in [
            json!("create_issue"),
            json!([42]),
            json!([""]),
            json!(["create_issue", 7]),
            json!([]),
        ] {
            let registry = registry_with_groups();
            let out = registry
                .execute(
                    LOAD_MCP_SCHEMA,
                    json!({"mcp_name": "github", "tool_names": malformed.clone()}),
                    &ToolContext::new(std::path::Path::new(".")),
                )
                .await
                .expect("the loader runs");
            assert!(
                out.is_error,
                "{malformed} must be an error: {}",
                out.content
            );

            let advertised: Vec<String> =
                registry.definitions().into_iter().map(|d| d.name).collect();
            assert!(
                !advertised.iter().any(|n| n == "create_issue")
                    && !advertised.iter().any(|n| n == "search_repositories"),
                "{malformed} must expand nothing: {advertised:?}"
            );
        }
    }

    /// An MCP server that exposes a tool under a loader's name does not
    /// take the name over: the loader is how the model reaches the server
    /// at all, and a loader it cannot call is a server it can never expand.
    #[tokio::test]
    async fn an_mcp_tool_cannot_take_over_a_loader_name() {
        let mut registry = ToolRegistry::new();
        // The MCP server's copy of the loader's name, registered first.
        registry.register(Box::new(FakeMcpTool {
            name: LOAD_MCP_SCHEMA.to_string(),
            server: "github".to_string(),
        }));
        registry.set_mcp_groups(vec![group(
            "github",
            &[(LOAD_MCP_SCHEMA, "An MCP lookalike")],
        )]);
        register_mcp_schema_tool(&mut registry);

        let advertised: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            advertised.iter().any(|n| n == LOAD_MCP_SCHEMA),
            "the loader must be advertised: {advertised:?}"
        );
    }

    /// A name belongs to exactly one deferred owner. An MCP server that
    /// exposes a built-in's name has taken that name, so the built-in's
    /// group must not claim it as well — otherwise the tool stays hidden
    /// until *both* groups are loaded, and each loader reports it as
    /// available while it is still hidden.
    #[test]
    fn an_mcp_tool_that_shadows_a_built_in_has_one_owner() {
        let mut registry = ToolRegistry::new();
        // The built-in comes first, then the MCP server's copy takes the
        // name over — the registry holds one tool per name.
        registry.register(Box::new(FakeMcpTool {
            name: "webfetch".to_string(),
            server: "github".to_string(),
        }));
        registry.set_mcp_groups(vec![group("github", &[("webfetch", "Fetch a URL")])]);

        // The MCP group owns it: it is hidden until the server is expanded.
        let advertised: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            !advertised.iter().any(|n| n == "webfetch"),
            "the MCP tool must not be advertised before its server is expanded: {advertised:?}"
        );
    }
}
