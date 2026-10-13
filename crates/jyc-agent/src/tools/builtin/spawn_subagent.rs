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
//! Two optional flags decide what the child *starts* from, and both default to
//! the lazier, more independent choice: `inherit_context` copies this topic's
//! conversation and transcript (off — a subagent given a task of its own does
//! not need them, and they cost tokens every turn), and `share_topic_path` puts
//! the child in **this** topic's working directory instead of one of its own
//! (off — for a reviewer that has to read uncommitted work, which no separate
//! copy would show).

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
        "Create a subagent topic — a sibling topic that inherits this topic's settings (task list, \
         mode, pattern) and runs on its own queue — and grant yourself read+write access to its \
         topic_path, the working directory it runs in. Its conversation starts empty: pass \
         inherit_context=true only when the child must continue this conversation. Pass \
         share_topic_path=true to have it work in *your* directory (a reviewer of uncommitted \
         work needs that; its own state still goes to its own state dir). Put whatever it needs \
         into that directory with `write`, then send the task with `jyc_send_to_topic` \
         (require_reply=true). To reconfigure it, send it a command body (`/build`, `/plan`, \
         `/model`, …), which runs without spending one of its turns. Subagents are one level deep \
         and capped at three per topic; when the subagent list in your system prompt already \
         holds one that fits, continue it instead of spawning another."
    }

    fn input_schema(&self) -> Value {
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
                                    Default false: it starts with your settings but no history."
                },
                "share_topic_path": {
                    "type": "boolean",
                    "description": "Let the subagent work in your own directory, for work that has \
                                    to see your files as they are (a review of uncommitted \
                                    changes). Default false: it gets a directory of its own."
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
        let inherit_context = input
            .get("inherit_context")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let share_topic_path = input
            .get("share_topic_path")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        match spawn_handler::spawn_subagent(
            &tm,
            &parent,
            ctx.working_dir,
            name,
            inherit_context,
            share_topic_path,
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
        // Both start-from decisions are the caller's to make, and both default
        // to off — the schema is where the model reads that.
        for flag in ["inherit_context", "share_topic_path"] {
            assert_eq!(
                SpawnSubagentTool.input_schema()["properties"][flag]["type"],
                "boolean",
                "{flag}"
            );
        }
    }
}
