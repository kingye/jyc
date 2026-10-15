//! Builtin tool: `jyc_send_to_topic` — send a message to another agent.
//!
//! Addressing is by agent, not by channel: a message goes to the agent's own
//! topic by default, or to one of its topics with `topic`. For example, an
//! agent in a Feishu topic can generate a PDF and send it to the
//! `invoice_processing` topic of the invoice agent.
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
        "Send a message to another agent: its own topic by default, or one of \
         its topics with 'topic'. An agent's own topic is created if it doesn't \
         exist yet. Set require_reply=true to request the target agent to send \
         results back to the source topic."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Target agent name, e.g. \"invoice\" or \"mail-bot\""
                },
                "topic": {
                    "type": "string",
                    "description": "One of that agent's topics, when it has more than one, e.g. \"invoice_processing\". Omit it to address the agent's own topic."
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
                    "description": "Optional recipient address/ID. Sets the sender_address on the injected message, enabling channel-appropriate reply routing"
                },
                "require_reply": {
                    "type": "boolean",
                    "description": "Whether to request the target agent to reply back with results. When true, the target agent will be instructed to use jyc_send_to_topic to send results back to the source topic. Default: false."
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
