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
//! and nothing but a human kept them together.
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
use super::fork_handler::ForkCommandHandler;
use super::grant_handler::{GrantCommandHandler, UngrantCommandHandler};
use super::help_handler::HelpCommandHandler;
use super::info_handler::InfoCommandHandler;
use super::mode_handler::{BuildCommandHandler, PlanCommandHandler};
use super::model_handler::ModelCommandHandler;
use super::new_handler::NewCommandHandler;
use super::pin_handler::PinCommandHandler;
use super::registry::CommandRegistry;
use super::reset_handler::ResetCommandHandler;
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
        desc: "Save and replay user messages (push|list|get|pop|rm|set)",
        continues: true,
        make: |_| Arc::new(BacklogCommandHandler::new())
    },
    "/bill" => {
        desc: "Usage/cost across topics (today | YYYY-MM | all)",
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
        desc: "Close and delete this topic (requires --force)",
        make: |tm| Arc::new(CloseCommandHandler::new(tm))
    },
    "/context" => {
        desc: "View or change the context strategy / debug-dump wire payload",
        make: |_| Arc::new(ContextCommandHandler)
    },
    "/exchange" => {
        desc: "Show shareable URLs for this topic's published files",
        make: |tm| Arc::new(ExchangeCommandHandler::new(tm))
    },
    "/fork" => {
        desc: "Branch a sibling topic off this one, keeping its context, history and settings",
        make: |tm| Arc::new(ForkCommandHandler::new(tm))
    },
    "/grant" => {
        desc: "Grant agent access to a path (read-only, until restart; -w write, -p persist)",
        make: |_| Arc::new(GrantCommandHandler)
    },
    "/info" => {
        desc: "Show topic info (mode, model, tokens, cost, tasks, files)",
        make: |tm| Arc::new(InfoCommandHandler::new(tm))
    },
    "/mcp" => {
        desc: "Toggle an MCP server for this topic: /mcp on|off|reset <name>",
        make: |_| Arc::new(ToggleCommandHandler::mcp())
    },
    "/model" => {
        desc: "Switch AI model for this topic",
        make: |_| Arc::new(ModelCommandHandler)
    },
    "/new" => {
        desc: "Reset session and clear chat history (requires --force)",
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
        desc: "Reset session, keep chat history (requires --force)",
        make: |_| Arc::new(ResetCommandHandler)
    },
    "/skill" => {
        desc: "Toggle a skill for this topic: /skill on|off|reset <name>",
        make: |_| Arc::new(ToggleCommandHandler::skill())
    },
    "/template" => {
        desc: "Apply or re-apply topic template",
        make: |_| Arc::new(TemplateCommandHandler)
    },
    "/thinking" => {
        desc: "Show or hide AI thinking/reasoning content",
        make: |_| Arc::new(ThinkingCommandHandler)
    },
    "/ungrant" => {
        desc: "Revoke a runtime access grant",
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

    /// `/fork` is the reason this file exists: it was registered but absent from
    /// the list the popup reads (#814). Both come from one table now, so what is
    /// worth asserting is that the table really does reach the popup rows.
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
        assert!(infos.iter().any(|i| i.name == "/fork"));
    }
}
