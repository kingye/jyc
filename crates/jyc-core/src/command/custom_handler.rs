use anyhow::Result;
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use super::mode_handler::set_mode;
use jyc_types::CustomCommand;

/// Handler for a user-defined command declared in `config.toml` `[[commands]]`.
///
/// On invocation it switches the thread mode (when the command declares one)
/// and injects the command's skills + `user_prompt` into the message body via
/// [`CommandResult::append_body`], so the agent receives them as part of the
/// user turn.
///
/// Skill *paths* are not resolved here: the system prompt already lists every
/// discovered skill with its path and description, so naming the skills is
/// enough for the agent to locate and read the right `SKILL.md`.
pub struct CustomCommandHandler {
    /// Command name including the leading slash (e.g. `/review`).
    name: String,
    config: CustomCommand,
}

impl CustomCommandHandler {
    /// Create a handler for `config`, normalizing the name to include a
    /// leading slash.
    pub fn new(config: CustomCommand) -> Self {
        let name = format!("/{}", config.name.trim().trim_start_matches('/'));
        Self { name, config }
    }

    /// Build the text appended to the message body: a skills directive
    /// (when the command names skills) followed by the `user_prompt`.
    fn build_append_body(&self) -> String {
        let mut out = String::new();

        if let Some(skills) = self.config.skills.as_ref().filter(|s| !s.is_empty()) {
            out.push_str(&format!(
                "For this task, use these skills: {}.\n\
                 Read their SKILL.md before starting.\n\n",
                skills.join(", ")
            ));
        }

        out.push_str(self.config.user_prompt.trim());
        out
    }
}

#[async_trait]
impl CommandHandler for CustomCommandHandler {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.config.description
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let mut notes = Vec::new();

        if let Some(ref mode) = self.config.mode {
            set_mode(&context, mode).await?;
            notes.push(format!("mode={mode}"));
        }
        if let Some(skills) = self.config.skills.as_ref().filter(|s| !s.is_empty()) {
            notes.push(format!("skills={}", skills.join(", ")));
        }

        let message = if notes.is_empty() {
            format!("{}: applied", self.name)
        } else {
            format!("{}: {}", self.name, notes.join(", "))
        };

        Ok(CommandResult {
            success: true,
            message,
            error: None,
            append_body: Some(self.build_append_body()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn cmd(mode: Option<&str>, skills: Option<Vec<&str>>) -> CustomCommand {
        CustomCommand {
            name: "review".into(),
            description: "Review the PR".into(),
            mode: mode.map(|m| m.to_string()),
            skills: skills.map(|v| v.into_iter().map(|s| s.to_string()).collect()),
            user_prompt: "Review the diff and report findings.".into(),
        }
    }

    fn test_context(thread_path: &Path) -> CommandContext {
        CommandContext {
            args: vec![],
            thread_path: thread_path.to_path_buf(),
            config: Arc::new(
                jyc_types::load_config_from_str(
                    r#"
[general]
[channels.test]
type = "email"
[channels.test.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.test.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
"#,
                )
                .unwrap(),
            ),
            channel: "test".into(),
            agent: None,
            template_dirs: PathBuf::from("/tmp/test/templates").into(),
            channel_type: "websocket".to_string(),
            config_path: None,
        }
    }

    #[test]
    fn name_gets_leading_slash() {
        assert_eq!(CustomCommandHandler::new(cmd(None, None)).name(), "/review");
    }

    #[test]
    fn name_is_not_double_slashed() {
        let mut c = cmd(None, None);
        c.name = "/review".into();
        assert_eq!(CustomCommandHandler::new(c).name(), "/review");
    }

    #[tokio::test]
    async fn plan_mode_writes_mode_override() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = CustomCommandHandler::new(cmd(Some("plan"), None));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();

        assert!(result.success);
        let mode = tokio::fs::read_to_string(tmp.path().join(".jyc/mode-override"))
            .await
            .unwrap();
        assert_eq!(mode, "plan");
    }

    #[tokio::test]
    async fn build_mode_clears_mode_override() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("mode-override"), "plan")
            .await
            .unwrap();

        let handler = CustomCommandHandler::new(cmd(Some("build"), None));
        handler.execute(test_context(tmp.path())).await.unwrap();

        assert!(!jyc_dir.join("mode-override").exists());
    }

    #[tokio::test]
    async fn no_mode_leaves_mode_override_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(jyc_dir.join("mode-override"), "plan")
            .await
            .unwrap();

        let handler = CustomCommandHandler::new(cmd(None, None));
        handler.execute(test_context(tmp.path())).await.unwrap();

        // Still in plan mode — the command did not touch it.
        let mode = tokio::fs::read_to_string(jyc_dir.join("mode-override"))
            .await
            .unwrap();
        assert_eq!(mode, "plan");
    }

    #[tokio::test]
    async fn append_body_names_skills_then_user_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let handler =
            CustomCommandHandler::new(cmd(None, Some(vec!["pr-review", "coding-principles"])));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();
        let body = result.append_body.unwrap();

        assert!(body.contains("pr-review, coding-principles"));
        assert!(body.contains("SKILL.md"));
        // user_prompt comes last so it is the most recent instruction.
        assert!(
            body.trim_end()
                .ends_with("Review the diff and report findings.")
        );
    }

    #[tokio::test]
    async fn append_body_without_skills_is_just_user_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = CustomCommandHandler::new(cmd(None, None));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();

        assert_eq!(
            result.append_body.unwrap(),
            "Review the diff and report findings."
        );
    }

    #[tokio::test]
    async fn empty_skills_list_is_treated_as_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let handler = CustomCommandHandler::new(cmd(None, Some(vec![])));

        let result = handler.execute(test_context(tmp.path())).await.unwrap();

        assert_eq!(
            result.append_body.unwrap(),
            "Review the diff and report findings."
        );
    }
}
