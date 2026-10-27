//! The in-process tool schemas ride in every request's stable prompt prefix —
//! #935 is what made that prefix byte-stable — so their text is a standing
//! per-call cost and their shape is a standing contract with the model.
//!
//! Three things are pinned here:
//!
//! * **the built-in tool set, by name** — a rename or a drop changes what the
//!   model may call, and it invalidates every provider prefix cache. The
//!   expected set is derived from the same calls `build_tool_registry` makes,
//!   so registering a 20th tool without saying so here fails the test rather
//!   than quietly enlarging every prompt.
//! * **the advertised set** — the tools a request actually carries. The
//!   extended group (`load_extended_tools`) is registered but kept out of that
//!   set until the model loads it, so the set a request pays for is the core
//!   one, not the whole registry.
//! * **the description text, by budget** — the 2026-10-18 slimming pass took the
//!   in-process natural language from 10,108 to 8,349 chars (-17.4%) without
//!   touching a single name, parameter name, type or required list. The
//!   2026-10-26 pass removed five tools (the four `job_*` and `glob`, whose
//!   text was 1,319 chars) and trimmed the rest by a further 1,272 chars, so
//!   the set is now 6,925. The budgets
//!   carry a few percent of slack, so an ordinary wording fix does not have to
//!   bump them. The *total* ceiling is a different thing: it is the standing
//!   per-call cost of the whole set, and it was raised once on purpose (#971,
//!   the two lesson tools) rather than paid for by squeezing a description
//!   until the model stopped understanding what the tool was for.
//!
//! Editing a description is therefore a deliberate act that costs one round of
//! prefix cache misses. Keep such an edit in its own change, where the diff
//! shows it on its own.

use std::sync::Arc;

use arc_swap::ArcSwap;
use jyc_agent::tools::builtin::extended::{EXTENDED_TOOLS, register_extended_tools};
use jyc_agent::tools::builtin::spawn_subagent::SpawnSubagentTool;
use jyc_agent::tools::builtin::{
    create_builtin_registry, register_publish_file, register_read_image,
};
use jyc_agent::tools::mcp_bridge::register_mcp_tools;
use jyc_agent::tools::registry::ToolRegistry;
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
/// The extended group goes on last, exactly as the service wires it: the
/// group is whatever the registry ended up with.
///
/// Both configuration-dependent values are irrelevant to the text being
/// budgeted: `publish_file`'s base URL and `read_image`'s two switches reach
/// `execute`, not `input_schema`.
///
/// MCP *server* tools are absent by construction — they arrive per channel
/// config, and their schemas belong to whoever runs the server.
fn built_in_registry() -> ToolRegistry {
    let mut registry = create_builtin_registry();
    register_read_image(&mut registry, false, None);
    register_publish_file(&mut registry, "http://127.0.0.1:8080".to_string());
    registry.register(Box::new(SpawnSubagentTool {
        config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
    }));
    register_mcp_tools(&mut registry);
    register_extended_tools(&mut registry);
    registry
}

/// Every registered tool — what a request carries once the model has loaded
/// the extended group.
fn built_in_tools() -> Vec<ToolDefinition> {
    built_in_registry().all_definitions()
}

/// The tools a request carries *before* anything is loaded: the core set.
fn advertised_tools() -> Vec<ToolDefinition> {
    built_in_registry().definitions()
}

/// Per tool: `(name, description budget, parameter-description budget)` in
/// characters. Measured against the 2026-10-18 slimming pass and rounded up with
/// slack; a tool added or removed shows up as a failure here rather than as a
/// silent change in what every prompt carries.
const BUDGETS: &[(&str, usize, usize)] = &[
    ("ask_user", 330, 290),
    ("bash", 128, 46),
    ("context_browse", 205, 101),
    ("edit", 222, 124),
    ("grep", 117, 113),
    ("jyc_publish_file", 211, 224),
    ("read", 121, 199),
    ("read_image", 327, 196),
    ("jyc_send_message", 131, 430),
    ("jyc_send_to_topic", 230, 562),
    ("jyc_spawn_subagent", 1173, 380),
    ("lesson_add", 220, 0),
    ("lesson_read", 114, 0),
    ("list_topics", 240, 0),
    ("load_extended_tools", 600, 0),
    ("task_list", 278, 0),
    ("task_create", 345, 46),
    ("task_update", 305, 81),
    ("webfetch", 96, 35),
    ("write", 112, 80),
];

/// The whole registered set, in characters. It was 8,349 when the budgets
/// above were set; other tools have grown it since, and the two lesson
/// tools (#971) brought it to 9,511 — 9,487 after the send tool's wording
/// was trimmed to say what a send does not do, and 9,479 after the spawn
/// tool's said what a send to a closed child does. The 2026-10-26 pass —
/// five tools removed, the rest trimmed — brought it to 6,888; the
/// follow-up that put back the two rules the trim had cut (the spawn tool's
/// "its state dir stays its own", the ask_user tool's "the turn ends") took
/// it to 6,925. The ceiling is the standing per-call cost of every
/// request's prefix, so raising it is a deliberate act — it was raised for
/// #971, where the alternative was to cut a tool's description until the
/// model stopped understanding what the tool was for.
///
/// The extended group raised it once more (the two new tools: `list_topics`
/// and the loader's index), but the *advertised* set — what a request
/// actually carries — went down, which is the number that matters. See
/// [`ADVERTISED_TOTAL_BUDGET`].
const TOTAL_BUDGET: usize = 7_800;

/// The advertised set — the core tools plus the loader's index — in
/// characters. This is the standing per-call cost after the extended group
/// was moved out of the request: the six extended tools are advertised only
/// after the model loads them, and the loader's own index is what it reads
/// instead. `jyc_send_message` stays in the advertised set because attachment
/// replies must be discoverable without a loader call.
///
/// A topic that never delegates, never publishes and never fetches a URL
/// pays this and not the whole registry.
const ADVERTISED_TOTAL_BUDGET: usize = 4_600;

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

/// Characters of description plus parameter text.
fn schema_text(tool: &ToolDefinition) -> usize {
    tool.description.chars().count() + param_text(tool)
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

/// The extended tools must not ride in a request before the model asks for
/// them — that is the whole point of the group: their schemas are a standing
/// per-call cost, and most turns never need them.
#[test]
fn the_extended_tools_are_not_advertised_until_they_are_loaded() {
    let registry = built_in_registry();
    let advertised: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();

    for (name, _) in EXTENDED_TOOLS {
        assert!(
            !advertised.iter().any(|n| n == name),
            "{name} is advertised before it was loaded — the extended group is supposed to \
             keep its schemas out of the request"
        );
    }
    assert!(
        advertised.iter().any(|n| n == "load_extended_tools"),
        "the loader must be advertised — it is how the model finds the group: {advertised:?}"
    );
    assert!(
        advertised.iter().any(|n| n == "read"),
        "the core tools must still be advertised: {advertised:?}"
    );
}

/// `jyc_send_message` carries attachment-bearing replies, so it must be
/// discoverable in the core set rather than behind the loader.
#[test]
fn send_message_is_advertised_without_loading_the_extended_group() {
    let registry = built_in_registry();
    let advertised = registry.definitions();
    let names: Vec<&str> = advertised.iter().map(|d| d.name.as_str()).collect();

    assert!(
        names.contains(&"jyc_send_message"),
        "jyc_send_message must be advertised before the group is loaded: {names:?}"
    );

    let loader = advertised
        .iter()
        .find(|d| d.name == "load_extended_tools")
        .expect("the loader is advertised");
    assert!(
        loader
            .description
            .contains("Already available: read, write, edit, grep, ask_user, jyc_send_message."),
        "the loader must list jyc_send_message as already available: {}",
        loader.description
    );
    assert!(
        !loader
            .description
            .contains("jyc_send_message (send a proactive message to a channel)"),
        "jyc_send_message must not remain in the loaded group: {}",
        loader.description
    );
}

/// Loading the group advertises it: the flag is what the registry reads on
/// every request, so the next model call after the load carries the schemas.
#[test]
fn loading_the_group_advertises_the_extended_tools() {
    let registry = built_in_registry();
    let group = registry
        .extended()
        .expect("the registry has an extended group")
        .clone();
    assert!(!group.is_loaded());

    group.load();

    let advertised: Vec<String> = registry.definitions().into_iter().map(|d| d.name).collect();
    for (name, _) in EXTENDED_TOOLS {
        assert!(
            advertised.iter().any(|n| n == name),
            "{name} is still hidden after the group was loaded: {advertised:?}"
        );
    }
    assert_eq!(
        advertised.len(),
        registry.all_definitions().len(),
        "a loaded group must advertise every registered tool"
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
    let total: usize = built_in_tools().iter().map(schema_text).sum();
    assert!(
        total <= TOTAL_BUDGET,
        "built-in schema text is {total} chars, over the {TOTAL_BUDGET} budget \
         (10,108 before the 2026-10-18 slimming pass)"
    );
}

/// The number that is actually billed: what a request carries before the
/// model loads anything.
#[test]
fn advertised_schema_text_stays_within_budget() {
    let total: usize = advertised_tools().iter().map(schema_text).sum();
    assert!(
        total <= ADVERTISED_TOTAL_BUDGET,
        "advertised schema text is {total} chars, over the {ADVERTISED_TOTAL_BUDGET} budget"
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
