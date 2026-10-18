//! The single source of truth for jyc's built-in slash commands.
//!
//! Every fact about a built-in that a *consumer* needs — the name it dispatches
//! on and the description the `/` popup and `/?` show — is written exactly
//! *once*, in the [`builtin_commands!`] table below. The macro expands that
//! table into the four things consumers need:
//!
//! - [`BUILTIN_COMMANDS`] — name + description, readable without any handler
//!   instance (the inspect server composes the popup long before an agent or
//!   topic worker exists)
//! - [`BUILTIN_COMMAND_NAMES`] — names only, passed into
//!   `jyc_types::validation`'s "don't shadow a built-in" rule, so `jyc-types`
//!   holds no command data of its own
//! - [`builtin_registry`] — what the topic worker registers
//! - [`builtin_infos`] — the built-in rows of `all_commands_with`
//!
//! Behaviour stays with the handler (its `execute`, its
//! `collect_subsequent_lines`). The one piece of routing data a channel needs
//! before it dispatches — whether the message still reaches the agent — is a
//! table field: the feishu channel reads it off the command list, the CLI's
//! mail bridge reads the post-dispatch answer from
//! [`registry::CommandOutput`](super::registry::CommandOutput), and
//! `test_only_backlog_continues_to_agent_in_builtins` pins the value.
//!
//! Adding a command in code is therefore: one row here, plus the handler. There
//! is no second list to remember, which is the whole point — `/fork` was
//! registered and shipped working while silently missing from the popup
//! (#814), because the registry and the display list lived in different files
//! and nothing but a human kept them together. (`/fork` itself is gone —
//! `/spawn -s` is what it did — but the reason this table exists outlived it.)
//!
//! [`builtin_registry`]: builtin_registry
//! [`builtin_infos`]: builtin_infos

use std::sync::Arc;

use jyc_types::CommandInfo;

use super::backlog_handler::BacklogCommandHandler;
use super::bill_handler::BillCommandHandler;
use super::cancel_handler::CancelCommandHandler;
use super::close_handler::CloseCommandHandler;
use super::context_handler::ContextCommandHandler;
use super::exchange_handler::ExchangeCommandHandler;
use super::flags::FlagSpec;
use super::grant_handler::{GrantCommandHandler, UngrantCommandHandler};
use super::help_handler::HelpCommandHandler;
use super::info_handler::InfoCommandHandler;
use super::mode_handler::{BuildCommandHandler, PlanCommandHandler};
use super::model_handler::ModelCommandHandler;
use super::new_handler::NewCommandHandler;
use super::pin_handler::PinCommandHandler;
use super::registry::CommandRegistry;
use super::reset_handler::ResetCommandHandler;
use super::send_handler::SendCommandHandler;
use super::spawn_handler::SpawnCommandHandler;
use super::spec::{self, ArgSource, Shape, Syntax};
use super::template_handler::TemplateCommandHandler;
use super::thinking_handler::ThinkingCommandHandler;
use super::toggle_handler::ToggleCommandHandler;
use super::unpin_handler::UnpinCommandHandler;
use crate::topic_manager::TopicManager;

/// One built-in command: what it is called, what the popup says about it, and
/// whether a message carrying it still reaches the agent.
pub struct CommandSpec {
    /// The dispatch key, exactly as the registry stores it (leading slash).
    pub name: &'static str,
    /// One-line help, shown in the `/` popup and by `/?`.
    pub description: &'static str,
    /// Whether a message carrying this command still reaches the agent.
    ///
    /// Read by whoever needs the answer *before* dispatch — the feishu channel
    /// looks it up in the command list to keep its progress card alive — while
    /// [`registry::CommandOutput`] carries the post-dispatch truth. Command-level
    /// rather than per-subcommand because a pre-dispatch reader cannot see args:
    /// `/backlog push` answers instantly but `/backlog pop` injects, and the
    /// former just flashes the card (#639).
    ///
    /// [`registry::CommandOutput`]: super::registry::CommandOutput
    pub continues_to_agent: bool,
    /// How the command's arguments are shaped: what [`spec::parse`] reads the
    /// line through, and what the popup's argument level and [`Self::usage`]
    /// are built from. Written here, on the command's one row.
    pub shape: Shape,
    /// The values this command's first argument level offers — keys,
    /// descriptions and whatever they open, declared once on the row that also
    /// declares its shape. Omitted means the command offers none and the popup
    /// closes after it.
    pub args: ArgSource,
    /// Usage-line words the popup cannot offer (patterns like `YYYY-MM`,
    /// values the popup replaces with better choices like `today`). Only for
    /// a command whose choices are not all declared in `args`; a word that
    /// names a value `args` declares is a second copy, and
    /// `usage_words_never_repeat_a_declared_value` refuses it.
    pub usage_words: &'static [&'static str],
    /// The command's flag declarations, without their enum keys — the
    /// projection its own handler reads its typed set through
    /// ([`super::flags::specs_of`]).
    ///
    /// A function rather than a slice because the table is a const of one type
    /// and each command's flags are keyed by its own enum; the spellings still
    /// live in exactly one place, beside the enum that handler matches on.
    pub flags: fn() -> Vec<&'static FlagSpec>,
}

impl CommandSpec {
    /// This row's argument syntax: shape, offered values, usage-only words.
    pub fn syntax(&self) -> Syntax {
        Syntax {
            shape: self.shape,
            args: self.args,
            usage_words: self.usage_words,
        }
    }

    /// The command's usage line, derived from its syntax and its flags.
    ///
    /// One writer for `/close [subagent|all] [--force] [--purge]`: the popup,
    /// `/?` and the command's own refusals all read this, so a line cannot
    /// advertise what the parser refuses.
    pub fn usage(&self) -> String {
        spec::usage(self.name, self.syntax(), &(self.flags)())
    }
}

/// A row's `args` — omitted means the command offers no values, so the rows that
/// offer none do not each say so.
macro_rules! args_or_empty {
    () => {
        ArgSource::Declared(&[])
    };
    ($args:expr) => {
        $args
    };
}

/// A row's `usage_words` — omitted means the usage line's value set is exactly
/// the popup's.
macro_rules! usage_words_or_empty {
    () => {
        &[] as &'static [&'static str]
    };
    ($words:expr) => {
        $words
    };
}

/// `continues_to_agent` for a table row — omitted means the common case, so 21
/// rows do not repeat a `false`. Kept module-local (no `#[macro_export]`):
/// it is only ever invoked from `builtin_commands!` below, in this module.
macro_rules! continues_or_false {
    () => {
        false
    };
    ($v:expr) => {
        $v
    };
}

/// A row's `shape` — omitted means the command takes no arguments, so the rows
/// that take none do not each say so.
macro_rules! shape_or_none {
    () => {
        $crate::command::spec::Shape::None
    };
    ($shape:expr) => {
        $shape
    };
}

/// A row's `flags` — omitted means the command takes none, and the popup level
/// and usage line read an empty projection.
macro_rules! flags_or_none {
    () => {
        no_flags
    };
    ($flags:expr) => {
        $flags
    };
}

/// The empty projection [`flags_or_none!`] defaults to.
fn no_flags() -> Vec<&'static FlagSpec> {
    Vec::new()
}

/// The syntax `/skill` and `/mcp` share: a subcommand and the thing it acts on.
///
/// One declaration for the shape, since a toggle reads the same line whichever
/// list it toggles — the difference is which names the popup offers, and that
/// comes from the topic's own skills and MCP servers, so each row declares its
/// own [`CommandSpec::args`].
const fn toggle_shape() -> Shape {
    Shape::Subcommand {
        labels: &["name"],
        rest: None,
    }
}

/// The syntax `name`'s row declares.
///
/// A handler reads its command line through the very declaration its popup level
/// and usage line come from, so it cannot accept a line its own row does not
/// advertise.
///
/// A name with no row is a caller's mistake, not a command that takes nothing —
/// a parameterized handler must hold the syntax of what it serves rather than
/// look its own name up (a wrong name here would otherwise read every argument
/// as one too many, silently and only at runtime).
pub fn syntax_of(name: &str) -> Syntax {
    let row = BUILTIN_COMMANDS.iter().find(|spec| spec.name == name);
    debug_assert!(row.is_some(), "no built-in row for {name}");
    row.map(|spec| spec.syntax()).unwrap_or(Syntax::EMPTY)
}

/// The first argument level `name`'s popup offers, or nothing for a command
/// with no built-in row (a config-declared custom command, whose arguments stay
/// free text).
pub fn args_of(name: &str) -> ArgSource {
    BUILTIN_COMMANDS
        .iter()
        .find(|spec| spec.name == name)
        .map(|spec| spec.args)
        .unwrap_or(ArgSource::Declared(&[]))
}

/// The usage line `name`'s row declares.
///
/// This is what a handler prints in its own refusals, so the syntax it demands is
/// the syntax its row advertises.
pub fn usage_of(name: &str) -> String {
    BUILTIN_COMMANDS
        .iter()
        .find(|spec| spec.name == name)
        .map(CommandSpec::usage)
        .unwrap_or_else(|| name.to_string())
}

/// Declare the built-in commands: the spec table, the name list, and the
/// registry, all from one list.
///
/// Each row's `make` is a closure receiving an owned `Arc<TopicManager>`; rows
/// that don't need it bind `_`. It has to be a closure because the row bodies
/// are written in *this* file's scope and macro hygiene stops them naming the
/// generated function's parameter. Handlers are built with `Arc::new(..)`
/// rather than `Box::new(..)` so the closure's return type needs no coercion —
/// inside a closure body inference cannot see `register`'s parameter type,
/// which is what forced an explicit `fn(&Arc<TopicManager>) -> Box<dyn ..>`
/// binding here before the registry stored `Arc` too.
macro_rules! builtin_commands {
    ( $( $name:literal => {
            desc: $desc:literal,
            $( continues: $cont:expr, )?
            $( shape: $shape:expr, )?
            $( args: $args:expr, )?
            $( usage_words: $words:expr, )?
            $( flags: $flags:expr, )?
            make: $make:expr
        },
    )* ) => {
        /// The table. Presentation order: anything that walks it unsorted keeps
        /// this order; the popup sorts by name itself.
        pub const BUILTIN_COMMANDS: &[CommandSpec] = &[ $(
            CommandSpec {
                name: $name,
                description: $desc,
                continues_to_agent: continues_or_false!( $( $cont )? ),
                shape: shape_or_none!( $( $shape )? ),
                args: args_or_empty!( $( $args )? ),
                usage_words: usage_words_or_empty!( $( $words )? ),
                flags: flags_or_none!( $( $flags )? ),
            },
        )* ];

        /// Names only — see the module docs.
        pub const BUILTIN_COMMAND_NAMES: &[&str] = &[ $($name),* ];

        /// Every built-in handler, registered under its table name.
        pub fn builtin_registry(topic_manager: Arc<TopicManager>) -> CommandRegistry {
            let mut registry = CommandRegistry::new();
            $(
                registry.register($name, ($make)(topic_manager.clone()));
            )*
            registry
        }
    };
}

/// The built-in rows of the command list the popup and `/?` show.
pub fn builtin_infos() -> Vec<CommandInfo> {
    BUILTIN_COMMANDS
        .iter()
        .map(|s| CommandInfo {
            name: s.name.to_string(),
            description: s.description.to_string(),
            usage: s.usage(),
            continues_to_agent: s.continues_to_agent,
            ..Default::default()
        })
        .collect()
}

builtin_commands! {
    "/?" => {
        desc: "Show available commands",
        make: |_| Arc::new(HelpCommandHandler)
    },
    "/backlog" => {
        desc: "Save and replay user messages",
        continues: true,
        shape: Shape::Subcommand {
            labels: &[],
            rest: Some("text"),
        },
        args: super::args::BACKLOG_ARGS,
        make: |_| Arc::new(BacklogCommandHandler::new())
    },
    "/bill" => {
        desc: "Usage/cost across topics",
        shape: Shape::Subcommand {
            labels: &[],
            rest: None,
        },
        args: super::args::BILL_ARGS,
        usage_words: &["today", "YYYY-MM"],
        make: |_| Arc::new(BillCommandHandler)
    },
    "/build" => {
        desc: "Switch to build mode (full execution)",
        make: |_| Arc::new(BuildCommandHandler)
    },
    "/cancel" => {
        desc: "Cancel current AI processing",
        make: |tm| Arc::new(CancelCommandHandler::new(tm))
    },
    "/close" => {
        desc: "Close this topic, or one of its subagents (it closes itself)",
        shape: Shape::Args { labels: &["subagent|all"] },
        args: super::args::CLOSE_ARGS,
        flags: super::flags::close_specs,
        make: |tm| Arc::new(CloseCommandHandler::new(tm))
    },
    "/context" => {
        desc: "View or change the context strategy, or dump the wire payload",
        shape: Shape::Subcommand {
            labels: &[],
            rest: Some("args"),
        },
        args: super::args::CONTEXT_ARGS,
        make: |_| Arc::new(ContextCommandHandler)
    },
    "/exchange" => {
        desc: "Show shareable URLs for this topic's published files",
        shape: Shape::Args { labels: &["file"] },
        make: |_| Arc::new(ExchangeCommandHandler)
    },
    "/grant" => {
        desc: "Grant agent access to a path (read-only, until restart)",
        shape: Shape::Args { labels: &["path"] },
        flags: super::flags::grant_specs,
        make: |_| Arc::new(GrantCommandHandler)
    },
    "/info" => {
        desc: "Show topic info (mode, model, tokens, cost, tasks, files)",
        make: |tm| Arc::new(InfoCommandHandler::new(tm))
    },
    "/mcp" => {
        desc: "Toggle an MCP server for this topic",
        shape: toggle_shape(),
        args: super::args::MCP_ARGS,
        make: |_| Arc::new(ToggleCommandHandler::mcp())
    },
    "/model" => {
        desc: "Switch AI model for this topic",
        shape: Shape::Args { labels: &["model|reset"] },
        args: super::args::MODEL_ARGS,
        make: |_| Arc::new(ModelCommandHandler)
    },
    "/new" => {
        desc: "Reset session and clear chat history",
        args: super::args::CONFIRM_FLAG_ARGS,
        flags: super::flags::confirm_specs,
        make: |_| Arc::new(NewCommandHandler)
    },
    "/pin" => {
        desc: "Pin this ad-hoc websocket topic to config.toml",
        make: |tm| Arc::new(PinCommandHandler::new(tm))
    },
    "/plan" => {
        desc: "Switch to plan mode (read-only)",
        make: |_| Arc::new(PlanCommandHandler)
    },
    "/reset" => {
        desc: "Reset session, keep chat history",
        args: super::args::CONFIRM_FLAG_ARGS,
        flags: super::flags::confirm_specs,
        make: |_| Arc::new(ResetCommandHandler)
    },
    "/send" => {
        desc: "Send a message to any agent, or to one of its topics",
        shape: Shape::ThenBody {
            label: "agent",
            body: "message",
        },
        args: super::args::SEND_ARGS,
        flags: super::flags::send_specs,
        make: |tm| Arc::new(SendCommandHandler::new(tm))
    },
    "/skill" => {
        desc: "Toggle a skill for this topic",
        shape: toggle_shape(),
        args: super::args::SKILL_ARGS,
        make: |_| Arc::new(ToggleCommandHandler::skill())
    },
    "/spawn" => {
        desc: "Start a sibling topic — a new dir by default, this one with -s",
        shape: Shape::Args { labels: &["type"] },
        args: super::args::SPAWN_ARGS,
        flags: super::flags::spawn_specs,
        make: |tm| Arc::new(SpawnCommandHandler::new(tm))
    },
    "/template" => {
        desc: "Apply or re-apply this topic's template",
        shape: Shape::Args { labels: &["name|update"] },
        args: super::args::TEMPLATE_ARGS,
        make: |_| Arc::new(TemplateCommandHandler)
    },
    "/thinking" => {
        desc: "Show or hide AI thinking/reasoning content",
        shape: Shape::Subcommand {
            labels: &[],
            rest: None,
        },
        args: super::args::THINKING_ARGS,
        make: |_| Arc::new(ThinkingCommandHandler)
    },
    "/ungrant" => {
        desc: "Revoke a runtime access grant",
        shape: Shape::Args { labels: &["path"] },
        make: |_| Arc::new(UngrantCommandHandler)
    },
    "/unpin" => {
        desc: "Remove pinned topic configuration from config.toml",
        make: |tm| Arc::new(UnpinCommandHandler::new(tm))
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariants that used to be checked by keeping a second list in sync
    /// are now checked against the one list.
    #[test]
    fn names_are_unique_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for spec in BUILTIN_COMMANDS {
            assert!(
                spec.name.starts_with('/') && spec.name.len() > 1,
                "bad command name {:?}",
                spec.name
            );
            assert_eq!(
                spec.name,
                spec.name.to_lowercase(),
                "the registry lowercases the incoming line, so an uppercase \
                 name could never dispatch: {:?}",
                spec.name
            );
            assert!(
                !spec.name.contains(char::is_whitespace),
                "{:?} would never match a token",
                spec.name
            );
            assert!(
                seen.insert(spec.name),
                "duplicate command name {:?}",
                spec.name
            );
            assert!(
                !spec.description.is_empty(),
                "{} needs a description for the popup",
                spec.name
            );
        }
        assert_eq!(BUILTIN_COMMAND_NAMES.len(), BUILTIN_COMMANDS.len());
    }

    /// A command that shipped registered-but-hidden (#814, `/fork`) is the
    /// reason this file exists. Both come from one table now, so what is worth
    /// asserting is that the table really does reach the popup rows.
    #[test]
    fn every_row_becomes_a_popup_entry() {
        let infos = builtin_infos();
        assert_eq!(infos.len(), BUILTIN_COMMANDS.len());
        for spec in BUILTIN_COMMANDS {
            let info = infos
                .iter()
                .find(|i| i.name == spec.name)
                .unwrap_or_else(|| panic!("{} missing from the popup rows", spec.name));
            assert_eq!(info.description, spec.description);
            assert_eq!(info.continues_to_agent, spec.continues_to_agent);
        }
        assert!(infos.iter().any(|i| i.name == "/spawn"));
    }
}
