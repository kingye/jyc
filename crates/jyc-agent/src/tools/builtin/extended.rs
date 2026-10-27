//! The extended tools — the group the model loads on demand.
//!
//! A tool schema is a standing per-call cost: it rides in every request's
//! stable prompt prefix, and a topic that never delegates, never publishes
//! and never looks at the web pays for those schemas on every single call.
//! The extended group is the part of the tool set that is *not* worth that:
//! the tools most topics use rarely or never.
//!
//! The model discovers the group through one tool, `load_extended_tools`,
//! whose description is the index of what it brings in. Calling it flips a
//! flag; from the next model call on, the group's schemas are part of the
//! advertised tool list. The flag is sticky for the life of the registry,
//! and the registry is cached per topic — so a topic pays the load once, and
//! a topic that never loads never pays at all. A rebuild (a config reload,
//! an `/mcp` override) starts a fresh registry, and the group hides again
//! until it is loaded once more.
//!
//! Only *advertisement* is gated. An extended tool stays registered and
//! executable, so a model that names one before loading it gets a working
//! call instead of a "not found" error: the group is a cost optimization,
//! not a permission boundary. The permission boundary is `disabled_tools`,
//! which removes a tool from the registry altogether.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolOutput};
use crate::tools::registry::{ExtendedTools, ToolRegistry};

/// The extended tools, by name, with the one-liner that tells the model
/// what each is for.
///
/// The one-liners are the whole discovery story: they ride in
/// `load_extended_tools`'s description, so the model learns what the group
/// holds without the group's schemas being in the request. A tool that is
/// not registered in a given registry (e.g. `jyc_spawn_subagent`, which only
/// exists on the synthesized `agents` channel) is left out of the index
/// rather than advertised as something the call would fail on.
pub const EXTENDED_TOOLS: &[(&str, &str)] = &[
    ("jyc_spawn_subagent", "delegate a task to a subagent topic"),
    ("jyc_send_to_topic", "message another topic"),
    ("list_topics", "list the channels and their active topics"),
    ("jyc_publish_file", "publish a file as a download link"),
    ("read_image", "view an image"),
    ("webfetch", "fetch a URL as text"),
    ("context_browse", "browse earlier turns"),
    ("lesson_add", "record a reusable lesson"),
    ("lesson_read", "read earlier recorded lessons"),
];

/// The core tools the loader mentions as already available.
///
/// These are the ones a request carries whatever the model does — the file
/// tools, the shell, the question tool, `jyc_send_message` (which carries
/// attachment-bearing replies), and the task tools (which every multi-step
/// turn and every post-compaction resume needs). The list is filtered by what
/// the registry actually holds, so a role that disables one of them is never
/// told it is there.
const CORE_TOOLS: &[&str] = &[
    "read",
    "write",
    "edit",
    "bash",
    "grep",
    "ask_user",
    "jyc_send_message",
    "task_list",
    "task_create",
    "task_update",
];

/// Bring the extended group into a registry: mark the tools it names, and
/// register the tool that loads them.
///
/// Call it once the rest of the registry is assembled — the group is
/// whatever is actually present, so a tool removed by `disabled_tools` (or
/// never registered, like `jyc_spawn_subagent` off the `agents` channel) is
/// not part of it.
pub fn register_extended_tools(registry: &mut ToolRegistry) {
    let present: Vec<(&str, &str)> = EXTENDED_TOOLS
        .iter()
        .copied()
        .filter(|(name, _)| registry.has_tool(name))
        .collect();
    if present.is_empty() {
        return;
    }

    // The core tools this registry actually has: the loader says what is
    // already available, and a tool that was removed (by `disabled_tools`)
    // is not available.
    let core: Vec<&str> = CORE_TOOLS
        .iter()
        .copied()
        .filter(|name| registry.has_tool(name))
        .collect();

    let group = Arc::new(ExtendedTools::new(
        present
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect(),
    ));
    let description = LoadExtendedToolsTool::description_for(&present, &core);
    registry.register(Box::new(LoadExtendedToolsTool {
        group: group.clone(),
        present,
        description,
    }));
    registry.set_extended(group);
}

/// The tool that loads the extended group.
pub struct LoadExtendedToolsTool {
    /// The shared group state this tool flips.
    group: Arc<ExtendedTools>,
    /// The group's tools and their one-liners, as they are in this registry.
    present: Vec<(&'static str, &'static str)>,
    /// The index of the group, built once when the tool is registered.
    description: String,
}

impl LoadExtendedToolsTool {
    /// The description, built from the tools this registry actually has.
    fn description_for(present: &[(&str, &str)], core: &[&str]) -> String {
        let mut text = String::from(
            "Load the extended tools into your tool list; they remain there while this topic's \
             registry is reused. Call before using:",
        );
        for (name, what) in present {
            text.push_str(&format!(" {name} ({what});"));
        }
        if !core.is_empty() {
            text.push_str(&format!(" Already available: {}.", core.join(", ")));
        }
        text
    }
}

#[async_trait]
impl Tool for LoadExtendedToolsTool {
    fn name(&self) -> &str {
        "load_extended_tools"
    }

    fn description(&self) -> &str {
        // Built once per registry and stored, so the trait can hand out a
        // borrow of it.
        &self.description
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let names: Vec<&str> = self.present.iter().map(|(name, _)| *name).collect();
        let first = self.group.load();
        if first {
            Ok(ToolOutput::success(format!(
                "Loaded {} extended tools: {}. They are in your tool list from the next model \
                 call on, while this topic's tool registry is reused — a config reload or an \
                 `/mcp` change rebuilds it, and hides them again until it is loaded once more.",
                names.len(),
                names.join(", ")
            )))
        } else {
            Ok(ToolOutput::success(format!(
                "The extended tools are already loaded: {}.",
                names.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::builtin::{create_builtin_registry, register_publish_file};

    /// A registry with the group wired the way the service wires it.
    fn registry_with_group() -> ToolRegistry {
        let mut registry = create_builtin_registry();
        register_publish_file(&mut registry, "http://127.0.0.1:8080".to_string());
        register_extended_tools(&mut registry);
        registry
    }

    /// The group is out of the request until the model asks for it.
    #[test]
    fn the_group_is_hidden_until_it_is_loaded() {
        let registry = registry_with_group();
        let advertised: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();

        assert!(
            !advertised.iter().any(|n| n == "jyc_send_to_topic"),
            "an extended tool must not be advertised before it is loaded: {advertised:?}"
        );
        assert!(
            advertised.iter().any(|n| n == "load_extended_tools"),
            "the loader is how the model finds the group: {advertised:?}"
        );
        assert!(
            advertised.iter().any(|n| n == "read"),
            "the core tools stay advertised: {advertised:?}"
        );
    }

    /// Calling the loader advertises the group — and the loader is itself
    /// part of what a request carries, so the discovery index is the only
    /// thing a topic that never loads pays for.
    #[tokio::test]
    async fn loading_the_group_advertises_it() {
        let registry = registry_with_group();
        let group = registry.extended().expect("a group").clone();
        let tool = registry
            .execute(
                "load_extended_tools",
                json!({}),
                &ToolContext::new(std::path::Path::new(".")),
            )
            .await
            .expect("the loader runs");

        assert!(group.is_loaded(), "the group must be loaded");
        assert!(
            tool.content.contains("jyc_send_to_topic"),
            "the result must name what it brought in: {}",
            tool.content
        );
        let advertised: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
        assert!(
            advertised.iter().any(|n| n == "jyc_send_to_topic"),
            "a loaded group must be advertised: {advertised:?}"
        );
    }

    /// A tool that is not registered is not part of the group: the index
    /// must not promise something the call would fail on.
    #[test]
    fn a_tool_that_is_not_registered_is_not_part_of_the_group() {
        let mut registry = create_builtin_registry();
        registry.remove("jyc_send_to_topic");
        register_extended_tools(&mut registry);

        let group = registry.extended().expect("a group");
        assert!(
            !group.names().iter().any(|n| n == "jyc_send_to_topic"),
            "a removed tool must not be in the group: {:?}",
            group.names()
        );
    }

    /// With nothing to load, there is no loader: an empty group would only
    /// add a schema to every request.
    #[test]
    fn no_group_means_no_loader() {
        let mut registry = ToolRegistry::new();
        register_extended_tools(&mut registry);
        assert!(registry.extended().is_none());
        assert!(!registry.has_tool("load_extended_tools"));
    }

    /// The loader must not offer a tool this registry does not have: a role
    /// that disables `bash` must not be told the shell is available.
    #[test]
    fn the_loader_offers_only_the_core_tools_that_are_there() {
        let mut registry = create_builtin_registry();
        registry.remove("bash");
        register_extended_tools(&mut registry);

        let loader = registry
            .definitions()
            .into_iter()
            .find(|d| d.name == "load_extended_tools")
            .expect("the loader is registered");
        assert!(
            !loader.description.contains("bash"),
            "the loader must not offer a tool that was removed: {}",
            loader.description
        );
        assert!(
            loader.description.contains(
                "Already available: read, write, edit, grep, ask_user, task_list, task_create, \
                 task_update."
            ),
            "the loader must still say what is available: {}",
            loader.description
        );
    }
}
