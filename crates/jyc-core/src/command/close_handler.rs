use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;

use super::flags::{self, CLOSE_FLAGS, CloseFlag};
use super::handler::{CommandContext, CommandHandler, CommandResult};
use super::send_handler::{SendRequest, SendSource, send_to_topic};
use super::spawn_handler::{format_subagent_names, subagents_of};
use crate::topic_manager::{PurgeOutcome, TopicManager};

/// /close command — close a topic, and with `--purge` delete its directory too.
pub struct CloseCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl CloseCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }

    /// Returns `true` if the args carry the explicit guard flag (`-f`/`--force`).
    ///
    /// Plain `/close` (no args) returns `false` so the handler can emit a
    /// warning instead of deleting (mirrors `/new`).
    fn is_forced(args: &[String]) -> bool {
        flags::has(CLOSE_FLAGS, args, CloseFlag::Force)
    }

    /// Returns `true` if the args ask for the topic directory to go as well
    /// (`-p`/`--purge`).
    ///
    /// Only meaningful together with `--force`: `/close` alone never deletes
    /// anything, whatever else it is given.
    fn is_purged(args: &[String]) -> bool {
        flags::has(CLOSE_FLAGS, args, CloseFlag::Purge)
    }

    /// The word in the args that names what to close, if any.
    ///
    /// `all` is a keyword: a subagent that happens to be called `all` is closed
    /// as part of the batch rather than by name. Flags are not names, and the
    /// blank check keeps a stray empty word — which args built with
    /// `split_whitespace` cannot normally produce — from reading as one.
    fn target(args: &[String]) -> Option<&str> {
        args.iter()
            .map(String::as_str)
            .find(|a| !a.starts_with('-') && !a.trim().is_empty())
    }

    /// Send `body` to a subagent, so that topic's own queue runs it.
    ///
    /// The child closes itself — this handler never touches its directory, and
    /// the guard the child has in front of its own `/close` still applies.
    async fn send_close_command(
        &self,
        context: &CommandContext,
        name: &str,
        body: &str,
    ) -> Result<String> {
        let managers = self
            .topic_manager
            .managers()
            .ok_or_else(|| anyhow::anyhow!("The agents channel is unreachable from here"))?;
        send_to_topic(
            &managers,
            SendSource {
                channel: None,
                topic: Some(&context.topic_name),
                working_dir: &context.topic_path,
            },
            SendRequest {
                agent: name,
                topic: None,
                message: body,
                attachments: &[],
                recipient: None,
                require_reply: false,
            },
        )
        .await
    }

    /// What a subagent is told to run: the very `/close` a user would type in
    /// that topic, forced — the guard on this side has already passed.
    fn close_body(args: &[String]) -> &'static str {
        if Self::is_purged(args) {
            "/close --force --purge"
        } else {
            "/close --force"
        }
    }

    /// The subagent names `target` selects, or the refusal that says why none.
    ///
    /// Split out from the send path so the rule — `all`, or exactly one of this
    /// topic's subagents — is testable without a manager behind it.
    fn subagent_targets(
        target: &str,
        children: &[(String, PathBuf)],
    ) -> Result<Vec<String>, String> {
        match target {
            "all" => Ok(children.iter().map(|(name, _)| name.clone()).collect()),
            name if children.iter().any(|(child, _)| child == name) => Ok(vec![name.to_string()]),
            name if children.is_empty() => Err(format!(
                "'{name}' is not a subagent of this topic, so `/close` leaves it alone. \
                 This topic has no subagents."
            )),
            name => Err(format!(
                "'{name}' is not a subagent of this topic, so `/close` leaves it alone. \
                 Subagents: {}",
                format_subagent_names(children)
            )),
        }
    }

    /// Close the subagent `target` names, or every one of them for `all`.
    ///
    /// The subagents are this topic's own — the list the spawn prompt shows,
    /// which is what `subagents_of` returns — because `/close` is a parent's
    /// lever over its children and nothing else.
    async fn close_subagents(
        &self,
        context: &CommandContext,
        target: &str,
    ) -> Result<CommandResult> {
        let children = subagents_of(&context.topic_name);
        let names = match Self::subagent_targets(target, &children) {
            Ok(names) => names,
            Err(message) => return Ok(CommandResult::refusal(message)),
        };

        if names.is_empty() {
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "Nothing to close: this topic has no subagents (`/close {target}`)."
                ),
                error: None,
                append_body: None,
            });
        }

        // The same guard closing this topic has: the batch is not fired by a
        // typo, and the warning names what will go.
        if !Self::is_forced(&context.args) {
            let what = if names.len() == 1 {
                format!("subagent '{}'", names[0])
            } else {
                format!("{} subagents: {}", names.len(), names.join(", "))
            };
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "⚠️  /close will close {what} — a subagent of topic '{}', not this topic \
                     itself. Each one closes itself and takes everything under it (chat \
                     history, AI session, attachments). This cannot be undone.\n\
                     \n\
                     To proceed, send: /close {target} --force",
                    context.topic_name
                ),
                error: None,
                append_body: None,
            });
        }

        let body = Self::close_body(&context.args);
        let mut sent: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        for name in &names {
            match self.send_close_command(context, name, body).await {
                Ok(_) => sent.push(name.clone()),
                Err(e) => failed.push(format!("{name}: {e}")),
            }
        }

        // What went out, not what it did: the message is dispatched to the child's
        // queue, and the child's own reply lands in its own topic — so this side
        // can only report the dispatch.
        let mut message = if sent.is_empty() {
            format!("Could not dispatch '{body}' to any subagent.")
        } else if sent.len() == 1 {
            format!(
                "Dispatched '{body}' to subagent '{}' — it closes itself on its own queue.",
                sent[0]
            )
        } else {
            format!(
                "Dispatched '{body}' to {} subagents: {}.",
                sent.len(),
                sent.join(", ")
            )
        };
        if !failed.is_empty() {
            message.push_str(&format!(" Failed: {}.", failed.join("; ")));
        }
        tracing::info!(
            target = %target,
            sent = sent.len(),
            failed = failed.len(),
            "Subagent close dispatched"
        );
        Ok(CommandResult {
            success: failed.is_empty(),
            message: message.clone(),
            // The reason rides in `error` too, or the reply would show only the
            // terse summary and drop which child failed (see `refusal`).
            error: (!failed.is_empty()).then_some(message),
            append_body: None,
        })
    }
}

#[async_trait]
impl CommandHandler for CloseCommandHandler {
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // The topic identity is the name, not the workspace dir: a child that
        // shares its parent's dir works there too, so deriving the name from the
        // path would close/purge the parent instead of the child.
        let topic_name = context.topic_name.as_str();

        if topic_name.is_empty() {
            return Ok(CommandResult {
                success: false,
                message: "Failed to determine topic name (topic_name is empty)".to_string(),
                error: Some("Topic name is empty".into()),
                append_body: None,
            });
        }

        // A word in the args names a subagent to close: that topic runs the
        // `/close --force` itself, so nothing here touches a child's directory.
        if let Some(target) = Self::target(&context.args) {
            return self.close_subagents(&context, target).await;
        }

        // Require --force to prevent accidental topic deletion. Plain
        // `/close` returns a warning instead of the destructive action.
        if !Self::is_forced(&context.args) {
            return Ok(CommandResult {
                success: true,
                message: format!(
                    "⚠️  /close will PERMANENTLY delete topic '{topic_name}' and all its data \
                     (chat history, AI session, attachments). This cannot be undone.\n\
                     \n\
                     To proceed, send: /close --force"
                ),
                error: None,
                append_body: None,
            });
        }

        // Purge the directory only when it is this topic's to delete. A
        // refusal does not block the close: the topic's state goes, only
        // the directory is kept (reported in the final message).
        let mut purge_refused: Option<String> = None;
        if Self::is_purged(&context.args) {
            match self.topic_manager.purge_topic_dir(topic_name).await? {
                PurgeOutcome::Refused(reason) => {
                    tracing::info!(topic = %topic_name, reason = %reason, "Purge refused");
                    purge_refused = Some(reason);
                }
                PurgeOutcome::Deleted => {
                    tracing::info!(topic = %topic_name, dir = %context.topic_path.display(), "Purged topic dir");
                }
                PurgeOutcome::Nothing => {}
            }
        }

        // Notification-only; fires before deletion so a hook can archive
        // the topic directory while it still exists.
        context.session_end_hook("close").await;

        match self.topic_manager.close_topic(topic_name).await {
            Ok(()) => {
                tracing::info!(topic = %topic_name, "Topic closed successfully via /close command");
                // `close_topic` deletes a dir that held nothing but its own
                // state, and keeps one that belongs to somebody — a child that
                // works in this dir, a `/spawn` in a directory somebody chose,
                // or a spawn dir that has since gained files. Ask the disk
                // rather than restating that branch here.
                let kept = tokio::fs::try_exists(&context.topic_path)
                    .await
                    .unwrap_or(false);
                let message = match purge_refused {
                    Some(reason) => format!(
                        "Topic '{topic_name}' closed; its state is deleted and {} is kept: {reason}",
                        context.topic_path.display()
                    ),
                    None if kept => format!(
                        "Topic '{topic_name}' closed; its state is deleted and {} is kept. \
                         Add --purge to delete the directory as well:\n/close --force --purge",
                        context.topic_path.display()
                    ),
                    None => {
                        format!("Topic '{topic_name}' closed; its directory and data are deleted.")
                    }
                };
                Ok(CommandResult {
                    success: true,
                    message,
                    error: None,
                    append_body: None,
                })
            }
            Err(e) => Ok(CommandResult {
                success: false,
                message: format!("Failed to close topic '{}'", topic_name),
                error: Some(e.context("close_topic failed").to_string()),
                append_body: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn is_forced_accepts_force_flag() {
        assert!(CloseCommandHandler::is_forced(&args(&["--force"])));
    }

    /// The guard and the purge flag answer to their short spellings too: both
    /// are read off the declaration the popup offers them from.
    #[test]
    fn is_forced_and_is_purged_read_the_short_spellings() {
        assert!(CloseCommandHandler::is_forced(&args(&["-f"])));
        assert!(CloseCommandHandler::is_forced(&args(&["joe", "-f"])));
        assert!(CloseCommandHandler::is_purged(&args(&["-f", "-p"])));
        assert!(!CloseCommandHandler::is_forced(&args(&["-p"])));
        assert!(!CloseCommandHandler::is_purged(&args(&["-f"])));
    }

    #[test]
    fn is_forced_rejects_old_confirm_tokens() {
        // `-y`/`--confirm` no longer bypass the guard — only `--force`.
        assert!(!CloseCommandHandler::is_forced(&args(&["-y"])));
        assert!(!CloseCommandHandler::is_forced(&args(&["--confirm"])));
        assert!(!CloseCommandHandler::is_forced(&args(&["--yes"])));
        assert!(!CloseCommandHandler::is_forced(&args(&["--foo"])));
    }

    #[test]
    fn is_forced_rejects_empty_args() {
        assert!(!CloseCommandHandler::is_forced(&args(&[])));
    }

    #[test]
    fn is_forced_accepts_flag_mixed_with_unknown_args() {
        assert!(CloseCommandHandler::is_forced(&args(&["--foo", "--force"])));
    }

    #[test]
    fn is_purged_only_answers_for_its_own_flag() {
        assert!(CloseCommandHandler::is_purged(&args(&[
            "--force", "--purge"
        ])));
        assert!(CloseCommandHandler::is_purged(&args(&["--purge"])));
        assert!(!CloseCommandHandler::is_purged(&args(&["--force"])));
        assert!(!CloseCommandHandler::is_purged(&args(&[])));
    }

    #[test]
    fn target_reads_the_first_bare_word() {
        assert_eq!(CloseCommandHandler::target(&args(&["joe"])), Some("joe"));
        assert_eq!(
            CloseCommandHandler::target(&args(&["--force", "joe", "--purge"])),
            Some("joe")
        );
        assert_eq!(CloseCommandHandler::target(&args(&["--force"])), None);
        assert_eq!(CloseCommandHandler::target(&args(&["--purge"])), None);
        assert_eq!(CloseCommandHandler::target(&args(&[])), None);
    }

    /// `all` is a keyword even when a subagent carries that name — and only
    /// this topic's own subagents can be named at all.
    #[test]
    fn all_is_a_keyword_and_other_names_must_be_subagents() {
        let children = vec![
            ("joe".to_string(), PathBuf::from("/tmp/joe")),
            ("all".to_string(), PathBuf::from("/tmp/all")),
        ];
        assert_eq!(
            CloseCommandHandler::subagent_targets("all", &children).unwrap(),
            vec!["joe".to_string(), "all".to_string()]
        );
        assert_eq!(
            CloseCommandHandler::subagent_targets("joe", &children).unwrap(),
            vec!["joe".to_string()]
        );
        let refusal = CloseCommandHandler::subagent_targets("nope", &children).unwrap_err();
        assert!(refusal.contains("is not a subagent"), "{refusal}");
        assert!(refusal.contains("joe, all"), "{refusal}");

        // With no subagents to list, the refusal says so rather than dangling.
        let refusal = CloseCommandHandler::subagent_targets("nope", &[]).unwrap_err();
        assert!(refusal.contains("no subagents"), "{refusal}");
    }

    /// What a subagent is told to run is always forced, and carries `--purge`
    /// through when the caller asked for it.
    #[test]
    fn close_body_forwards_purge_and_always_forces() {
        assert_eq!(
            CloseCommandHandler::close_body(&args(&[])),
            "/close --force"
        );
        assert_eq!(
            CloseCommandHandler::close_body(&args(&["joe", "--force"])),
            "/close --force"
        );
        assert_eq!(
            CloseCommandHandler::close_body(&args(&["joe", "--force", "--purge"])),
            "/close --force --purge"
        );
    }
}
