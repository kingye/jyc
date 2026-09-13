use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::session_state::{
    MCP_OVERRIDE_FILE, SKILL_OVERRIDE_FILE, clear_toggle_override, read_toggle_override,
    write_toggle_override,
};

/// `/skill` and `/mcp`: toggle one name on/off for this topic at runtime.
///
/// Both commands share the same mechanics (subcommand parse + persisted
/// `ToggleOverride` file), so one parameterized handler serves both —
/// constructed via [`ToggleCommandHandler::skill`] / [`ToggleCommandHandler::mcp`].
/// Overrides take effect from the next message (tool registry and system
/// prompt are rebuilt per message) and persist until `reset`.
pub struct ToggleCommandHandler {
    /// Command name, e.g. "/skill".
    name: &'static str,
    /// What the names refer to, for user-facing messages.
    noun: &'static str,
    /// Override file name inside the topic `.jyc/` dir.
    file: &'static str,
    /// Validate that a name may be force-enabled; `Some(msg)` rejects it.
    validate_on: fn(&CommandContext, &str) -> Option<String>,
}

impl ToggleCommandHandler {
    /// The `/skill` command (skill-name on/off for this topic).
    pub fn skill() -> Self {
        Self {
            name: "/skill",
            noun: "skill",
            file: SKILL_OVERRIDE_FILE,
            validate_on: validate_skill_exists,
        }
    }

    /// The `/mcp` command (MCP-server on/off for this topic).
    pub fn mcp() -> Self {
        Self {
            name: "/mcp",
            noun: "MCP server",
            file: MCP_OVERRIDE_FILE,
            validate_on: validate_mcp_defined,
        }
    }
}

#[async_trait]
impl CommandHandler for ToggleCommandHandler {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Toggle one name on/off for this topic: /<cmd> on|off|reset <name>"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let topic_name = context.topic_name.as_str();
        let topic_path = &context.topic_path;
        let usage = format!("Usage: {} on|off|reset <name>", self.name);

        let mut ovr = read_toggle_override(topic_name, topic_path, self.file)
            .await
            .unwrap_or_default();

        // No args: show current override state.
        if context.args.is_empty() {
            return Ok(ok(format!(
                "{}: on=[{}] off=[{}] (empty means config defaults; {} reset <name> not supported — use `{} reset` to clear all)",
                self.name,
                ovr.on.join(", "),
                ovr.off.join(", "),
                self.name,
                self.name,
            )));
        }

        let sub = context.args[0].to_lowercase();
        match sub.as_str() {
            "on" | "off" => {
                let Some(target) = context.args.get(1) else {
                    return Err(anyhow::anyhow!("missing <name>. {usage}"));
                };
                if sub == "on"
                    && let Some(err) = (self.validate_on)(&context, target)
                {
                    return Ok(fail(err));
                }
                ovr.set(target, sub == "on");
                write_toggle_override(topic_name, topic_path, self.file, &ovr).await?;
                Ok(ok(format!(
                    "{}: {} `{target}` forced {} for this topic — takes effect from the next message",
                    self.name, self.noun, sub
                )))
            }
            "reset" => {
                clear_toggle_override(topic_name, topic_path, self.file).await;
                Ok(ok(format!(
                    "{}: override cleared, {} selection is back to config defaults",
                    self.name, self.noun
                )))
            }
            other => Err(anyhow::anyhow!("unknown subcommand `{other}`. {usage}")),
        }
    }
}

fn ok(message: String) -> CommandResult {
    CommandResult {
        success: true,
        message,
        error: None,
        append_body: None,
    }
}

fn fail(error: String) -> CommandResult {
    CommandResult {
        success: false,
        message: String::new(),
        error: Some(error),
        append_body: None,
    }
}

/// Reject forcing on a skill whose `SKILL.md` is not discoverable under the
/// same roots `discover_skills` scans (minus the process workdir root, which
/// only ever holds the built-in repo skills — all also present in the global
/// dir). `off` is never validated: un-disabling a typo is a harmless no-op.
fn validate_skill_exists(context: &CommandContext, skill: &str) -> Option<String> {
    if skill.is_empty() || skill.contains('/') || skill.contains("..") {
        return Some(format!("invalid skill name `{skill}`"));
    }
    let topic_path = &context.topic_path;
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        roots.push(PathBuf::from(&home).join(".config/opencode/skills"));
        roots.push(PathBuf::from(&home).join(".claude/skills"));
    }
    if let Some(global) = jyc_utils::paths::global_skills_dir() {
        roots.push(global);
    }
    for rel in [
        "skills",
        ".claude/skills",
        ".opencode/skills",
        "repo/.claude/skills",
        "repo/.jyc/skills",
    ] {
        roots.push(topic_path.join(rel));
    }
    roots.push(jyc_types::state_dir::jyc_dir(&context.topic_name, topic_path).join("skills"));
    // Per-agent skill dirs are configured via `skills = [...]` whitelists over
    // these same roots, so a name found here may still be off-config — forcing
    // it on via the override is exactly the point of the command.
    if roots
        .iter()
        .any(|r| r.join(skill).join("SKILL.md").is_file())
    {
        None
    } else {
        Some(format!(
            "unknown skill `{skill}` (no SKILL.md under scanned skill dirs)"
        ))
    }
}

/// Reject forcing on an MCP server that no config layer defines — enabling a
/// nonexistent server would silently do nothing. `off` is never validated.
fn validate_mcp_defined(context: &CommandContext, server: &str) -> Option<String> {
    let cfg = &context.config;
    let mut names: Vec<&str> = Vec::new();
    names.extend(cfg.mcps.iter().map(|m| m.name.as_str()));
    for ch in cfg.channels.values() {
        if let Some(mcps) = &ch.mcps {
            names.extend(mcps.iter().map(|m| m.name.as_str()));
        }
        if let Some(patterns) = &ch.patterns {
            for p in patterns {
                if let Some(mcps) = &p.mcps {
                    names.extend(mcps.iter().map(|m| m.name.as_str()));
                }
            }
        }
    }
    for agent in cfg.agents.values() {
        if let Some(mcps) = &agent.mcps {
            names.extend(mcps.iter().map(|m| m.name.as_str()));
        }
    }
    names.sort_unstable();
    names.dedup();
    if names.contains(&server) {
        None
    } else {
        Some(format!(
            "unknown MCP server `{server}` (defined: {})",
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_state::ToggleOverride;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx(topic_path: PathBuf) -> CommandContext {
        CommandContext {
            topic_name: "t".into(),
            topic_path,
            ..Default::default()
        }
    }

    /// Trivial validator: everything exists.
    fn allow_all(_: &CommandContext, _: &str) -> Option<String> {
        None
    }

    fn handler(file: &'static str) -> ToggleCommandHandler {
        ToggleCommandHandler {
            name: "/test",
            noun: "thing",
            file,
            validate_on: allow_all,
        }
    }

    async fn run(
        h: &ToggleCommandHandler,
        args: &[&str],
        context: &CommandContext,
    ) -> CommandResult {
        let mut c = context.clone();
        c.args = args.iter().map(|s| s.to_string()).collect();
        h.execute(c).await.unwrap()
    }

    #[tokio::test]
    async fn on_off_reset_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let context = ctx(tmp.path().to_path_buf());
        let h = handler(MCP_OVERRIDE_FILE);

        let r = run(&h, &["on", "srv"], &context).await;
        assert!(r.success);
        let ovr = read_toggle_override("t", tmp.path(), MCP_OVERRIDE_FILE)
            .await
            .unwrap();
        assert_eq!(ovr.on, vec!["srv".to_string()]);

        // Toggling the same name to off moves it across lists.
        run(&h, &["off", "srv"], &context).await;
        let ovr = read_toggle_override("t", tmp.path(), MCP_OVERRIDE_FILE)
            .await
            .unwrap();
        assert!(ovr.on.is_empty() && ovr.off == vec!["srv".to_string()]);

        // reset removes the file entirely.
        let r = run(&h, &["reset"], &context).await;
        assert!(r.success);
        assert!(
            read_toggle_override("t", tmp.path(), MCP_OVERRIDE_FILE)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn on_rejected_by_validator() {
        let tmp = TempDir::new().unwrap();
        let context = ctx(tmp.path().to_path_buf());
        let h = ToggleCommandHandler {
            name: "/test",
            noun: "thing",
            file: MCP_OVERRIDE_FILE,
            validate_on: |_, name| Some(format!("nope: {name}")),
        };
        let r = run(&h, &["on", "x"], &context).await;
        assert!(!r.success);
        assert_eq!(r.error.as_deref(), Some("nope: x"));
        assert!(
            read_toggle_override("t", tmp.path(), MCP_OVERRIDE_FILE)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn no_args_shows_state() {
        let tmp = TempDir::new().unwrap();
        let context = ctx(tmp.path().to_path_buf());
        let h = handler(MCP_OVERRIDE_FILE);
        run(&h, &["on", "a"], &context).await;
        let r = run(&h, &[], &context).await;
        assert!(r.success);
        assert!(r.message.contains("on=[a]"));
    }

    #[test]
    fn apply_toggle_semantics() {
        let base = vec!["a".to_string(), "b".to_string()];
        let ovr = ToggleOverride {
            on: vec!["a".into()],
            off: vec!["c".into()],
        };
        assert_eq!(ovr.apply(&base), vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn mcp_validator_against_config() {
        let config = jyc_types::load_config_from_str(
            r#"
[[mcps]]
name = "global_srv"
type = "local"
command = ["true"]

[agents.x]
[[agents.x.mcps]]
name = "agent_srv"
type = "local"
command = ["true"]
"#,
        )
        .unwrap();
        let mut context = ctx(PathBuf::new());
        context.config = Arc::new(config);
        assert!(validate_mcp_defined(&context, "global_srv").is_none());
        assert!(validate_mcp_defined(&context, "agent_srv").is_none());
        let err = validate_mcp_defined(&context, "ghost").unwrap();
        assert!(err.contains("agent_srv, global_srv"), "{err}");
    }

    #[test]
    fn skill_validator_finds_topic_dir_skill() {
        let tmp = TempDir::new().unwrap();
        let skill_dir = tmp.path().join(".claude/skills/ztest-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: ztest-skill\ndescription: d\n---\nbody",
        )
        .unwrap();
        let context = ctx(tmp.path().to_path_buf());
        // Unique miss-name so HOME-dir pollution cannot flip it.
        assert!(validate_skill_exists(&context, "zz-absent-skill-9k7").is_some());
        assert!(validate_skill_exists(&context, "ztest-skill").is_none());
        assert!(validate_skill_exists(&context, "../escape").is_some());
    }
}
