//! Enumerable argument values per command — the data behind the TUI's
//! multi-level `/` popup.
//!
//! The inspect server folds this table into `CommandInfo::args` when it
//! builds `TopicInfo.commands`/`TopicSummary.commands`, so the popup can
//! offer every command's next level without knowing anything about the
//! commands itself. `/model` is just one row of this table, not a special
//! case in the client.
//!
//! Values come from two places: the command's own contract (the literals
//! each handler `match`es on) and per-topic state the caller already has
//! (models, skills, MCP names). A command whose arguments are free text
//! (`/grant <path>`, `/backlog push <text>`) gets an empty vec — the popup
//! then closes and the input field sends what was typed.
//!
//! Deliberately absent: `/exchange` (published names need a directory scan
//! per topic per poll) and `/ungrant` (grants live in the agent runtime, not
//! in the inspect payload). Both stay free text; add them when the popup
//! gets a lazy per-topic command fetch (see the `TODO(perf)` in the overview
//! builder).

use std::collections::HashSet;

use chrono::{Datelike, Utc};
use jyc_types::{AppConfig, CommandArg, McpServerConfig, ModelInfo};

/// Per-topic state the dynamic values are read from.
pub struct ArgCtx<'a> {
    /// Merged application config (source of the MCP server names).
    pub config: &'a AppConfig,
    /// Skills discoverable in this topic — the same list the overview
    /// payload already carries on `TopicInfo::skills`.
    pub skills: &'a [String],
    /// Models the configured providers expose.
    pub models: &'a [ModelInfo],
}

/// A leaf value with no further levels.
fn val(value: &str, description: &str) -> CommandArg {
    CommandArg {
        value: value.to_string(),
        description: description.to_string(),
        args: vec![],
    }
}

/// A value that opens another level (`/skill on <name>`).
fn val_with(value: &str, description: &str, args: Vec<CommandArg>) -> CommandArg {
    CommandArg {
        value: value.to_string(),
        description: description.to_string(),
        args,
    }
}

/// Bare names, no descriptions.
fn names<'a>(iter: impl Iterator<Item = &'a str>) -> Vec<CommandArg> {
    iter.map(|n| val(n, "")).collect()
}

/// Argument values for `command` (with its leading slash), empty when the
/// command takes free-text arguments.
pub fn command_args(command: &str, ctx: &ArgCtx) -> Vec<CommandArg> {
    match command {
        "/model" => {
            let mut args = names(ctx.models.iter().map(|m| m.name.as_str()));
            args.push(val("reset", "Clear the override, use the configured model"));
            args
        }
        "/thinking" => vec![
            val("show", "Show reasoning content"),
            val("hide", "Hide reasoning content"),
        ],
        "/context" => vec![
            val("full", "Keep the whole prior conversation verbatim"),
            // The handler also answers to `sliding_window`; the table offers
            // one spelling, and typing the other simply closes the popup.
            val("sliding", "Sliding window; optionally follow a window size"),
            val("reset", "Drop the runtime override"),
            val_with(
                "dump",
                "Toggle wire-payload dumping",
                vec![val("on", ""), val("off", "")],
            ),
        ],
        "/backlog" => vec![
            val("push", "Add an item"),
            val("list", "Show all items"),
            val("ls", "Show all items"),
            val("get", "Show one item by index"),
            val("pop", "Remove and show the next item"),
            val("rm", "Remove an item by index"),
            val("set", "Replace an item"),
        ],
        "/skill" => toggle_args(names(ctx.skills.iter().map(String::as_str))),
        "/mcp" => toggle_args(names(mcp_names(ctx.config).iter().map(String::as_str))),
        "/template" => vec![val(
            "update",
            "Re-apply the template, overwriting local edits",
        )],
        // The one argument these three guards accept, in any position. Without
        // it the popup would close on `/close ` even though `--force` exists.
        "/close" | "/new" | "/reset" => vec![val("--force", "Skip the confirmation guard")],
        "/bill" => {
            let mut args = recent_months();
            args.push(val("all", "Every day with recorded usage"));
            args
        }
        _ => vec![],
    }
}

/// `<cmd> on|off|reset <name>` — `reset` clears everything at once, so only
/// `on` and `off` carry the name list.
fn toggle_args(targets: Vec<CommandArg>) -> Vec<CommandArg> {
    vec![
        val_with("on", "Force this name on for the topic", targets.clone()),
        val_with("off", "Force this name off for the topic", targets),
        val("reset", "Clear all overrides for this topic"),
    ]
}

/// The `YYYY-MM` scopes of this month and the two before it, newest first.
fn recent_months() -> Vec<CommandArg> {
    let today = Utc::now().date_naive();
    let index = today.year() as i64 * 12 + today.month() as i64 - 1;
    (0..3)
        .map(|back| {
            let at = index - back;
            val(
                &format!("{:04}-{:02}", at.div_euclid(12), at.rem_euclid(12) + 1),
                "",
            )
        })
        .collect()
}

/// MCP server names from every config layer, deduplicated and sorted.
///
/// Mirrors `toggle_handler::mcp_known` except for the L3 topic
/// overlay, which would cost a config file read per topic per poll.
/// ponytail: include the overlay once the popup fetches commands lazily.
fn mcp_names(cfg: &AppConfig) -> Vec<String> {
    let collect =
        |mcps: &[McpServerConfig]| mcps.iter().map(|m| m.name.clone()).collect::<Vec<_>>();
    let mut out: HashSet<String> = HashSet::new();
    out.extend(collect(&cfg.mcps));
    for ch in cfg.channels.values() {
        if let Some(mcps) = &ch.mcps {
            out.extend(collect(mcps));
        }
        for p in ch.patterns.iter().flatten() {
            if let Some(mcps) = &p.mcps {
                out.extend(collect(mcps));
            }
        }
    }
    for agent in cfg.agents.values() {
        if let Some(mcps) = &agent.mcps {
            out.extend(collect(mcps));
        }
    }
    let mut names: Vec<String> = out.into_iter().collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(config: &'a AppConfig, skills: &'a [String], models: &'a [ModelInfo]) -> ArgCtx<'a> {
        ArgCtx {
            config,
            skills,
            models,
        }
    }

    fn find<'a>(args: &'a [CommandArg], value: &str) -> Option<&'a CommandArg> {
        args.iter().find(|a| a.value == value)
    }

    #[test]
    fn model_lists_models_then_reset() {
        let cfg = AppConfig::default();
        let models = vec![
            ModelInfo {
                name: "deepseek/deepseek-chat".into(),
            },
            ModelInfo {
                name: "claude/sonnet".into(),
            },
        ];
        let args = command_args("/model", &ctx(&cfg, &[], &models));
        assert_eq!(args.len(), 3);
        assert_eq!(args[0].value, "deepseek/deepseek-chat");
        assert!(args[0].args.is_empty(), "a model id is a leaf");
        assert!(find(&args, "reset").is_some());
    }

    #[test]
    fn toggle_levels_nest_under_on_and_off_only() {
        let cfg = AppConfig::default();
        let skills = vec!["ponytail".to_string()];
        let args = command_args("/skill", &ctx(&cfg, &skills, &[]));
        assert_eq!(
            args.iter().map(|a| a.value.as_str()).collect::<Vec<_>>(),
            ["on", "off", "reset"]
        );
        assert_eq!(find(&args, "on").unwrap().args[0].value, "ponytail");
        assert_eq!(find(&args, "off").unwrap().args.len(), 1);
        assert!(
            find(&args, "reset").unwrap().args.is_empty(),
            "`reset` takes no name"
        );
    }

    #[test]
    fn context_dump_has_its_own_level() {
        let cfg = AppConfig::default();
        let args = command_args("/context", &ctx(&cfg, &[], &[]));
        let dump = find(&args, "dump").unwrap();
        assert_eq!(
            dump.args
                .iter()
                .map(|a| a.value.as_str())
                .collect::<Vec<_>>(),
            ["on", "off"]
        );
        assert!(find(&args, "sliding").unwrap().args.is_empty());
    }

    #[test]
    fn force_flag_is_offered_where_the_handlers_accept_it() {
        let cfg = AppConfig::default();
        let empty = ctx(&cfg, &[], &[]);
        for cmd in ["/close", "/new", "/reset"] {
            assert_eq!(
                command_args(cmd, &empty)
                    .iter()
                    .map(|a| a.value.as_str())
                    .collect::<Vec<_>>(),
                ["--force"],
                "{cmd}'s only argument"
            );
        }
    }

    #[test]
    fn free_text_commands_have_no_values() {
        let cfg = AppConfig::default();
        let empty = ctx(&cfg, &[], &[]);
        for cmd in ["/grant", "/plan", "/pin", "/exchange", "/ungrant"] {
            assert!(command_args(cmd, &empty).is_empty(), "{cmd} must not");
        }
    }

    #[test]
    fn mcp_names_merge_layers_and_dedup() {
        let mut cfg = AppConfig::default();
        cfg.mcps.push(mcp("global"));
        cfg.agents.insert(
            "jyc".into(),
            jyc_types::AgentConfig {
                mcps: Some(vec![mcp("agent"), mcp("global")]),
                ..Default::default()
            },
        );
        assert_eq!(mcp_names(&cfg), vec!["agent", "global"]);
    }

    fn mcp(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            kind: jyc_types::McpServerKind::Local {
                command: vec!["true".into()],
                environment: Default::default(),
            },
            enabled_tools: None,
            timeout_ms: None,
        }
    }

    #[test]
    fn recent_months_are_sorted_descending() {
        let args = recent_months();
        assert_eq!(args.len(), 3);
        assert!(
            args[0].value > args[2].value,
            "{} should be newer than {}",
            args[0].value,
            args[2].value
        );
        assert!(
            args[0]
                .value
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'-')
        );
    }
}
