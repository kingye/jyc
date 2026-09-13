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
    /// Whether the name exists today — used only to hint the user after a
    /// successful write; the toggle itself is always persisted (an unknown
    /// name is a harmless no-op that may materialize later).
    known: fn(&CommandContext, &str) -> bool,
}

impl ToggleCommandHandler {
    /// The `/skill` command (skill-name on/off for this topic).
    pub fn skill() -> Self {
        Self {
            name: "/skill",
            noun: "skill",
            file: SKILL_OVERRIDE_FILE,
            known: skill_known,
        }
    }

    /// The `/mcp` command (MCP-server on/off for this topic).
    pub fn mcp() -> Self {
        Self {
            name: "/mcp",
            noun: "MCP server",
            file: MCP_OVERRIDE_FILE,
            known: mcp_known,
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
                ovr.set(target, sub == "on");
                write_toggle_override(topic_name, topic_path, self.file, &ovr).await?;
                let hint = if (self.known)(&context, target) {
                    String::new()
                } else {
                    format!(
                        " — note: no `{target}` among current {}s yet, double-check the name (`{} reset` to undo)",
                        self.noun, self.name
                    )
                };
                Ok(ok(format!(
                    "{}: {} `{target}` forced {} for this topic — takes effect from the next message{hint}",
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

/// Whether a skill is discoverable right now. Mirrors the roots
/// `discover_skills` scans, minus the process workdir root (its repo skills
/// ship in the global dir too). A false negative only loses a hint — the
/// toggle is still written, so it works once the skill materializes.
fn skill_known(context: &CommandContext, skill: &str) -> bool {
    if skill.is_empty() || skill.contains('/') || skill.contains("..") {
        return false;
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
    roots
        .iter()
        .any(|r| r.join(skill).join("SKILL.md").is_file())
}

/// Whether any config layer (global / channel / pattern / agent / L3 topic
/// overlay) defines this MCP server. Same layers `disabled_mcps` merges from,
/// so an `on` hint stays honest for topic-local servers.
fn mcp_known(context: &CommandContext, server: &str) -> bool {
    let cfg = &context.config;
    let hit = |mcps: &[jyc_types::McpServerConfig]| mcps.iter().any(|m| m.name == server);
    if hit(&cfg.mcps) {
        return true;
    }
    for ch in cfg.channels.values() {
        if let Some(mcps) = &ch.mcps
            && hit(mcps)
        {
            return true;
        }
        if let Some(patterns) = &ch.patterns {
            for p in patterns {
                if let Some(mcps) = &p.mcps
                    && hit(mcps)
                {
                    return true;
                }
            }
        }
    }
    for agent in cfg.agents.values() {
        if let Some(mcps) = &agent.mcps
            && hit(mcps)
        {
            return true;
        }
    }
    if let Some(topic_cfg) = jyc_types::load_topic_config(&context.topic_name, &context.topic_path)
        && let Some(mcps) = &topic_cfg.mcps
    {
        return hit(mcps);
    }
    false
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

    /// Trivial predicate: everything exists.
    fn allow_all(_: &CommandContext, _: &str) -> bool {
        true
    }

    fn handler(file: &'static str) -> ToggleCommandHandler {
        ToggleCommandHandler {
            name: "/test",
            noun: "thing",
            file,
            known: allow_all,
        }
    }

    async fn try_run(
        h: &ToggleCommandHandler,
        args: &[&str],
        context: &CommandContext,
    ) -> Result<CommandResult> {
        let mut c = context.clone();
        c.args = args.iter().map(|s| s.to_string()).collect();
        h.execute(c).await
    }

    async fn run(
        h: &ToggleCommandHandler,
        args: &[&str],
        context: &CommandContext,
    ) -> CommandResult {
        try_run(h, args, context).await.unwrap()
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
    async fn unknown_name_gets_hint_but_toggle_persists() {
        let tmp = TempDir::new().unwrap();
        let context = ctx(tmp.path().to_path_buf());
        let h = ToggleCommandHandler {
            name: "/test",
            noun: "thing",
            file: MCP_OVERRIDE_FILE,
            known: |_, _| false,
        };
        let r = run(&h, &["on", "x"], &context).await;
        assert!(r.success);
        assert!(r.message.contains("double-check the name"), "{}", r.message);
        // The toggle is still written — harmless until the name materializes.
        let ovr = read_toggle_override("t", tmp.path(), MCP_OVERRIDE_FILE)
            .await
            .unwrap();
        assert_eq!(ovr.on, vec!["x".to_string()]);
    }

    #[tokio::test]
    async fn missing_name_and_unknown_subcommand_error() {
        let tmp = TempDir::new().unwrap();
        let context = ctx(tmp.path().to_path_buf());
        let h = handler(MCP_OVERRIDE_FILE);
        let e = try_run(&h, &["on"], &context).await.unwrap_err();
        assert!(e.to_string().contains("missing <name>"), "{e}");
        let e = try_run(&h, &["frobnicate", "x"], &context)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("unknown subcommand"), "{e}");
        // Neither wrote anything.
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
    fn mcp_known_covers_all_layers() {
        let config = jyc_types::load_config_from_str(
            r#"
[ai]
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
        // A topic L3 overlay server (`.jyc/config.toml`) must count as known too.
        let tmp = TempDir::new().unwrap();
        let dir = jyc_types::state_dir::jyc_dir("t", tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "[[mcps]]\nname = \"l3_srv\"\ntype = \"local\"\ncommand = [\"true\"]\n",
        )
        .unwrap();
        let mut context = ctx(tmp.path().to_path_buf());
        context.config = Arc::new(config);
        assert!(mcp_known(&context, "global_srv"));
        assert!(mcp_known(&context, "agent_srv"));
        assert!(mcp_known(&context, "l3_srv"));
        assert!(!mcp_known(&context, "ghost"));
    }

    #[test]
    fn skill_known_finds_topic_dir_skill() {
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
        assert!(!skill_known(&context, "zz-absent-skill-9k7"));
        assert!(skill_known(&context, "ztest-skill"));
        assert!(!skill_known(&context, "../escape"));
    }
}
