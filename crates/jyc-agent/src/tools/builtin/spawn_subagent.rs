//! Builtin tool: `jyc_spawn_subagent` — create a subagent topic for this agent.
//!
//! Registered only on the synthesized `agents` channel (see
//! `JycAgentService::build_tool_registry`), where a spawned sibling topic is
//! addressable. The tool is the agent's trigger for the one implementation
//! `/spawn` also runs (`jyc_core::command::spawn_handler::spawn_subagent`):
//! same inheritance of this topic's state dir, same lineage breadcrumb, same
//! read+write grant on the child's topic_path. Seeding that topic_path and
//! sending the first task stay with the caller — `write` for the former,
//! `jyc_send_to_topic` for the latter — and so does reconfiguring the child,
//! which is a command away (`/build`, `/model`, … sent as a message body).
//!
//! Three optional parameters decide what the child *starts* from. Two are flags
//! that default to the lazier, more independent choice: `inherit_context` copies
//! this topic's conversation and transcript (off — a subagent given a task of its
//! own does not need them, and they cost tokens every turn), and
//! `share_topic_path` puts the child in **this** topic's working directory
//! instead of one of its own (off — for a reviewer that has to read uncommitted
//! work, which no separate copy would show). The third, `type`, names a role
//! (`[subagents.<type>]` in the config, or `<config dir>/subagents/<type>/
//! config.toml`) whose settings the child starts with — its model, skills,
//! disabled tools, mode and instructions, written into the child's own state at
//! spawn time. A role's own file may also state how it is spawned (`[spawn]`,
//! defaults for the two flags) and describe itself in one line; both are listed
//! in this tool's `type` argument, so an agent that was never told about a role
//! can still recognize when to reach for it — and an explicit argument here
//! still overrides the role's default.

use anyhow::Result;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::tools::{Tool, ToolContext, ToolOutput};
use jyc_core::command::spawn_handler;
use jyc_types::AppConfig;

/// Tool for spawning a subagent topic that inherits this topic's state.
pub struct SpawnSubagentTool {
    /// The service's shared config, so a `type` is resolved against the config
    /// this instance is actually running with (`--config <FILE>` included, and
    /// config reloads applied) — the same ArcSwap `build_tool_registry` reads
    /// its MCP and pattern settings from.
    pub config: Arc<ArcSwap<AppConfig>>,
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        "jyc_spawn_subagent"
    }

    fn description(&self) -> &str {
        "Create a subagent topic — a sibling topic that inherits this topic's settings (mode, \
         pattern, and the runtime overrides you do not pin with `type`), starts on your model for \
         the mode it is in — the one you set for that mode, else the model you are running — \
         unless `type` names one, starts with an empty task list, and runs on its own \
         queue — and grant yourself read+write access to its \
         topic_path, the working directory it runs in. Its conversation starts empty: pass \
         inherit_context=true only when the child must continue this conversation. Pass \
         share_topic_path=true to have it work in *your* directory (a reviewer of uncommitted \
         work needs that; its own state still goes to its own state dir). Pass type=\"<name>\" to \
         start it as a configured role — the settings under that name (its model, skill \
         allow-list, disabled tools, mode, instructions) are written into the child's state, so \
         e.g. the `review` type starts a child in plan mode with only the review skills and \
         without `write`/`edit`; the types you can pass are listed in this tool's `type` \
         argument, and a role may bring its own defaults for the two flags above (the `review` \
         role shares your directory without being asked). Put whatever it needs into that \
         directory with `write`, then send \
         the task with `jyc_send_to_topic` (require_reply=true). To reconfigure it, send it a \
         command body (`/build`, `/plan`, `/model`, …), which runs without spending one of its \
         turns. When the subagent \
         list in your system prompt already holds one that fits, continue it instead of spawning \
         another."
    }

    fn input_schema(&self) -> Value {
        let catalog = spawn_handler::type_catalog(
            &self.config.load(),
            jyc_utils::paths::config_dir().as_deref(),
        );
        let type_line = if catalog.is_empty() {
            "No types are configured on this machine.".to_string()
        } else {
            format!("Available types: {}.", catalog.join("; "))
        };
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Topic name for the subagent. Default: <your topic>-<N>."
                },
                "inherit_context": {
                    "type": "boolean",
                    "description": "Copy your conversation and transcript into the subagent. \
                                    Default false: it starts with your settings but no history. \
                                    A type may set its own default here; passing this overrides it."
                },
                "share_topic_path": {
                    "type": "boolean",
                    "description": "Let the subagent work in your own directory, for work that has \
                                    to see your files as they are (a review of uncommitted \
                                    changes). Default false: it gets a directory of its own. \
                                    A type may set its own default here; passing this overrides it."
                },
                "type": {
                    "type": "string",
                    "description": format!(
                        "Optional subagent type: the child starts with that role's \
                         settings (model, skills, disabled tools, mode, instructions), taken \
                         from [subagents.<type>] in the config or \
                         <config dir>/subagents/<type>/config.toml — and, when that file says \
                         so, its own defaults for the two flags above. Default: no type — the \
                         child inherits this topic's settings. {type_line}"
                    )
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let Some(parent) = ctx.current_topic.clone() else {
            return Ok(ToolOutput::error(
                "jyc_spawn_subagent needs a topic context; use it from a topic, not a \
                 standalone agent.",
            ));
        };
        // The role is resolved first, and against the live config: an unknown or
        // malformed type is refused before a directory exists, and before this
        // context is asked for anything else it may lack.
        let type_name = input
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let subagent_type = match spawn_handler::resolve_type_arg(type_name, &self.config.load()) {
            Ok(resolved) => resolved,
            Err(refusal) => return Ok(ToolOutput::error(format!("jyc_spawn_subagent: {refusal}"))),
        };
        let (Some(channel), Some(managers)) =
            (ctx.current_channel.clone(), ctx.topic_managers.as_ref())
        else {
            return Ok(ToolOutput::error(
                "jyc_spawn_subagent is unavailable in this context: no channel topic managers.",
            ));
        };
        let Some(tm) = managers.by_channel(&channel) else {
            return Ok(ToolOutput::error(format!(
                "no topic manager registered for channel '{channel}'."
            )));
        };

        let name = input.get("name").and_then(|v| v.as_str());
        // `None` = the caller said nothing, which is what lets a type's own
        // `[spawn]` defaults speak — and an explicit argument override them.
        let inherit_context = input.get("inherit_context").and_then(|v| v.as_bool());
        let share_topic_path = input.get("share_topic_path").and_then(|v| v.as_bool());
        let (inherit_context, share_topic_path) = spawn_handler::type_start_flags(
            subagent_type.as_ref(),
            inherit_context,
            share_topic_path,
        );
        // The child starts on this topic's model, unless the role names one of
        // its own: resolved with the same live config the type was, so a
        // `--config` file and config reloads are honored here too.
        let start_model = jyc_core::session_state::resolve_effective_model(
            &parent,
            ctx.working_dir,
            &self.config.load(),
            &channel,
        )
        .await;
        match spawn_handler::spawn_subagent(
            &tm,
            &parent,
            ctx.working_dir,
            name,
            inherit_context,
            share_topic_path,
            subagent_type.as_ref(),
            start_model.as_deref(),
        )
        .await
        {
            Err(refusal) => Ok(ToolOutput::error(refusal)),
            Ok(child) => {
                let dir = tm
                    .topic_path(&child)
                    .await
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| child.clone());
                let where_to_write = if share_topic_path {
                    format!("your own directory {dir}")
                } else {
                    dir.clone()
                };
                Ok(ToolOutput::success(format!(
                    "Spawned subagent '{child}' at {dir} — you hold read+write access there. Put \
                     its inputs in {where_to_write} with `write`, then send the task with \
                     jyc_send_to_topic (channel \"{channel}\", topic \"{child}\", \
                     require_reply=true)."
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tool as the service registers it: what the tests exercise is the
    /// argument handling, not any configured type.
    fn tool() -> SpawnSubagentTool {
        SpawnSubagentTool {
            config: Arc::new(ArcSwap::from_pointee(AppConfig::default())),
        }
    }

    /// Without a channel context there is nothing to resolve, and the tool says
    /// so instead of panicking.
    #[tokio::test]
    async fn refuses_without_channel_context() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(tmp.path());
        let out = tool().execute(json!({}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("topic context"), "{}", out.content);
    }

    /// A type that nothing defines is refused before any topic is created, and
    /// the refusal comes from the tool (which holds the live config) rather
    /// than from the core it calls.
    #[tokio::test]
    async fn refuses_an_unknown_type_before_spawning() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(tmp.path());
        ctx.current_topic = Some("parent".to_string());
        let out = tool()
            .execute(json!({"type": "no-such-role"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("unknown subagent type 'no-such-role'"),
            "{}",
            out.content
        );
    }

    /// A type defined in the config *this instance is running with* resolves — the
    /// tool reads the service's live `ArcSwap`, not the default config path, so a
    /// `--config <FILE>` (or a reloaded config) is what decides. The spawn then
    /// proceeds to the next missing piece of context instead of refusing the
    /// type, which is what a disk read would do here.
    #[tokio::test]
    async fn resolves_a_type_from_the_live_config() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(tmp.path());
        ctx.current_topic = Some("parent".to_string());
        let config = jyc_types::load_config_from_str(
            "[ai]\nenabled = true\n[subagents.review]\nmode = \"plan\"\n",
        )
        .unwrap();
        let tool = SpawnSubagentTool {
            config: Arc::new(ArcSwap::from_pointee(config)),
        };

        let out = tool.execute(json!({"type": "review"}), &ctx).await.unwrap();

        assert!(out.is_error, "no channel context yet: {}", out.content);
        assert!(
            !out.content.contains("unknown subagent type"),
            "the type came from the live config, so it is known: {}",
            out.content
        );
        assert!(out.content.contains("topic managers"), "{}", out.content);
    }

    #[test]
    fn name_and_schema_are_stable() {
        assert_eq!(tool().name(), "jyc_spawn_subagent");
        assert_eq!(
            tool().input_schema()["properties"]["name"]["type"],
            "string"
        );
        // Both start-from decisions are the caller's to make, and both default
        // to off — the schema is where the model reads that.
        for flag in ["inherit_context", "share_topic_path"] {
            assert_eq!(
                tool().input_schema()["properties"][flag]["type"],
                "boolean",
                "{flag}"
            );
        }
        // The role is opt-in too, and its absence is the documented default
        // (inherit this topic's settings).
        assert_eq!(
            tool().input_schema()["properties"]["type"]["type"],
            "string"
        );
    }
}
