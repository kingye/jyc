//! `list_topics` — the addressing table, on demand.
//!
//! The channels and their active topics used to ride in every turn's
//! `<runtime-state>` block: a static list that changed whenever a topic
//! appeared or closed, repeated in the history of every later turn. It is
//! only needed when the model wants to address someone, so it lives behind
//! the extended group and is fetched when it is wanted.
//!
//! The output keeps the shape the runtime-state block had, so a model that
//! saw the old list reads this one the same way.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolOutput};

/// List the channels and their active topics.
pub struct ListTopicsTool;

#[async_trait]
impl Tool for ListTopicsTool {
    fn name(&self) -> &str {
        "list_topics"
    }

    fn description(&self) -> &str {
        "List the channels and their active topics, for addressing `jyc_send_to_topic`. \
         Use it when you need a channel or topic name you do not know; the names it returns \
         are the ones the tool accepts."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let Some(managers) = ctx.topic_managers.as_ref() else {
            return Ok(ToolOutput::success(
                "No channels are reachable from this topic.",
            ));
        };

        let mut out = String::new();
        for tm in managers.all().iter() {
            out.push_str(&format!(
                "- Channel \"{}\" ({})\n",
                tm.channel_name(),
                tm.channel_type()
            ));
            let topics = tm.list_topic_names().await;
            if topics.is_empty() {
                out.push_str("    (no active topics)\n");
            } else {
                for name in &topics {
                    out.push_str(&format!("  - {name}\n"));
                }
            }
        }
        if out.is_empty() {
            return Ok(ToolOutput::success(
                "No channels are reachable from this topic.",
            ));
        }
        Ok(ToolOutput::success(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_core::topic_manager::TopicManagers;
    use std::path::Path;
    use std::sync::Arc;

    fn ctx(path: &Path) -> ToolContext<'_> {
        ToolContext::new(path)
    }

    /// Without any channel there is nothing to list, and the model is told
    /// so rather than handed an empty answer.
    #[tokio::test]
    async fn reports_when_no_channel_is_reachable() {
        let tool = ListTopicsTool;
        let dir = tempfile::tempdir().unwrap();
        let out = tool.execute(json!({}), &ctx(dir.path())).await.unwrap();
        assert!(out.content.contains("No channels"), "{}", out.content);
    }

    /// An empty manager set is the same story as no manager at all.
    #[tokio::test]
    async fn reports_when_the_manager_set_is_empty() {
        let tool = ListTopicsTool;
        let dir = tempfile::tempdir().unwrap();
        let mut context = ctx(dir.path());
        context.topic_managers = Some(Arc::new(TopicManagers::new()));
        let out = tool.execute(json!({}), &context).await.unwrap();
        assert!(out.content.contains("No channels"), "{}", out.content);
    }
}
