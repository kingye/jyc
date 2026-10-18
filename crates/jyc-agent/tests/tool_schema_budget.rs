//! The in-process tool schemas ride in every request's stable prompt prefix —
//! #935 is what made that prefix byte-stable — so their text is a standing
//! per-call cost and their shape is a standing contract with the model.
//!
//! Two things are pinned here:
//!
//! * **the built-in tool set, by name** — a rename or a drop changes what the
//!   model may call, and it invalidates every provider prefix cache. The
//!   expected set is derived from the same calls `build_tool_registry` makes,
//!   so registering a 22nd tool without saying so here fails the test rather
//!   than quietly enlarging every prompt.
//! * **the description text, by budget** — the 2026-10-18 slimming pass took the
//!   in-process natural language from 10,108 to 8,349 chars (-17.4%) without
//!   touching a single name, parameter name, type or required list. The budgets
//!   carry a few percent of slack, so an ordinary wording fix does not have to
//!   bump them.
//!
//! Editing a description is therefore a deliberate act that costs one round of
//! prefix cache misses. Keep such an edit in its own change, where the diff
//! shows it on its own.

use std::sync::Arc;

use arc_swap::ArcSwap;
use jyc_agent::tools::builtin::spawn_subagent::SpawnSubagentTool;
use jyc_agent::tools::builtin::{
    create_builtin_registry, register_publish_file, register_read_image,
};
use jyc_agent::tools::mcp_bridge::register_mcp_tools;
use jyc_agent::types::ToolDefinition;
use jyc_types::AppConfig;

/// The in-process tools, assembled the way `build_tool_registry` assembles
/// them: the built-in registry, then `read_image` and `publish_file`, then
/// `spawn_subagent` — which is registered only on the synthesized `agents`
/// channel, the one place a sibling topic is addressable, so taking it here is
/// the generous reading — and finally the MCP *bridge* tools, which
/// `register_mcp_tools` adds to every registry and which ship a built-in
/// schema of their own rather than a server's.
///
/// Both configuration-dependent values are irrelevant to the text being
/// budgeted: `publish_file`'s base URL and `read_image`'s two switches reach
/// `execute`, not `input_schema`.
///
/// MCP *server* tools are absent by construction — they arrive per channel
/// config, and their schemas belong to whoever runs the server.
fn built_in_tools() -> Vec<ToolDefinition> {
    let mut registry = create_builtin_registry();
    register_read_image(&mut registry, false, None);
    register_publish_file(&mut registry, "http://127.0.0.1:8080".to_string());
    registry.register(Box::new(SpawnSubagentTool {
        config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
    }));
    register_mcp_tools(&mut registry);
    registry.definitions()
}

/// Per tool: `(name, description budget, parameter-description budget)` in
/// characters. Measured against the 2026-10-18 slimming pass and rounded up with
/// slack; a tool added or removed shows up as a failure here rather than as a
/// silent change in what every prompt carries.
const BUDGETS: &[(&str, usize, usize)] = &[
    ("ask_user", 465, 250),
    ("bash", 180, 75),
    ("context_browse", 280, 100),
    ("edit", 235, 125),
    ("glob", 100, 125),
    ("grep", 165, 145),
    ("job_list", 175, 0),
    ("job_create", 385, 300),
    ("job_delete", 135, 30),
    ("job_toggle", 95, 95),
    ("jyc_publish_file", 275, 225),
    ("read", 185, 200),
    ("read_image", 425, 195),
    ("jyc_send_message", 215, 380),
    ("jyc_send_to_topic", 315, 630),
    ("jyc_spawn_subagent", 1140, 515),
    ("task_list", 280, 0),
    ("task_create", 425, 60),
    ("task_update", 345, 80),
    ("webfetch", 140, 55),
    ("write", 140, 115),
];

/// The whole in-process set, in characters: 8,349 when the budgets above were
/// set, plus `task_create`'s `format!` template and `jyc_send_message`.
const TOTAL_BUDGET: usize = 9_400;

/// `jyc_spawn_subagent`'s `type` description ends with the catalog of roles
/// configured *on this machine*, so its length is configuration rather than
/// something a test may pin.
fn is_machine_dependent(tool: &str, property: &str) -> bool {
    tool == "jyc_spawn_subagent" && property == "type"
}

/// Characters of parameter descriptions, minus the machine-dependent ones.
fn param_text(tool: &ToolDefinition) -> usize {
    tool.input_schema
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|props| {
            props
                .iter()
                .filter(|(name, _)| !is_machine_dependent(&tool.name, name))
                .filter_map(|(_, schema)| schema.get("description").and_then(|d| d.as_str()))
                .flat_map(str::chars)
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn the_built_in_tool_set_is_stable() {
    let tools = built_in_tools();
    let mut registered: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    registered.sort_unstable();
    let mut budgeted: Vec<&str> = BUDGETS.iter().map(|(name, _, _)| *name).collect();
    budgeted.sort_unstable();

    assert_eq!(
        registered, budgeted,
        "the built-in tool set moved: a rename or a drop is an API change, and it \
         invalidates every provider prefix cache. Add or remove its budget deliberately."
    );
}

#[test]
fn descriptions_stay_within_budget() {
    for tool in built_in_tools() {
        let (_, desc_budget, param_budget) = BUDGETS
            .iter()
            .find(|(name, _, _)| *name == tool.name)
            .unwrap_or_else(|| panic!("{} has no budget", tool.name));

        let desc = tool.description.chars().count();
        assert!(
            desc <= *desc_budget,
            "{}: description is {desc} chars, over its {desc_budget} budget — schema text \
             rides in every prompt's cached prefix, so growth is a standing cost",
            tool.name
        );

        let params = param_text(&tool);
        assert!(
            params <= *param_budget,
            "{}: parameter descriptions total {params} chars, over their {param_budget} budget",
            tool.name
        );
    }
}

#[test]
fn schema_text_total_stays_within_budget() {
    let total: usize = built_in_tools()
        .iter()
        .map(|t| t.description.chars().count() + param_text(t))
        .sum();
    assert!(
        total <= TOTAL_BUDGET,
        "built-in schema text is {total} chars, over the {TOTAL_BUDGET} budget \
         (10,108 before the 2026-10-18 slimming pass)"
    );
}

#[test]
fn every_tool_says_what_it_is_for() {
    // A budget is only safe while the text still carries its load: a description
    // that says nothing about when to reach for the tool is a bug the byte count
    // cannot see.
    for tool in built_in_tools() {
        assert!(
            tool.description.chars().count() > 40,
            "{}: description is too thin to guide a call: {:?}",
            tool.name,
            tool.description
        );
    }
}
