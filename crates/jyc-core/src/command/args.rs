//! Enumerable argument values per command — the data behind the TUI's
//! multi-level `/` popup.
//!
//! The inspect server folds this table into `CommandInfo::args` when it
//! builds `TopicInfo.commands`/`TopicSummary.commands`, so the popup can
//! offer every command's next level without knowing anything about the
//! commands itself. `/model` is just one row of this table, not a special
//! case in the client.
//!
//! Values come from three places: the command's own contract (the literals
//! each handler `match`es on), per-topic state the caller already has
//! (models, skills, MCP names), and the instance's own subagent types
//! (`/spawn`'s first level). A command whose arguments are free text
//! (`/grant <path>`, `/backlog push <text>`) gets an empty vec — the popup
//! then closes and the input field sends what was typed.
//!
//! Deliberately absent: `/exchange` (published names need a directory scan
//! per topic per poll, where `/spawn`'s types are resolved once for the whole
//! payload) and `/ungrant` (grants live in the agent runtime, not in the
//! inspect payload). Both stay free text; add them when the popup gets a lazy
//! per-topic command fetch (see the `TODO(perf)` in the overview builder).

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
    /// The subagent types a spawn would accept, as `(name, description)`, with
    /// the start defaults each role declares appended to its line — read from
    /// the installed role files by the caller, once for the whole payload (see
    /// `spawn_handler::type_catalog_pairs`).
    pub subagent_types: &'a [(String, String)],
    /// The agents `/send` can address: the active topics of the agents channel
    /// — the same list the prompt shows as "active topics". Read by the caller
    /// once for the whole payload, since it is the same for every topic.
    pub agents: &'a [String],
    /// This topic's own direct subagents — the only names `/close` accepts.
    /// Read per topic by the caller (`spawn_handler::subagents_of`).
    pub subagents: &'a [String],
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
        "/spawn" => spawn_args(ctx),
        "/skill" => toggle_args(names(ctx.skills.iter().map(String::as_str))),
        "/mcp" => toggle_args(names(mcp_names(ctx.config).iter().map(String::as_str))),
        "/template" => vec![val(
            "update",
            "Re-apply the template, overwriting local edits",
        )],
        // `/send` addresses any agent (it does not check the name), so it
        // offers every agent; `/close` accepts only this topic's own
        // subagents, so it offers the narrower list.
        "/send" => send_args(ctx),
        "/close" => close_args(ctx),
        "/new" | "/reset" => vec![val("--force", "Skip the confirmation guard")],
        "/bill" => {
            let mut args = recent_months();
            args.push(val("all", "Every day with recorded usage"));
            args
        }
        _ => vec![],
    }
}

/// `/spawn`'s next levels: the types a spawn accepts, and then the four flags.
///
/// The type is the only positional, so it is what the popup offers first — and
/// each type carries the whole flag set again, because `/spawn review -n worker`
/// is as ordinary as `/spawn -n worker`. A line that names a start default
/// (`shares your directory`) is a role that declares one: on the command line
/// the *absence* of `--share` is what lets that default stand.
fn spawn_args(ctx: &ArgCtx) -> Vec<CommandArg> {
    let mut args: Vec<CommandArg> = ctx
        .subagent_types
        .iter()
        .map(|(name, description)| val_with(name, description, spawn_flags()))
        .collect();
    args.extend(spawn_flags());
    args
}

/// `/send`'s next levels: every agent, then the flags that precede the message.
///
/// Each agent carries the flag set again — `/send invoice --reply <message>` is
/// as ordinary as `/send --reply invoice <message>` — and the flags come first
/// because that is where `parse` stops reading them.
fn send_args(ctx: &ArgCtx) -> Vec<CommandArg> {
    let mut args: Vec<CommandArg> = ctx
        .agents
        .iter()
        .map(|name| val_with(name, "Send to this agent", send_flags()))
        .collect();
    args.extend(send_flags());
    args
}

/// The flags `/send` takes before the message.
fn send_flags() -> Vec<CommandArg> {
    vec![
        val("--topic", "One of that agent's topics (-t)"),
        val("--reply", "Ask the target to send its result back"),
        val("--attach", "Attach a file from this topic's directory (-a)"),
        val("--recipient", "Override the recipient address (-r)"),
    ]
}

/// `/close`'s next levels: this topic's own subagents — the only names it
/// accepts — then `all`, then the way to close this topic itself.
fn close_args(ctx: &ArgCtx) -> Vec<CommandArg> {
    let mut args: Vec<CommandArg> = ctx
        .subagents
        .iter()
        .map(|name| {
            val_with(
                name,
                "Close this subagent (it closes itself on its own queue)",
                close_flags(),
            )
        })
        .collect();
    if !ctx.subagents.is_empty() {
        args.push(val("all", "Close every direct subagent"));
    }
    // A bare `/close` closes this topic, and the popup needs a row for exactly
    // that: a space completes to `/close  `, which carries no argument.
    args.push(val(" ", "Close this topic (self)"));
    args.extend(close_flags());
    args
}

/// What `/close` accepts after a name: the confirmation guard, and the dir.
fn close_flags() -> Vec<CommandArg> {
    vec![
        val("--force", "Skip the confirmation guard"),
        val("--purge", "Delete the topic dir too, not just its state"),
    ]
}

/// `/spawn`'s flags, long spelling each — the one the popup inserts — with the
/// short one named in the description.
fn spawn_flags() -> Vec<CommandArg> {
    vec![
        val(
            "--inherit",
            "Copy the conversation and the transcript too (-i)",
        ),
        val("--share", "Work in this topic's own directory (-s)"),
        val("--name", "Name the new topic; defaults to <this>-2 (-n)"),
        val(
            "--dir",
            "Where the child works; defaults to <agents dir>/<name> (-d)",
        ),
    ]
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
            subagent_types: &[],
            agents: &[],
            subagents: &[],
        }
    }

    fn ctx_with_types<'a>(config: &'a AppConfig, types: &'a [(String, String)]) -> ArgCtx<'a> {
        ArgCtx {
            config,
            skills: &[],
            models: &[],
            subagent_types: types,
            agents: &[],
            subagents: &[],
        }
    }

    /// A context carrying the two name lists `/send` and `/close` are built from.
    fn ctx_with_names<'a>(
        config: &'a AppConfig,
        agents: &'a [String],
        subagents: &'a [String],
    ) -> ArgCtx<'a> {
        ArgCtx {
            config,
            skills: &[],
            models: &[],
            subagent_types: &[],
            agents,
            subagents,
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
        for (cmd, expected) in [
            ("/close", vec![" ", "--force", "--purge"]),
            ("/new", vec!["--force"]),
            ("/reset", vec!["--force"]),
        ] {
            assert_eq!(
                command_args(cmd, &empty)
                    .iter()
                    .map(|a| a.value.as_str())
                    .collect::<Vec<_>>(),
                expected,
                "{cmd}'s arguments"
            );
        }
    }

    /// `/close` offers this topic's own subagents — the only names it accepts —
    /// then `all`, then the bare form that closes the topic itself.
    #[test]
    fn close_lists_subagents_all_then_the_self_row() {
        let cfg = AppConfig::default();
        let subagents = vec!["ci-watch-917".to_string(), "review-send".to_string()];
        let args = command_args("/close", &ctx_with_names(&cfg, &[], &subagents));

        assert_eq!(
            args.iter().map(|a| a.value.as_str()).collect::<Vec<_>>(),
            [
                "ci-watch-917",
                "review-send",
                "all",
                " ",
                "--force",
                "--purge"
            ]
        );
        assert_eq!(
            find(&args, "ci-watch-917").unwrap().args.len(),
            2,
            "a subagent opens the same flag level"
        );
        assert!(
            find(&args, " ").unwrap().args.is_empty(),
            "the self row is a space: it completes to `/close  `"
        );
    }

    /// With no subagents there is nothing to name and no `all` to offer, but
    /// the self row stays.
    #[test]
    fn close_without_subagents_still_offers_the_self_row() {
        let cfg = AppConfig::default();
        let args = command_args("/close", &ctx(&cfg, &[], &[]));
        assert_eq!(
            args.iter().map(|a| a.value.as_str()).collect::<Vec<_>>(),
            [" ", "--force", "--purge"]
        );
    }

    /// `/send` offers every agent — a name it never checks — and then the flags
    /// that have to come before the message.
    #[test]
    fn send_lists_agents_then_flags_with_a_flag_level_under_every_agent() {
        let cfg = AppConfig::default();
        let agents = vec!["invoice".to_string(), "mail-bot".to_string()];
        let args = command_args("/send", &ctx_with_names(&cfg, &agents, &[]));

        assert_eq!(
            args.iter().map(|a| a.value.as_str()).collect::<Vec<_>>(),
            [
                "invoice",
                "mail-bot",
                "--topic",
                "--reply",
                "--attach",
                "--recipient"
            ],
            "agents first, then the flags that precede the message"
        );
        assert_eq!(find(&args, "invoice").unwrap().args.len(), 4);
    }

    /// `/spawn` offers the types it accepts and then its flags, and every type
    /// opens the same flag level — the type comes first because it is the only
    /// positional, not because the flags are unavailable beside it.
    #[test]
    fn spawn_lists_types_then_flags_with_a_flag_level_under_every_type() {
        let cfg = AppConfig::default();
        let types = vec![
            (
                "review".to_string(),
                "Reviews a change before you push (shares your directory)".to_string(),
            ),
            (
                "analyst".to_string(),
                "Reads a repo and reports".to_string(),
            ),
        ];
        let args = command_args("/spawn", &ctx_with_types(&cfg, &types));

        assert_eq!(
            args.iter().map(|a| a.value.as_str()).collect::<Vec<_>>(),
            [
                "review",
                "analyst",
                "--inherit",
                "--share",
                "--name",
                "--dir"
            ],
            "types first, then the flags"
        );
        assert_eq!(
            find(&args, "review").unwrap().description,
            "Reviews a change before you push (shares your directory)",
            "a role's declared default rides in its own line"
        );
        assert_eq!(
            find(&args, "review")
                .unwrap()
                .args
                .iter()
                .map(|a| a.value.as_str())
                .collect::<Vec<_>>(),
            ["--inherit", "--share", "--name", "--dir"]
        );
        assert!(find(&args, "analyst").unwrap().args.len() == 4);
        assert!(
            find(&args, "--share").unwrap().args.is_empty(),
            "a flag is a leaf"
        );
    }

    /// No types on this machine is not a row-less level: the flags are still
    /// there, so `/spawn --name x` stays completable.
    #[test]
    fn spawn_offers_its_flags_with_no_types_installed() {
        let cfg = AppConfig::default();
        let args = command_args("/spawn", &ctx(&cfg, &[], &[]));
        assert_eq!(
            args.iter().map(|a| a.value.as_str()).collect::<Vec<_>>(),
            ["--inherit", "--share", "--name", "--dir"]
        );
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
