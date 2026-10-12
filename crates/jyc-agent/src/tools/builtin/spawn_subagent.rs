//! Builtin tool: `jyc_spawn_subagent` — create a subagent topic for this agent.
//!
//! Registered only on the synthesized `agents` channel (see
//! `JycAgentService::build_tool_registry`), where a spawned sibling topic is
//! addressable. The tool is the agent's trigger for the one implementation
//! `/spawn` also runs (`jyc_core::command::spawn_handler::spawn_subagent`):
//! same inheritance of this topic's state, same lineage breadcrumb, same
//! read+write grant on the child's directory. Seeding that directory and
//! sending the first task stay with the caller — `write` for the former,
//! `jyc_send_to_topic` for the latter — and so does reconfiguring the child,
//! which is a command away (`/build`, `/model`, … sent as a message body).

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tools::{Tool, ToolContext, ToolOutput};
use jyc_core::command::spawn_handler;

/// Tool for spawning a subagent topic that inherits this topic's state.
pub struct SpawnSubagentTool;

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        "jyc_spawn_subagent"
    }

    fn description(&self) -> &str {
        "Create a subagent topic — a sibling topic that inherits a copy of this topic's state \
         (conversation, task list, mode, pattern) and runs on its own queue — and grant \
         yourself read+write access to its directory. Put whatever it needs into that \
         directory with `write`, then send the task with `jyc_send_to_topic` \
         (require_reply=true). To reconfigure it, send it a command body (`/build`, `/plan`, \
         `/model`, …), which runs without spending one of its turns. Subagents are one level \
         deep and capped at three per topic; when the subagent list in your system prompt \
         already holds one that fits, continue it instead of spawning another."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Topic name for the subagent. Default: <your topic>-<N>."
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
        let (Some(channel), Some(managers)) =
            (ctx.current_channel.clone(), ctx.topic_managers.as_ref())
        else {
            return Ok(ToolOutput::error(
                "jyc_spawn_subagent is unavailable in this context: no channel topic managers.",
            ));
        };
        let Some(tm) = managers.lock().await.get(&channel).cloned() else {
            return Ok(ToolOutput::error(format!(
                "no topic manager registered for channel '{channel}'."
            )));
        };

        let name = input.get("name").and_then(|v| v.as_str());
        match spawn_handler::spawn_subagent(&tm, &parent, ctx.working_dir, name).await {
            Err(refusal) => Ok(ToolOutput::error(refusal)),
            Ok(child) => {
                let dir = tm
                    .topic_path(&child)
                    .await
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| child.clone());
                let children = spawn_handler::subagents_of(&parent);
                Ok(ToolOutput::success(format!(
                    "Spawned subagent '{child}' at {dir} — you hold read+write access to it, \
                     and it inherits this topic's state (a copy of the conversation, the task \
                     list, the mode, the pattern) as it is now.\n\
                     Put its inputs in that directory with `write`, then send it the task with \
                     jyc_send_to_topic (channel \"{channel}\", topic \"{child}\", \
                     require_reply=true). Send it a command body (`/build`, `/model`, …) to \
                     reconfigure it. Do not touch its `.jyc` directory.\n\
                     Your subagents ({} of at most {}): {}. Continue an existing one instead \
                     of spawning a duplicate.",
                    children.len(),
                    spawn_handler::MAX_SUBAGENTS,
                    spawn_handler::format_subagent_names(&children)
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a channel context there is nothing to resolve, and the tool says
    /// so instead of panicking.
    #[tokio::test]
    async fn refuses_without_channel_context() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(tmp.path());
        let out = SpawnSubagentTool.execute(json!({}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("topic context"), "{}", out.content);
    }

    #[test]
    fn name_and_schema_are_stable() {
        assert_eq!(SpawnSubagentTool.name(), "jyc_spawn_subagent");
        assert_eq!(
            SpawnSubagentTool.input_schema()["properties"]["name"]["type"],
            "string"
        );
    }
}
