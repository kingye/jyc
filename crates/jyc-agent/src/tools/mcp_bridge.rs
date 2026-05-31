//! MCP tool bridge — wraps JYC's MCP tools as in-process Tool trait objects.
//!
//! Instead of spawning MCP subprocesses, these bridge tools call the
//! same core logic directly within the agent process.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use tracing;

use crate::tools::{Tool, ToolContext, ToolOutput};

/// Reply message tool — writes reply for delivery by the monitor process.
///
/// This is the in-process equivalent of the `jyc_reply` MCP tool.
/// It writes `reply.md` and `reply-sent.flag` signal files that the
/// thread manager's delivery system picks up.
pub struct ReplyMessageTool;

#[async_trait]
impl Tool for ReplyMessageTool {
    fn name(&self) -> &str {
        "jyc_reply_reply_message"
    }

    fn description(&self) -> &str {
        "Send a reply message back through the originating channel. \
         The reply will be delivered by the monitor process. \
         After a successful reply, STOP immediately."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The reply text to send"
                },
                "attachments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of filenames within the thread directory to attach"
                }
            },
            "required": ["message"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let message = input
            .get("message")
            .and_then(|m| m.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter"))?;

        let attachments: Option<Vec<String>> = input
            .get("attachments")
            .and_then(|a| serde_json::from_value(a.clone()).ok());

        if message.trim().is_empty() {
            return Ok(ToolOutput::error("Message cannot be empty"));
        }

        let thread_path = ctx.working_dir;
        let jyc_dir = thread_path.join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.ok();

        // Validate attachments
        let validated_attachments = if let Some(ref filenames) = attachments {
            let mut valid = Vec::new();
            for filename in filenames {
                let file_path = thread_path.join(filename);
                if !file_path.exists() {
                    return Ok(ToolOutput::error(format!(
                        "Attachment not found: '{}'",
                        filename
                    )));
                }
                // Security: ensure within thread directory
                if let Ok(canonical) = file_path.canonicalize() {
                    let thread_canonical = thread_path
                        .canonicalize()
                        .unwrap_or_else(|_| thread_path.to_path_buf());
                    if !canonical.starts_with(&thread_canonical) {
                        return Ok(ToolOutput::error(format!(
                            "Attachment '{}' is outside thread directory",
                            filename
                        )));
                    }
                }
                valid.push(filename.clone());
            }
            valid
        } else {
            vec![]
        };

        // Write reply.md for background delivery watcher
        tokio::fs::write(jyc_dir.join("reply.md"), message)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write reply.md: {e}"))?;

        // Write signal file
        let signal = json!({
            "sent_at": chrono::Utc::now().to_rfc3339(),
            "message_len": message.len(),
            "attachment_count": validated_attachments.len(),
            "attachments": validated_attachments,
        });
        tokio::fs::write(
            jyc_dir.join("reply-sent.flag"),
            serde_json::to_string_pretty(&signal)?,
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to write signal file: {e}"))?;

        tracing::info!(
            message_len = message.len(),
            attachments = validated_attachments.len(),
            "Reply signal written"
        );

        Ok(ToolOutput::success(format!(
            "Reply sent ({} chars). The monitor will deliver it.",
            message.len()
        )))
    }
}

/// Send message tool — sends a proactive out-of-thread message via the
/// pre-warmed outbound adapter.
///
/// This is the in-process equivalent of the `jyc_send_message` MCP tool.
/// Unlike `ReplyMessageTool` which replies within the current thread, this
/// tool sends messages to arbitrary recipients for alerts and notifications.
pub struct SendMessageTool;

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "jyc_send_message"
    }

    fn description(&self) -> &str {
        "Send a proactive message to an arbitrary recipient. \
         Use ONLY for alerts and notifications — NEVER for in-thread replies. \
         The recipient format is channel-specific (e.g. \"wecomkf:kf001:user123\")."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": {
                    "type": "string",
                    "description": "Channel-specific recipient identifier"
                },
                "subject": {
                    "type": "string",
                    "description": "Message subject (optional, channel-dependent)"
                },
                "message": {
                    "type": "string",
                    "description": "The message body to send"
                }
            },
            "required": ["recipient", "message"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let recipient = input
            .get("recipient")
            .and_then(|r| r.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'recipient' parameter"))?;

        let subject = input.get("subject").and_then(|s| s.as_str()).unwrap_or("");

        let message = input
            .get("message")
            .and_then(|m| m.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter"))?;

        if recipient.trim().is_empty() {
            return Ok(ToolOutput::error("Recipient cannot be empty"));
        }

        if message.trim().is_empty() {
            return Ok(ToolOutput::error("Message cannot be empty"));
        }

        let outbound = match ctx.outbound.as_ref() {
            Some(o) => o,
            None => {
                return Ok(ToolOutput::error(
                    "No outbound adapter available for proactive messaging",
                ));
            }
        };

        match outbound.send_message(recipient, subject, message).await {
            Ok(result) => {
                tracing::info!(
                    recipient = %recipient,
                    message_id = %result.message_id,
                    "Proactive message sent"
                );
                Ok(ToolOutput::success(format!(
                    "Message sent to '{}' (message_id: {})",
                    recipient, result.message_id
                )))
            }
            Err(e) => {
                tracing::error!(
                    recipient = %recipient,
                    error = %e,
                    "Failed to send proactive message"
                );
                Ok(ToolOutput::error(format!(
                    "Failed to send message to '{}': {}",
                    recipient, e
                )))
            }
        }
    }
}

/// Register MCP bridge tools into a tool registry.
pub fn register_mcp_tools(registry: &mut crate::tools::registry::ToolRegistry) {
    registry.register(Box::new(ReplyMessageTool));
    registry.register(Box::new(SendMessageTool));
}
