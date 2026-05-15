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
        let message = input.get("message")
            .and_then(|m| m.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter"))?;

        let attachments: Option<Vec<String>> = input.get("attachments")
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
                        "Attachment not found: '{}'", filename
                    )));
                }
                // Security: ensure within thread directory
                if let Ok(canonical) = file_path.canonicalize() {
                    let thread_canonical = thread_path.canonicalize()
                        .unwrap_or_else(|_| thread_path.to_path_buf());
                    if !canonical.starts_with(&thread_canonical) {
                        return Ok(ToolOutput::error(format!(
                            "Attachment '{}' is outside thread directory", filename
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
        tokio::fs::write(jyc_dir.join("reply.md"), message).await
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
        ).await
            .map_err(|e| anyhow::anyhow!("Failed to write signal file: {e}"))?;

        tracing::info!(
            message_len = message.len(),
            attachments = validated_attachments.len(),
            "Reply signal written"
        );

        Ok(ToolOutput::success(format!(
            "Reply sent ({} chars). The monitor will deliver it.", message.len()
        )))
    }
}

/// Register MCP bridge tools into a tool registry.
pub fn register_mcp_tools(registry: &mut crate::tools::registry::ToolRegistry) {
    registry.register(Box::new(ReplyMessageTool));
    // TODO: Add VisionTool and QuestionTool bridges when needed
}
