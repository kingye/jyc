//! Builtin tool: `jyc_send_to_topic` — send a message to another agent.
//!
//! Addressing is by agent, not by channel: a message goes to the agent's own
//! topic by default, or to one of its topics with `topic`. For example, an
//! agent in a Feishu topic can generate a PDF and send it to the
//! `invoice_processing` topic of the invoice agent.
//!
//! An agent's own topic is named after the agent, so `topic` carrying the
//! agent's own name addresses that same default topic — the two spellings
//! mean one thing.
//!
//! `/send` parses its arguments into the same request and calls the same
//! `send_handler::send_to_topic` — one implementation, two triggers, as
//! `/spawn` and `jyc_spawn_subagent`.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tools::{Tool, ToolContext, ToolOutput};
use jyc_core::command::send_handler::{SendRequest, SendSource, send_to_topic};

/// Tool for sending messages to other agents and their topics.
pub struct SendToThreadTool;

#[async_trait]
impl Tool for SendToThreadTool {
    fn name(&self) -> &str {
        "jyc_send_to_topic"
    }

    fn description(&self) -> &str {
        "Send a message to another agent: its own topic by default, or one of its \
         topics with 'topic' — a name an existing topic holds goes there, a name no topic \
         owns becomes that agent's own topic, created on demand. Set require_reply=true to \
         have the target send its results back to this topic."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "The agent to reach, e.g. \"invoice\". Matched against topic names first; a name no topic owns becomes that agent's own topic."
                },
                "topic": {
                    "type": "string",
                    "description": "One of that agent's topics, when it has more than one; a topic name is unique process-wide. An agent's own topic is named after the agent, so the same name here addresses that default topic."
                },
                "message": {
                    "type": "string",
                    "description": "Message body to inject into the target topic"
                },
                "attachments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of filenames within the current topic directory to attach"
                },
                "recipient": {
                    "type": "string",
                    "description": "Optional recipient address/ID; sets the sender_address for channel-appropriate reply routing."
                },
                "require_reply": {
                    "type": "boolean",
                    "description": "Ask the target agent to send its results back to this topic; it then uses jyc_send_to_topic. Default false."
                }
            },
            "required": ["agent", "message"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let agent = input.get("agent").and_then(|v| v.as_str()).unwrap_or("");
        let topic = input.get("topic").and_then(|v| v.as_str());
        let message = input.get("message").and_then(|v| v.as_str()).unwrap_or("");
        let attachments: Vec<String> = input
            .get("attachments")
            .and_then(|a| serde_json::from_value(a.clone()).ok())
            .unwrap_or_default();
        let recipient = input.get("recipient").and_then(|v| v.as_str());
        let require_reply = input
            .get("require_reply")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let Some(managers) = ctx.topic_managers.as_ref() else {
            return Ok(ToolOutput::error(
                "No topic managers available for cross-topic communication",
            ));
        };

        let source = SendSource {
            channel: ctx.current_channel.as_deref(),
            topic: ctx.current_topic.as_deref(),
            working_dir: ctx.working_dir,
        };
        let req = SendRequest {
            agent,
            topic,
            message,
            attachments: &attachments,
            recipient,
            require_reply,
        };

        match send_to_topic(managers, source, req).await {
            Ok(message) => Ok(ToolOutput::success(message)),
            Err(e) => Ok(ToolOutput::error(e.to_string())),
        }
    }
}
