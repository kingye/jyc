//! Enumerable argument values per command — the data behind the TUI's
//! multi-level `/` popup.
//!
//! The inspect server folds this table into `CommandInfo::args` when it
//! builds `TopicInfo.commands`/`TopicSummary.commands`, so the popup can
//! offer every command's next level without knowing anything about the
//! commands itself. `/model` is just one row of this table, not a special
//! case in the client.
//!
//! Every value a command offers is written here, once, on a table its row in
//! [`super::builtin`] points at: the key, the description the popup shows, and
//! what it opens. Nothing below repeats a key the usage line or another
//! command already names — `/context`'s `<sliding|full|reset|dump>` and the
//! popup rows are the same declaration read twice, by two readers.
//!
//! Values come from three places: the command's own contract (the literals
//! each handler `match`es on), per-topic state the caller already has
//! (models, skills, MCP names), and the instance's own subagent types
//! (`/spawn`'s first level). A command whose arguments are free text
//! (`/grant <path>`, `/backlog push <text>`) gets an empty vec — the popup
//! then closes and the input field sends what was typed.
//!
//! A flag is not one of these values: its row is built from [`super::flags`],
//! the same declaration the handler reads the command line through, so the
//! popup cannot offer a spelling the parser does not accept.
//!
//! Deliberately absent: `/exchange` (published names need a directory scan
//! per topic per poll, where `/spawn`'s types are resolved once for the whole
//! payload) and `/ungrant` (grants live in the agent runtime, not in the
//! inspect payload). Both stay free text; add them when the popup gets a lazy
//! per-topic command fetch (see the `TODO(perf)` in the overview builder).

use std::collections::HashSet;

use chrono::{Datelike, Utc};
use jyc_types::{AppConfig, CommandArg, McpServerConfig, ModelInfo};

use super::flags;
use super::spec::{ArgDef, ArgSource};

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

/// The values a command's first argument level offers, resolved against
/// per-topic state.
///
/// A command with no built-in row — a config-declared custom command — offers
/// nothing, so its arguments stay free text.
pub fn command_args(command: &str, ctx: &ArgCtx) -> Vec<CommandArg> {
    super::builtin::args_of(command).resolve(ctx)
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

/// `/context`: the strategies, plus the one toggle. Sliding comes first because
/// it is what a long topic wants; the order here *is* the order the popup offers
/// and the order the usage line lists, which is why there is only one list.
pub static CONTEXT_ARGS: ArgSource = ArgSource::Declared(&[
    // The handler also answers to `sliding_window`; the table offers one
    // spelling, and typing the other simply closes the popup.
    ArgDef::leaf("sliding", "Sliding window; optionally follow a window size"),
    ArgDef::leaf("full", "Keep the whole prior conversation verbatim"),
    ArgDef::leaf("reset", "Drop the runtime override"),
    ArgDef::opening(
        "dump",
        "Toggle wire-payload dumping",
        &[ArgDef::leaf("on", ""), ArgDef::leaf("off", "")],
    ),
]);

/// `/thinking`: show or hide the reasoning content.
pub static THINKING_ARGS: ArgSource = ArgSource::Declared(&[
    ArgDef::leaf("show", "Show reasoning content"),
    ArgDef::leaf("hide", "Hide reasoning content"),
]);

/// `/backlog`'s verbs. Their arguments are free text (`/backlog push <text>`),
/// so a bare `/backlog` closes the popup and sends the line as typed.
pub static BACKLOG_ARGS: ArgSource = ArgSource::Declared(&[
    ArgDef::leaf("push", "Add an item"),
    ArgDef::leaf("list", "Show all items"),
    ArgDef::leaf("ls", "Show all items"),
    ArgDef::leaf("get", "Show one item by index"),
    ArgDef::leaf("pop", "Remove and show the next item"),
    ArgDef::leaf("rm", "Remove an item by index"),
    ArgDef::leaf("set", "Replace an item"),
]);

/// `/bill`: this month's scopes and then every recorded day. `today` and
/// `YYYY-MM` are what the usage line names instead — the popup offers the
/// months themselves, which is the better answer to the same question.
pub static BILL_ARGS: ArgSource = ArgSource::Mixed(
    &[],
    recent_months,
    &[ArgDef::leaf("all", "Every day with recorded usage")],
);

/// `/model`: every model the configured providers expose, then the way back to
/// the configured one.
pub static MODEL_ARGS: ArgSource = ArgSource::Mixed(
    &[],
    model_names,
    &[ArgDef::leaf(
        "reset",
        "Clear the override, use the configured model",
    )],
);

/// `/skill`: one toggle shell, with this topic's own skills behind `on`/`off`.
/// `/mcp` is the same shell over MCP server names — hence the shared
/// [`toggle_args`] builder rather than two hand-written lists.
pub static SKILL_ARGS: ArgSource = ArgSource::Declared(&toggle_args(skill_names));
pub static MCP_ARGS: ArgSource = ArgSource::Declared(&toggle_args(mcp_name_args));

/// `/template`: the only verb it offers; a template name is free text.
pub static TEMPLATE_ARGS: ArgSource = ArgSource::Declared(&[ArgDef::leaf(
    "update",
    "Re-apply the template, overwriting local edits",
)]);

/// `/new` and `/reset`: the confirm flags, projected from the one declaration
/// the parser reads them through.
pub static CONFIRM_FLAG_ARGS: ArgSource = ArgSource::Provided(confirm_flag_args);

/// `/spawn`, `/send` and `/close`: every key comes from per-topic state, so the
/// level is provided whole and the usage line names the slot from the shape.
pub static SPAWN_ARGS: ArgSource = ArgSource::Provided(spawn_args);
pub static SEND_ARGS: ArgSource = ArgSource::Provided(send_args);
pub static CLOSE_ARGS: ArgSource = ArgSource::Provided(close_args);

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
        .map(|(name, description)| val_with(name, description, flags::rows(flags::SPAWN_FLAGS)))
        .collect();
    args.extend(flags::rows(flags::SPAWN_FLAGS));
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
        .map(|name| val_with(name, "Send to this agent", flags::rows(flags::SEND_FLAGS)))
        .collect();
    args.extend(flags::rows(flags::SEND_FLAGS));
    args
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
                flags::rows(flags::CLOSE_FLAGS),
            )
        })
        .collect();
    if !ctx.subagents.is_empty() {
        // `all` stands in for the names above it, so it opens the same flag
        // level they do: `/close all --force` is what the row completes to.
        args.push(val_with(
            "all",
            "Close every direct subagent",
            flags::rows(flags::CLOSE_FLAGS),
        ));
    }
    // A bare `/close` closes this topic, and the popup needs a row for exactly
    // that: a space completes to `/close  `, which carries no argument.
    args.push(val(" ", "Close this topic (self)"));
    args.extend(flags::rows(flags::CLOSE_FLAGS));
    args
}

/// `<cmd> on|off|reset <name>` — `reset` clears everything at once, so only
/// `on` and `off` carry the name list.
///
/// One shell for `/skill` and `/mcp`: a toggle reads the same line whichever
/// list it toggles, so the words are written here once and each row supplies
/// its own names.
const fn toggle_args(names: fn(&ArgCtx) -> Vec<CommandArg>) -> [ArgDef; 3] {
    // `on`/`off` carry the names; `reset` clears everything at once. Both
    // toggles say the same thing about the same line, so the words are written
    // here once — the noun each row toggles does not change what `on` does.
    [
        ArgDef::dynamic("on", "Force this name on for the topic", names),
        ArgDef::dynamic("off", "Force this name off for the topic", names),
        ArgDef::leaf("reset", "Clear all overrides for this topic"),
    ]
}

/// The names behind a toggle's `on`/`off`.
fn skill_names(ctx: &ArgCtx) -> Vec<CommandArg> {
    ctx.skills.iter().map(|name| val(name, "")).collect()
}

fn mcp_name_args(ctx: &ArgCtx) -> Vec<CommandArg> {
    mcp_names(ctx.config)
        .iter()
        .map(|name| val(name, ""))
        .collect()
}

fn model_names(ctx: &ArgCtx) -> Vec<CommandArg> {
    ctx.models.iter().map(|m| val(&m.name, "")).collect()
}

/// `/new`'s and `/reset`'s confirm flags.
fn confirm_flag_args(_ctx: &ArgCtx) -> Vec<CommandArg> {
    flags::rows(flags::CONFIRM_FLAGS)
}

/// The `YYYY-MM` scopes of this month and the two before it, newest first.
fn recent_months(_ctx: &ArgCtx) -> Vec<CommandArg> {
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

    /// A flag's short spelling rides in its own popup line, straight off the
    /// declaration the handler reads it through — `/close -f` is offered even
    /// though the popup inserts the long one.
    #[test]
    fn a_flag_row_names_its_short_spelling() {
        let cfg = AppConfig::default();
        let args = command_args("/close", &ctx(&cfg, &[], &[]));
        assert_eq!(
            find(&args, "--force").unwrap().description,
            "Skip the confirmation guard (-f)"
        );
        assert_eq!(
            find(&args, "--purge").unwrap().description,
            "Delete the topic dir too, not just its state (-p)"
        );
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
        assert_eq!(
            find(&args, "all")
                .unwrap()
                .args
                .iter()
                .map(|a| a.value.as_str())
                .collect::<Vec<_>>(),
            ["--force", "--purge"],
            "`all` names the same subagents, so it takes the same flags"
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
        let args = recent_months(&ctx(&AppConfig::default(), &[], &[]));
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

    /// The usage line's value set and the popup's are one declaration read
    /// twice, so they cannot disagree — except where a row declares words the
    /// popup has no row for (`/bill`'s `today`, `YYYY-MM`). Those words must not
    /// also name a value the popup offers: that would be the second copy this
    /// module exists to delete.
    ///
    /// Compared against everything the popup *offers*, nested levels and
    /// dynamic runs included: a level two deep is still a value the popup shows.
    #[test]
    fn usage_words_never_repeat_a_declared_value() {
        let cfg = AppConfig::default();
        let skills = ["ponytail".to_string()];
        // A topic with state, so the dynamic runs really produce values: a
        // usage word must not collide with what only the topic can supply
        // either (`/model`'s model list is resolved, not assumed empty).
        let models = [ModelInfo {
            name: "deepseek/deepseek-chat".into(),
        }];
        let fixture = ctx(&cfg, &skills, &models);
        let mut checked = 0;
        for spec in crate::command::builtin::BUILTIN_COMMANDS {
            let offered = collect_values(&command_args(spec.name, &fixture));
            for word in spec.usage_words {
                assert!(
                    !offered.iter().any(|value| value == word),
                    "{}: usage word `{word}` is already a value the popup offers \
                     ({offered:?}) — drop the word and let the declaration speak",
                    spec.name
                );
            }
            checked += spec.usage_words.len();
        }
        assert!(checked > 0, "no row declares a usage-only word");
    }

    /// Every word of a label, however it spells its choices.
    fn label_words(label: &str) -> Vec<&str> {
        label.split('|').collect()
    }

    /// A `Shape::Args` label names what the row cannot see. A value the row
    /// declares is spelled by the declaration itself — the usage line reads it
    /// from `args` — so a label that repeats one is a second copy of it, and
    /// the drift this guard exists to prevent (`[model]` beside a vanished
    /// `reset`) is exactly what a label-only reader would never notice.
    #[test]
    fn args_labels_never_repeat_a_declared_value() {
        use crate::command::spec::Shape;
        let mut checked = 0;
        for spec in crate::command::builtin::BUILTIN_COMMANDS {
            let Shape::Args { labels } = spec.shape else {
                continue;
            };
            let declared: Vec<&str> = spec
                .args
                .declared_keys()
                .iter()
                .map(|arg| arg.value)
                .collect();
            if declared.is_empty() {
                continue;
            }
            // The line folds the declared values into the slot the labels name,
            // so a second label would fold two slots into one group.
            assert_eq!(
                labels.len(),
                1,
                "{}: `args_groups` folds the declared values into one slot, but \
                 the shape declares {}",
                spec.name,
                labels.len()
            );
            for word in labels.iter().flat_map(|label| label_words(label)) {
                assert!(
                    !declared.contains(&word),
                    "{}: label spells `{word}`, which the row already declares in \
                     `args` — drop it from the label",
                    spec.name
                );
            }
            checked += 1;
        }
        assert!(
            checked > 0,
            "no row pairs a fixed shape with declared values"
        );
    }

    /// Every value the popup offers, at every level, flattened.
    fn collect_values(rows: &[CommandArg]) -> Vec<String> {
        rows.iter()
            .flat_map(|row| {
                let mut values = vec![row.value.clone()];
                values.extend(collect_values(&row.args));
                values
            })
            .collect()
    }

    /// Every value a row declares reaches the popup, in the declared order —
    /// the one thing the resolver could still get wrong, since it stitches
    /// declared and dynamic rows together.
    #[test]
    fn every_declared_value_reaches_the_popup_in_order() {
        let cfg = AppConfig::default();
        let skills = ["ponytail".to_string()];
        let ctx = ctx(&cfg, &skills, &[]);
        let mut checked = 0;
        for spec in crate::command::builtin::BUILTIN_COMMANDS {
            let declared = spec.args.declared_keys();
            if declared.is_empty() {
                continue;
            }
            let offered = command_args(spec.name, &ctx);
            let mut rest = offered.iter();
            for arg in declared {
                assert!(
                    rest.any(|row| row.value == arg.value),
                    "{}: `{}` never reaches the popup (offered: {:?})",
                    spec.name,
                    arg.value,
                    offered.iter().map(|r| &r.value).collect::<Vec<_>>()
                );
            }
            checked += 1;
        }
        assert!(checked > 0, "no row declares a value");
    }

    /// `/context` leads with `sliding`: a long topic is the case it exists for,
    /// and the order is one declaration, so the popup and the usage line put it
    /// first together.
    #[test]
    fn context_offers_sliding_first() {
        let cfg = AppConfig::default();
        let args = command_args("/context", &ctx(&cfg, &[], &[]));
        assert_eq!(args[0].value, "sliding");
        assert!(
            crate::command::builtin::usage_of("/context").starts_with("/context <sliding|"),
            "{}",
            crate::command::builtin::usage_of("/context")
        );
    }
}
