//! Builtin tool: `request_access` — ask the user to allow a path outside the
//! working directory.
//!
//! The tool asks; it never grants. Every option it offers is a **command** the
//! user runs, and the answer travels back as that command verbatim
//! ([`AnswerStyle::Command`]): `/grant /abs/path -once`. The server's command
//! dispatcher executes it, so the decision is carried by a surface the model
//! cannot write to.
//!
//! That split is the point of the whole design. An earlier shape had the model
//! call back with `grant: "session"` after reading the user's answer, and it
//! could not be trusted: nothing in that path verifies what the user actually
//! said, so a model could skip the question, misread "no", or grant itself a
//! wider path than was offered. Handing the decision to the command layer
//! removes the model from the authorisation path entirely — it can only ask,
//! and the options it offers quote the exact path it was denied.
//!
//! The turn ends when the questions are handed over, exactly like `ask_user`:
//! this tool has no way to wait, and the grant has to be in force *before* the
//! turn that retries the tool starts. Commands run before the turn's
//! `ToolContext` is built, which is what makes the retry see the grant.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::ask_user::{Ask, deliver_questions};
use crate::tools::{Tool, ToolContext, ToolOutput};

/// The line the user's answer has to carry after the command.
///
/// A message holding only commands runs no agent turn — the worker stops once
/// the body is empty after commands are stripped. Without this line the grant
/// would be applied and the turn that asked for it would never resume, which
/// looks exactly like the grant having failed.
const FOLLOWUP: &str = "Continue.";

/// The option that declines. It is deliberately **not** a command: an answer
/// line carrying no `/` stays message text, so the model reads a refusal
/// instead of the server running something.
fn decline(path: &str) -> String {
    format!("I do not grant access to {path}.")
}

/// The path as a grant has to spell it.
///
/// A trailing slash would make the option name a different string than the one
/// the boundary check resolved, and `/` itself must survive: trimming it leaves
/// an empty path, which is not a grant.
fn quoted(raw: &str) -> &str {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() { raw } else { trimmed }
}

/// The options a permission request offers, narrowest first and the refusal
/// last.
///
/// Both grant options are built here rather than by the model, and every byte
/// of them comes from the path that was actually denied: a model that could
/// write the option text could offer a wider path than it was refused.
fn options_for(path: &str, write: bool) -> Vec<String> {
    let flag = if write { "-w " } else { "" };
    vec![
        format!("/grant {flag}{path} -once"),
        format!("/grant {flag}{path}"),
        decline(path),
    ]
}

/// Tool for asking the user to allow a path.
pub struct RequestAccessTool;

#[async_trait]
impl Tool for RequestAccessTool {
    fn name(&self) -> &str {
        "request_access"
    }

    fn description(&self) -> &str {
        "Ask the user to allow a path outside the working directory. Returns at \
         once: the turn ends and the answer arrives as the user's next message. Each \
         option is a command the user runs, so the grant is made by the server, not \
         by you — you cannot allow a path yourself, and you must not claim the user \
         allowed one that this call did not offer."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path the work needs. Use the resolved \
                                    path from the access-denied message."
                },
                "write": {
                    "type": "boolean",
                    "description": "Ask for write access rather than read. Default: false."
                },
                "reason": {
                    "type": "string",
                    "description": "One sentence on what the work needs it for. Shown \
                                    to the user above the options."
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let Some(raw) = input.get("path").and_then(|p| p.as_str()) else {
            return Ok(ToolOutput::error("Missing 'path' parameter"));
        };
        let write = input
            .get("write")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty());

        // Only an absolute path can be granted: `/grant` takes a path, and a
        // relative one would resolve against whatever directory the command
        // happens to run in, not against the caller's intent.
        let path = std::path::Path::new(raw);
        if !path.is_absolute() {
            return Ok(ToolOutput::error(format!(
                "'{raw}' is not absolute. A grant names a path, so ask with the resolved \
                 absolute path from the access-denied message."
            )));
        }

        // Already reachable: asking would spend the user's attention on a
        // decision that has no effect. Ask the same two functions the tools
        // call, so this can never disagree with what access they get.
        let check = if write {
            ctx.check_write_boundary(raw, path)
        } else {
            ctx.check_path_boundary(raw, path)
        };
        if check.is_ok() {
            let access = if write { "write" } else { "read" };
            return Ok(ToolOutput::success(format!(
                "{raw} is already within your {access} boundary — go ahead and use it. \
                 No grant is needed."
            )));
        }

        // The options are built here, not by the model, and every byte of the
        // command comes from the path that was actually denied.
        let quoted = quoted(raw);
        let options = options_for(quoted, write);
        let question = match reason {
            Some(reason) => format!("Allow access to {quoted}? {reason}"),
            None => format!("Allow access to {quoted}?"),
        };
        // One line: the tool renders one item per line on every surface, and a
        // model-written line break would push the options around.
        let question = question.split_whitespace().collect::<Vec<_>>().join(" ");
        let asks = vec![Ask {
            question,
            options,
            allow_multiple: false,
        }];

        Ok(deliver_questions(
            ctx,
            &asks,
            &jyc_types::channel::AnswerStyle::Command {
                followup: FOLLOWUP.to_string(),
            },
        )
        .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The options a permission request offers for a read: two grants quoting
    /// the path that was denied, then a refusal. `-once` comes first because it
    /// is the narrower promise — the one a cautious user reaches for.
    #[test]
    fn read_options_are_commands_naming_the_path() {
        assert_eq!(
            options_for("/srv/data", false),
            vec![
                "/grant /srv/data -once".to_string(),
                "/grant /srv/data".to_string(),
                "I do not grant access to /srv/data.".to_string(),
            ]
        );
    }

    /// A write request asks for write: the flag rides inside the command, which
    /// is the only place it can be seen before the user picks.
    #[test]
    fn write_options_carry_the_write_flag() {
        assert_eq!(
            options_for("/srv/data", true),
            vec![
                "/grant -w /srv/data -once".to_string(),
                "/grant -w /srv/data".to_string(),
                "I do not grant access to /srv/data.".to_string(),
            ]
        );
    }

    /// Only the grant options are commands. A refusal line starting with `/`
    /// would be dispatched instead of read, so the user's "no" would run
    /// something.
    #[test]
    fn only_the_grant_options_are_commands() {
        let options = options_for("/srv/data", false);
        let commands = options.iter().filter(|o| o.starts_with('/')).count();
        assert_eq!(commands, 2, "exactly the two grants: {options:?}");
        assert!(
            options.last().is_some_and(|o| !o.starts_with('/')),
            "the last option is the refusal and must not be a command: {options:?}"
        );
    }

    /// A trailing slash would name a different string than the one the boundary
    /// check resolved, so the option drops it.
    #[test]
    fn a_trailing_slash_is_dropped_from_the_quoted_path() {
        assert_eq!(quoted("/srv/data/"), "/srv/data");
    }

    /// The root directory survives the trailing-slash rule: trimming it would
    /// leave an empty path, and an empty path is not a grant.
    #[test]
    fn the_root_path_survives_trimming() {
        assert_eq!(quoted("/"), "/");
        assert_eq!(options_for(quoted("/"), false)[1], "/grant /");
    }

    /// The follow-up line has to be message text: a message made only of
    /// commands ends the worker before any turn runs, so the grant would land
    /// and nothing would resume to use it.
    #[test]
    fn the_followup_line_is_not_a_command() {
        assert!(!FOLLOWUP.starts_with('/'));
        assert!(!FOLLOWUP.trim().is_empty());
    }

    /// The description has to say the user decides. The model's failure mode
    /// here is treating its own request as a grant it can proceed on, and the
    /// one text it reads about this tool is the description.
    #[test]
    fn the_description_denies_the_model_the_power_to_grant() {
        let tool = RequestAccessTool;
        let description = tool.description();
        assert!(
            description.contains("the grant is made by the server, not by you"),
            "the description must deny the model the power to grant: {description}"
        );
        assert!(
            description.contains("the turn ends"),
            "the description must say the turn ends, or the model waits for an \
             answer that cannot arrive in it: {description}"
        );
    }
}
