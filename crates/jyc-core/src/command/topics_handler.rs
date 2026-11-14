//! /topics command — switch the dashboard's chat screen to another topic.
//!
//! The topic list is resolved server-side (the same live registry the
//! overview polls), but the action itself is a pure TUI switch: no agent
//! turn is started, so the handler's reply is a no-op marker the TUI
//! intercepts before it reaches the transcript. The command is registered
//! here so the `/` popup can offer the topic names as completions — the
//! same `ArgSource::Provided` mechanism `/send` uses for its agent list.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use super::spec;
use crate::topic_manager::TopicManager;

pub struct TopicsCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl TopicsCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }
}

#[async_trait]
impl CommandHandler for TopicsCommandHandler {
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let parsed = match spec::parse_plain("/topics", &context.args) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Ok(CommandResult {
                    success: false,
                    message: error.to_string(),
                    ..Default::default()
                });
            }
        };
        let Some(target) = parsed.positionals.first().map(String::as_str) else {
            return Ok(CommandResult {
                success: false,
                message: "usage: /topics <topic>".to_string(),
                ..Default::default()
            });
        };
        // Resolve the topic's channel from the live registry: the TUI needs
        // the `(channel, name)` pair to open the chat screen — a channel's
        // handler binds the topic from the URL, so the pair is what the open
        // path (and every later WS call) is keyed by.
        let hit = self
            .topic_manager
            .list_topics_without_git()
            .await
            .into_iter()
            .find(|t| t.name == target)
            .map(|t| (t.channel, t.name));
        match hit {
            Some((channel, name)) => Ok(CommandResult {
                success: true,
                // Machine-readable marker: the TUI intercepts this and
                // switches screens; the transcript never shows it.
                message: format!("\u{1}topics-switch\u{1}{channel}\u{1}{name}"),
                ..Default::default()
            }),
            None => Ok(CommandResult {
                success: false,
                message: format!("No topic named `{target}` is live"),
                ..Default::default()
            }),
        }
    }
}
