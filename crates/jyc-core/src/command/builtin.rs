//! The single source of truth for jyc's built-in slash commands.
//!
//! Every fact about a built-in — the name it dispatches on, the description
//! the `/` popup and `/?` show, whether the message continues into an agent
//! run, and how to construct its handler — is written exactly *once*, in the
//! [`builtin_commands!`] table below. The macro expands that table into the
//! four things consumers need:
//!
//! - [`BUILTIN_COMMANDS`] — the spec table, readable without any handler
//!   instance (the inspect server and config validation compose the popup
//!   before an agent or topic worker exists, so a static list is required)
//! - [`BUILTIN_COMMAND_NAMES`] — names only, passed into
//!   `jyc_types::validation`'s "don't shadow a built-in" rule, so `jyc-types`
//!   holds no command data of its own
//! - [`builtin_registry`] — what the topic worker registers
//! - [`builtin_infos`] — the built-in rows of `all_commands_with`
//!
//! Adding a command in code is therefore: one row here, plus the handler.
//! There is no second list to remember, which is the whole point — `/fork` was
//! registered and shipped working while silently missing from the popup
//! (#814), because the registry and the display list lived in different files
//! and nothing but a human kept them together. (README's command table is
//! prose, and `/?` remains the authoritative list.)
//!
//! [`BUILTIN_COMMANDS`]: BUILTIN_COMMANDS
//! [`BUILTIN_COMMAND_NAMES`]: BUILTIN_COMMAND_NAMES
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
use super::handler::CommandHandler;
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

/// One built-in command, as declared in the table below.
pub struct CommandSpec {
    /// The dispatch key, exactly as the registry stores it (leading slash).
    pub name: &'static str,
    /// One-line help, shown in the `/` popup and by `/?`.
    pub description: &'static str,
    /// Whether a message carrying this command still reaches the agent.
    ///
    /// Set at the command level, not per subcommand: the channel watcher
    /// spawns its progress indicator before dispatch and cannot see args.
    pub continues_to_agent: bool,
}

/// Declare the built-in commands: the spec table, the name list, and the
/// registry, all from one list.
///
/// Each row's `make` is a closure taking the topic manager (rows that don't
/// need it bind `_`) — the row bodies are written in *this* file's scope, so
/// they cannot name the generated function's parameter directly. The
/// expansion binds each closure to a typed `fn` local, which is what lets
/// `Box::new(..)` coerce to the trait object; inference through the macro alone
/// is not enough for that.
macro_rules! builtin_commands {
    (
        $(
            $name:literal => {
                desc: $desc:literal,
                continues: $cont:expr,
                make: $make:expr
            },
        )*
    ) => {
        /// The table. Presentation order: anything that walks it unsorted
        /// keeps this order; the popup sorts by name itself.
        pub const BUILTIN_COMMANDS: &[CommandSpec] = &[
            $(
                CommandSpec {
                    name: $name,
                    description: $desc,
                    continues_to_agent: $cont,
                },
            )*
        ];

        /// Names only — see the module docs.
        pub const BUILTIN_COMMAND_NAMES: &[&str] = &[ $($name),* ];

        /// Every built-in handler, registered under its table name.
        pub fn builtin_registry(topic_manager: &Arc<TopicManager>) -> CommandRegistry {
            let mut registry = CommandRegistry::new();
            $(
                let make: fn(&Arc<TopicManager>) -> Box<dyn CommandHandler> = $make;
                registry.register($name, make(topic_manager));
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
        continues: false,
        make: |_| Box::new(HelpCommandHandler)
    },
    "/backlog" => {
        // `pop` injects the popped text into the agent's next turn via
        // `append_body`, so it continues into an agent run and needs a
        // progress indicator on piped channels (feishu). The other
        // subcommands (`push`/`list`/`get`/`rm`/`set`) reply instantly — but
        // the flag is set at the command level, not per-subcommand, because
        // the channels.rs watcher spawns before dispatch and cannot inspect
        // the subcommand.
        desc: "Save and replay user messages (push|list|get|pop|rm|set)",
        continues: true,
        make: |_| Box::new(BacklogCommandHandler::new())
    },
    "/bill" => {
        desc: "Usage/cost across topics (today | YYYY-MM | all)",
        continues: false,
        make: |_| Box::new(BillCommandHandler)
    },
    "/build" => {
        desc: "Switch to build mode (full execution)",
        continues: false,
        make: |_| Box::new(BuildCommandHandler)
    },
    "/cancel" => {
        desc: "Cancel current AI processing",
        continues: false,
        make: |tm| Box::new(CancelCommandHandler::new(tm.clone()))
    },
    "/close" => {
        desc: "Close and delete this topic (requires --force)",
        continues: false,
        make: |tm| Box::new(CloseCommandHandler::new(tm.clone()))
    },
    "/context" => {
        desc: "View or change the context strategy / debug-dump wire payload",
        continues: false,
        make: |_| Box::new(ContextCommandHandler)
    },
    "/exchange" => {
        desc: "Show shareable URLs for this topic's published files",
        continues: false,
        make: |tm| Box::new(ExchangeCommandHandler::new(tm.clone()))
    },
    "/fork" => {
        desc: "Branch a sibling topic off this one, keeping its context, history and settings",
        continues: false,
        make: |tm| Box::new(ForkCommandHandler::new(tm.clone()))
    },
    "/grant" => {
        desc: "Grant agent access to a path (read-only, until restart; -w write, -p persist)",
        continues: false,
        make: |_| Box::new(GrantCommandHandler)
    },
    "/info" => {
        desc: "Show topic info (mode, model, tokens, cost, tasks, files)",
        continues: false,
        make: |tm| Box::new(InfoCommandHandler::new(tm.clone()))
    },
    "/mcp" => {
        desc: "Toggle an MCP server for this topic: /mcp on|off|reset <name>",
        continues: false,
        make: |_| Box::new(ToggleCommandHandler::mcp())
    },
    "/model" => {
        desc: "Switch AI model for this topic",
        continues: false,
        make: |_| Box::new(ModelCommandHandler)
    },
    "/new" => {
        desc: "Reset session and clear chat history (requires --force)",
        continues: false,
        make: |_| Box::new(NewCommandHandler)
    },
    "/pin" => {
        desc: "Pin this ad-hoc websocket topic to config.toml",
        continues: false,
        make: |tm| Box::new(PinCommandHandler::new(tm.clone()))
    },
    "/plan" => {
        desc: "Switch to plan mode (read-only)",
        continues: false,
        make: |_| Box::new(PlanCommandHandler)
    },
    "/reset" => {
        desc: "Reset session, keep chat history (requires --force)",
        continues: false,
        make: |_| Box::new(ResetCommandHandler)
    },
    "/skill" => {
        desc: "Toggle a skill for this topic: /skill on|off|reset <name>",
        continues: false,
        make: |_| Box::new(ToggleCommandHandler::skill())
    },
    "/template" => {
        desc: "Apply or re-apply topic template",
        continues: false,
        make: |_| Box::new(TemplateCommandHandler)
    },
    "/thinking" => {
        desc: "Show or hide AI thinking/reasoning content",
        continues: false,
        make: |_| Box::new(ThinkingCommandHandler)
    },
    "/ungrant" => {
        desc: "Revoke a runtime access grant",
        continues: false,
        make: |_| Box::new(UngrantCommandHandler)
    },
    "/unpin" => {
        desc: "Remove pinned topic configuration from config.toml",
        continues: false,
        make: |tm| Box::new(UnpinCommandHandler::new(tm.clone()))
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariants that used to be checked by keeping a second list in
    /// sync are now checked against the one list.
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

    /// `/fork` is the reason this file exists: it was registered but absent
    /// from the list the popup reads (#814). Both come from one table now, so
    /// the check that matters is that the table reaches the popup rows.
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
