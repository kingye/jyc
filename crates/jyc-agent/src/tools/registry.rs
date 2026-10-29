//! Tool registry — collects all available tools and provides definitions to the LLM.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use serde_json::Value;

use jyc_types::config::HookEvent;
use jyc_utils::hooks::{HookCtx, HookOutcome, HookSet};

use super::{Tool, ToolContext, ToolOutput};
use crate::types::ToolDefinition;

/// The group of tools that stay out of the advertised tool list until the
/// model asks for them (`load_extended_tools`).
///
/// The state is shared between the registry — which decides what the model
/// sees — and the tool that flips it. One instance is created per registry
/// build, and the registry itself is cached per topic, so a load sticks for
/// the rest of that topic's registry lifetime (a config reload rebuilds the
/// registry and starts over).
///
/// Only *advertisement* is gated: an extended tool stays registered and
/// executable, so a model that names one before loading it gets a working
/// call rather than a "not found" error.
#[derive(Debug)]
pub struct ExtendedTools {
    /// The extended tools present in this registry, by name.
    names: Vec<String>,
    /// Whether the model has loaded the group.
    loaded: AtomicBool,
}

impl ExtendedTools {
    /// Create the group for the given tool names.
    pub fn new(names: Vec<String>) -> Self {
        Self {
            names,
            loaded: AtomicBool::new(false),
        }
    }

    /// The extended tools in this group, by name.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Whether the group has been loaded.
    pub fn is_loaded(&self) -> bool {
        self.loaded.load(Ordering::Relaxed)
    }

    /// Load the group. Returns `true` when this call is the one that loaded
    /// it (a second call reports `false`).
    pub fn load(&self) -> bool {
        !self.loaded.swap(true, Ordering::Relaxed)
    }
}

/// One MCP server's tools, and which of them the model has expanded.
///
/// A tool schema is a standing per-call cost, and an MCP server's tools are
/// no exception: a server with fifty tools puts fifty schemas in every
/// request, whether or not this conversation ever touches them. The group's
/// tools stay out of the advertised list until the model expands them
/// through `load_mcp_schema`.
///
/// The state is shared between the registry — which decides what the model
/// sees — and the tool that flips it. One group is created per MCP server per
/// registry build, and the registry itself is cached per topic, so an
/// expansion sticks for the rest of that topic's registry lifetime (a config
/// reload rebuilds the registry and the server's tools hide again).
///
/// Only *advertisement* is gated: an MCP tool stays registered and
/// executable, so a model that names one before expanding it gets a working
/// call rather than a "not found" error.
#[derive(Debug)]
pub struct McpGroup {
    /// The MCP server's name, as configured.
    server_name: String,
    /// The server's tools, sorted by name.
    tools: Vec<McpToolEntry>,
    /// The tools the model has expanded.
    expanded: RwLock<HashSet<String>>,
}

/// One MCP tool, as it appears in the group's index.
#[derive(Debug, Clone)]
pub struct McpToolEntry {
    /// The tool's name — the name it is called by.
    pub name: String,
    /// The server's own description of the tool, in one line.
    pub description: String,
}

impl McpGroup {
    /// Create a group for one MCP server. The tools are sorted by name, so
    /// the index the model reads is stable across builds.
    pub fn new(server_name: String, mut tools: Vec<McpToolEntry>) -> Self {
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            server_name,
            tools,
            expanded: RwLock::new(HashSet::new()),
        }
    }

    /// The MCP server's name.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The server's tools, sorted by name.
    pub fn tools(&self) -> &[McpToolEntry] {
        &self.tools
    }

    /// The server's tool names, sorted.
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name.as_str()).collect()
    }

    /// Whether the server exposes a tool by that name.
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name == name)
    }

    /// Whether the tool has been expanded into the advertised list.
    pub fn is_expanded(&self, tool_name: &str) -> bool {
        self.expanded
            .read()
            .expect("MCP group lock poisoned")
            .contains(tool_name)
    }

    /// Expand the named tools. Returns the ones this call actually expanded
    /// — a tool that was already expanded is not reported again.
    pub fn expand(&self, names: &[String]) -> Vec<String> {
        let mut expanded = self.expanded.write().expect("MCP group lock poisoned");
        let mut newly = Vec::new();
        for name in names {
            if expanded.insert(name.clone()) {
                newly.push(name.clone());
            }
        }
        newly
    }

    /// Expand every tool the server exposes. Returns the ones this call
    /// actually expanded.
    pub fn expand_all(&self) -> Vec<String> {
        let names: Vec<String> = self.tools.iter().map(|t| t.name.clone()).collect();
        self.expand(&names)
    }
}

/// Registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
    /// Compiled global + per-agent hook set for tool events
    /// (`pre_tool_use`, `post_tool_use`, `post_tool_use_failure`);
    /// `None` (the default) → zero hook overhead.
    hooks: Option<Arc<HookSet>>,
    /// The extended-tools group, when this registry has one.
    extended: Option<Arc<ExtendedTools>>,
    /// The MCP groups, when this registry has any, sorted by server name.
    mcp_groups: Vec<Arc<McpGroup>>,
    /// Which group owns each MCP tool, by tool name. A tool name two servers
    /// both expose belongs to the one that won the registration.
    mcp_tool_owner: HashMap<String, usize>,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            hooks: None,
            extended: None,
            mcp_groups: Vec::new(),
            mcp_tool_owner: HashMap::new(),
        }
    }

    /// Attach agent hooks. Called by `build_tool_registry` when the agent
    /// (or global config) declares hooks; the registry cache key includes
    /// the config snapshot pointer, so a reload rebuilds with fresh hooks.
    pub fn set_hooks(&mut self, hooks: Arc<HookSet>) {
        self.hooks = Some(hooks);
    }

    /// Attach the extended-tools group. The tools it names stay out of the
    /// advertised list until the group is loaded.
    pub fn set_extended(&mut self, extended: Arc<ExtendedTools>) {
        self.extended = Some(extended);
    }

    /// The extended-tools group, when this registry has one.
    pub fn extended(&self) -> Option<&Arc<ExtendedTools>> {
        self.extended.as_ref()
    }

    /// Attach the MCP groups. The tools they name stay out of the advertised
    /// list until the model expands them through `load_mcp_schema`.
    pub fn set_mcp_groups(&mut self, mut groups: Vec<Arc<McpGroup>>) {
        groups.sort_by(|a, b| a.server_name().cmp(b.server_name()));
        let mut owner = HashMap::new();
        for (index, group) in groups.iter().enumerate() {
            for entry in group.tools() {
                owner.insert(entry.name.clone(), index);
            }
        }
        self.mcp_groups = groups;
        self.mcp_tool_owner = owner;
    }

    /// The MCP groups, when this registry has any.
    pub fn mcp_groups(&self) -> &[Arc<McpGroup>] {
        &self.mcp_groups
    }

    /// The group of one MCP server, by name.
    pub fn mcp_group(&self, server_name: &str) -> Option<&Arc<McpGroup>> {
        self.mcp_groups
            .iter()
            .find(|group| group.server_name() == server_name)
    }

    /// Whether a tool belongs to an MCP group that has not been expanded
    /// for it yet. A tool that is not an MCP tool is always advertised.
    fn mcp_tool_is_advertised(&self, name: &str) -> bool {
        match self.mcp_tool_owner.get(name) {
            None => true,
            Some(index) => self.mcp_groups[*index].is_expanded(name),
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

    /// Get the tool definitions for the LLM, sorted by name.
    ///
    /// The sort is required for prompt caching: providers cache on an exact
    /// prefix match, and `HashMap` iteration order is randomized per process.
    /// Without a stable order the serialized `tools` array differs on every
    /// restart, so the cache breakpoint placed on the last tool would never
    /// produce a hit (see `provider::anthropic::apply_cache_breakpoints`).
    ///
    /// The extended tools are filtered out until the model loads them —
    /// their schemas are a standing per-call cost, and most turns never need
    /// them. The MCP tools are filtered out until the model expands their
    /// server, for the same reason. Use [`Self::all_definitions`] for the
    /// unfiltered set.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let hidden = self.extended.as_ref().filter(|group| !group.is_loaded());
        let mut defs: Vec<ToolDefinition> = self
            .tools
            .values()
            .filter(|tool| {
                let name = tool.name();
                if let Some(group) = hidden {
                    if group.names().iter().any(|hidden_name| hidden_name == name) {
                        return false;
                    }
                }
                self.mcp_tool_is_advertised(name)
            })
            .map(|t| t.to_definition())
            .collect();
        // Unstable sort: tool names are unique (they're `HashMap` keys), so
        // there are no equal elements whose relative order could matter, and
        // it avoids the scratch allocation a stable sort needs.
        defs.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Every registered tool's definition, sorted by name — the extended
    /// ones included, whether or not they are advertised right now.
    pub fn all_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs: Vec<ToolDefinition> =
            self.tools.values().map(|t| t.to_definition()).collect();
        defs.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Execute a tool by name, applying the agent's external hooks.
    ///
    /// Exit-code semantics mirror Claude Code: a `pre_tool_use` hook that
    /// exits 2 fails the call with the hook's stderr as the error (visible
    /// to the model); a `post_tool_use` hook that exits 2 marks the
    /// (already executed) result as error with the stderr appended.
    /// `post_tool_use_failure` is notification-only and fires whenever the
    /// TOOL itself failed. Synchronous reply deliveries fire `reply_send`
    /// at their send site in `deliver_reply` instead. Without hooks
    /// configured this is the same zero-cost pass-through it always was.
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

        let Some(hooks) = self.hooks.clone() else {
            return tool.execute(input, ctx).await;
        };

        // Clone payload data only when hooks are configured.
        let input_for_hook = input.clone();
        let hook_ctx = |response: Option<String>| HookCtx {
            topic: ctx.current_topic.clone().unwrap_or_default(),
            cwd: ctx.working_dir.display().to_string(),
            channel: ctx.current_channel.clone(),
            tool_name: Some(name.to_string()),
            tool_input: Some(input_for_hook.clone()),
            tool_response: response,
            ..Default::default()
        };

        // ── pre_tool_use ───────────────────────────────────────────────
        if let HookOutcome::Block(reason) = hooks
            .run(HookEvent::PreToolUse, Some(name), &hook_ctx(None))
            .await
        {
            return Err(anyhow::anyhow!("blocked by pre_tool_use hook: {reason}"));
        }

        let mut output = match tool.execute(input, ctx).await {
            Ok(output) => output,
            Err(e) => {
                // Failure notification (does not alter the outcome).
                hooks
                    .run(
                        HookEvent::PostToolUseFailure,
                        Some(name),
                        &hook_ctx(Some(e.to_string())),
                    )
                    .await;
                return Err(e);
            }
        };

        // ── post_tool_use ──────────────────────────────────────────────
        // "Failure" means the TOOL failed — captured before the post hook
        // so a post-hook's own block is not misreported as a tool failure.
        let tool_failed = output.is_error;
        if let HookOutcome::Block(reason) = hooks
            .run(
                HookEvent::PostToolUse,
                Some(name),
                &hook_ctx(Some(output.content.clone())),
            )
            .await
        {
            // The tool already ran; surface the hook's reason to the model
            // as an error result.
            output.is_error = true;
            output.content.push_str("\n\n[post_tool_use hook: ");
            output.content.push_str(reason.trim());
            output.content.push(']');
        }
        // ── post_tool_use_failure (notification-only) ──────────────────
        if tool_failed {
            hooks
                .run(
                    HookEvent::PostToolUseFailure,
                    Some(name),
                    &hook_ctx(Some(output.content.clone())),
                )
                .await;
        }
        Ok(output)
    }

    /// Check if a tool exists.
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Attached agent hooks, if any. Used by delivery paths outside tool
    /// execution (e.g. `deliver_reply`) that gate on hooks themselves.
    pub fn hooks(&self) -> Option<&HookSet> {
        self.hooks.as_deref()
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

    /// A group with two tools.
    fn mcp_group(server: &str) -> Arc<McpGroup> {
        Arc::new(McpGroup::new(
            server.to_string(),
            vec![
                McpToolEntry {
                    name: "create_issue".to_string(),
                    description: "Create an issue".to_string(),
                },
                McpToolEntry {
                    name: "search_repositories".to_string(),
                    description: "Search repositories".to_string(),
                },
            ],
        ))
    }

    /// The group's tools are sorted, so the index the model reads is stable.
    #[test]
    fn the_group_sorts_its_tools() {
        let group = McpGroup::new(
            "github".to_string(),
            vec![
                McpToolEntry {
                    name: "search_repositories".to_string(),
                    description: "Search repositories".to_string(),
                },
                McpToolEntry {
                    name: "create_issue".to_string(),
                    description: "Create an issue".to_string(),
                },
            ],
        );
        assert_eq!(
            group.tool_names(),
            vec!["create_issue", "search_repositories"]
        );
    }

    /// An expansion reports what it brought in; a second one brings in
    /// nothing.
    #[test]
    fn expanding_reports_only_the_new_tools() {
        let group = mcp_group("github");
        let newly = group.expand(&["create_issue".to_string()]);
        assert_eq!(newly, vec!["create_issue".to_string()]);
        assert!(group.is_expanded("create_issue"));
        assert!(!group.is_expanded("search_repositories"));

        let again = group.expand(&["create_issue".to_string()]);
        assert!(again.is_empty());
    }

    /// Expanding the whole server covers every tool it has.
    #[test]
    fn expanding_all_covers_every_tool() {
        let group = mcp_group("github");
        let newly = group.expand_all();
        assert_eq!(newly.len(), 2);
        assert!(group.is_expanded("create_issue"));
        assert!(group.is_expanded("search_repositories"));
    }

    /// The registry keeps the groups sorted by server name, whatever order
    /// they arrive in.
    #[test]
    fn the_groups_are_sorted_by_server_name() {
        let mut registry = ToolRegistry::new();
        registry.set_mcp_groups(vec![mcp_group("zulu"), mcp_group("alpha")]);
        let names: Vec<&str> = registry
            .mcp_groups()
            .iter()
            .map(|group| group.server_name())
            .collect();
        assert_eq!(names, vec!["alpha", "zulu"]);
        assert!(registry.mcp_group("alpha").is_some());
        assert!(registry.mcp_group("missing").is_none());
    }

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

    /// Tool order must be deterministic regardless of registration order,
    /// because prompt caching needs a byte-identical `tools` prefix across
    /// requests (and `HashMap` iteration is randomized per process).
    #[test]
    fn definitions_are_sorted_by_name() {
        let mut reg = ToolRegistry::new();
        for name in ["write", "bash", "read", "edit"] {
            reg.register(Box::new(MockTool {
                name: name.to_string(),
            }));
        }
        let names: Vec<String> = reg.definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["bash", "edit", "read", "write"]);
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

    fn hook_cfg(event: &str, script: &str) -> jyc_types::config::HookConfig {
        jyc_types::config::HookConfig {
            event: event.to_string(),
            matcher: None,
            shell: vec!["sh".into(), "-c".into(), script.into()],
            timeout: None,
        }
    }

    fn hook(event: &str, script: &str) -> Arc<HookSet> {
        Arc::new(HookSet::for_agent(&[hook_cfg(event, script)], "test"))
    }

    fn reg_with_mock() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(MockTool {
            name: "mock".to_string(),
        }));
        reg
    }

    #[tokio::test]
    async fn pre_tool_hook_exit2_blocks_call() {
        let mut reg = reg_with_mock();
        reg.set_hooks(hook("pre_tool_use", "echo nope >&2; exit 2"));
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        let err = reg.execute("mock", Value::Null, &ctx).await.unwrap_err();
        assert!(err.to_string().contains("nope"), "got: {err}");
    }

    #[tokio::test]
    async fn pre_tool_hook_exit0_allows_call() {
        let mut reg = reg_with_mock();
        reg.set_hooks(hook("pre_tool_use", "exit 0"));
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        let out = reg.execute("mock", Value::Null, &ctx).await.unwrap();
        assert_eq!(out.content, "executed");
    }

    #[tokio::test]
    async fn post_tool_hook_exit2_marks_error_appends_reason() {
        let mut reg = reg_with_mock();
        reg.set_hooks(hook("post_tool_use", "echo review this >&2; exit 2"));
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        let out = reg.execute("mock", Value::Null, &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("executed"));
        assert!(out.content.contains("review this"), "got: {}", out.content);
    }

    #[tokio::test]
    async fn hook_matcher_scopes_by_tool_name() {
        let mut reg = reg_with_mock();
        // matcher only fires for a tool literally named "other", not "mock".
        reg.set_hooks(Arc::new(HookSet::for_agent(
            &[jyc_types::config::HookConfig {
                event: "pre_tool_use".into(),
                matcher: Some("^other$".into()),
                shell: vec!["sh".into(), "-c".into(), "exit 2".into()],
                timeout: None,
            }],
            "test",
        )));
        let ctx = ToolContext::new(std::path::Path::new("/tmp"));
        // "mock" doesn't match "^other$" → proceeds unblocked.
        let out = reg.execute("mock", Value::Null, &ctx).await.unwrap();
        assert_eq!(out.content, "executed");
    }

    #[tokio::test]
    async fn post_hook_block_does_not_emit_failure_event() {
        let mut reg = reg_with_mock();
        // Tool SUCCEEDS; post hook blocks. The failure hook must not fire.
        reg.set_hooks(Arc::new(HookSet::for_agent(
            &[
                hook_cfg("post_tool_use", "echo p >> markers; exit 2"),
                hook_cfg("post_tool_use_failure", "echo f >> markers"),
            ],
            "test",
        )));
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(tmp.path());
        let out = reg.execute("mock", Value::Null, &ctx).await.unwrap();
        assert!(out.is_error, "post block must mark error");
        let markers = std::fs::read_to_string(tmp.path().join("markers")).unwrap();
        assert_eq!(
            markers.trim(),
            "p",
            "failure event fired for a hook-marked error"
        );
    }

    #[tokio::test]
    async fn tool_error_output_emits_failure_event() {
        struct ErrorTool;
        #[async_trait]
        impl Tool for ErrorTool {
            fn name(&self) -> &str {
                "mock"
            }
            fn description(&self) -> &str {
                "e"
            }
            fn input_schema(&self) -> Value {
                Value::Null
            }
            async fn execute(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput> {
                Ok(ToolOutput::error("boom"))
            }
        }
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(ErrorTool));
        reg.set_hooks(Arc::new(HookSet::for_agent(
            &[hook_cfg(
                "post_tool_use_failure",
                "grep -q boom - && echo f >> markers",
            )],
            "test",
        )));
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(tmp.path());
        let out = reg.execute("mock", Value::Null, &ctx).await.unwrap();
        assert!(out.is_error);
        let markers = std::fs::read_to_string(tmp.path().join("markers")).unwrap();
        assert_eq!(markers.trim(), "f");
    }
}
