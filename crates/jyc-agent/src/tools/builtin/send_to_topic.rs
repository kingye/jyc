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
//! The topic has to exist: a name no topic owns is an error, not a new agent.
//! `jyc_spawn_subagent` is what makes one.
//!
//! `/send` parses its arguments into the same request and calls the same
//! `send_handler::send_to_topic` — one implementation, two triggers, as
//! `/spawn` and `jyc_spawn_subagent`.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tools::{Tool, ToolContext, ToolOutput};
use jyc_core::command::send_handler::{SendRequest, SendSource, send_to_topic_with_target};

/// Tool for sending messages to other agents and their topics.
pub struct SendToThreadTool;

#[async_trait]
impl Tool for SendToThreadTool {
    fn name(&self) -> &str {
        "jyc_send_to_topic"
    }

    fn description(&self) -> &str {
        "Send a message to another agent: its own topic by default, or one of its \
         topics with 'topic'. The topic must already exist — a name no topic owns is \
         an error, so create the agent first with jyc_spawn_subagent."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "The agent to reach, e.g. \"invoice\". It must already exist."
                },
                "topic": {
                    "type": "string",
                    "description": "One of that agent's topics, when it has more than one. A topic name is unique process-wide; an agent's own topic is named after the agent."
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
                    // The fallback — the framework forwards the turn's last
                    // message when the target never answers — is stated in
                    // the prompt's cross-topic section, not here: this schema
                    // rides in every request's cached prefix, and the prompt
                    // says it better ("an explicit reply is still better").
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

        match send_to_topic_with_target(managers, source, req).await {
            Ok(outcome) => {
                // Record where the message landed. A turn that answers a
                // `require_reply` request has to be recognizable as having
                // answered it, so the service does not forward the turn's
                // final words on top of a reply that already arrived.
                ctx.outbound_targets
                    .lock()
                    .expect("outbound_targets poisoned")
                    .push((outcome.channel, outcome.topic));
                Ok(ToolOutput::success(outcome.message))
            }
            Err(e) => Ok(ToolOutput::error(e.to_string())),
        }
    }
}
