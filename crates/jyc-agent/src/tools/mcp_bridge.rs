//! MCP tool bridge — wraps JYC's MCP tools as in-process Tool trait objects.
//!
//! Instead of spawning MCP subprocesses, these bridge tools call the
//! same core logic directly within the agent process.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing;

use crate::tools::{Tool, ToolContext, ToolOutput};
use jyc_types::channel::OutboundAttachment;

/// Reply message tool — writes reply for delivery by the monitor process.
///
/// This is the in-process equivalent of the `jyc_reply` MCP tool.
/// It writes `reply.md` and `reply-sent.flag` signal files that the
/// thread manager's delivery system picks up.
pub struct ReplyMessageTool;

#[async_trait]
impl Tool for ReplyMessageTool {
    fn name(&self) -> &str {
        "jyc_reply_message"
    }

    fn description(&self) -> &str {
        "Send a reply message back through the originating channel. \
         The reply will be delivered by the monitor process. \
         Use `stop_after: false` for progress/status updates where you intend to \
         continue working. Use `stop_after: true` (or omit it) for the final reply \
         — after a successful reply with stop_after=true, STOP immediately."
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
                },
                "stop_after": {
                    "type": "boolean",
                    "description": "Whether to stop working after this reply. Set to false for \
                     progress/status updates where you will continue working. \
                     Default: true (final reply, stop immediately)."
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

        let stop_after = input
            .get("stop_after")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

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

        if stop_after {
            Ok(ToolOutput::success(format!(
                "Reply sent ({} chars). STOP NOW — do not call any more tools.",
                message.len()
            )))
        } else {
            Ok(ToolOutput::success_continue(format!(
                "Progress update sent ({} chars). Continue working.",
                message.len()
            )))
        }
    }
}

/// Send message tool — sends a proactive out-of-thread message via the
/// pre-warmed outbound adapter.
///
/// This is the in-process equivalent of the `jyc_send_message` MCP tool.
/// Unlike `ReplyMessageTool` which replies within the current thread, this
/// tool sends messages to arbitrary recipients for alerts and notifications.
///
/// Supports:
/// - `channel`: optional target channel name for cross-channel messaging.
///   When omitted, uses the current channel's outbound adapter (backward compatible).
/// - `attachments`: optional list of file paths (relative to working directory)
///   to include as attachments. Only supported by channels with attachment capability
///   (e.g. email). Non-email channels return an error if attachments are provided.
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
                },
                "channel": {
                    "type": "string",
                    "description": "Optional target channel name for cross-channel sending. \
                                     When omitted, uses the current channel. \
                                     The recipient format must match the target channel type."
                },
                "attachments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of file paths within the working directory to attach. \
                                     Only supported by email channels."
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

        let channel: Option<String> = input
            .get("channel")
            .and_then(|c| c.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let attachment_filenames: Option<Vec<String>> = input
            .get("attachments")
            .and_then(|a| serde_json::from_value(a.clone()).ok());

        if recipient.trim().is_empty() {
            return Ok(ToolOutput::error("Recipient cannot be empty"));
        }

        if message.trim().is_empty() {
            return Ok(ToolOutput::error("Message cannot be empty"));
        }

        // Determine which outbound adapter to use
        let outbound: Arc<dyn jyc_types::channel::OutboundAdapter> = if let Some(ref ch) = channel {
            // Cross-channel: look up from outbounds map
            let outbounds_map = match ctx.outbounds.as_ref() {
                Some(m) => m,
                None => {
                    return Ok(ToolOutput::error(format!(
                        "Cross-channel messaging is not available: no outbounds map configured. \
                             Cannot send to channel '{}'",
                        ch
                    )));
                }
            };
            let map = outbounds_map.lock().await;
            match map.get(ch) {
                Some(o) => o.clone(),
                None => {
                    return Ok(ToolOutput::error(format!(
                        "Unknown channel '{}'. Available channels: {}",
                        ch,
                        map.keys().cloned().collect::<Vec<_>>().join(", ")
                    )));
                }
            }
        } else {
            // Same-channel: use current outbound adapter
            match ctx.outbound.as_ref() {
                Some(o) => o.clone(),
                None => {
                    return Ok(ToolOutput::error(
                        "No outbound adapter available for proactive messaging",
                    ));
                }
            }
        };

        // Process attachments if provided
        let attachment_objs: Option<Vec<OutboundAttachment>> =
            if let Some(ref filenames) = attachment_filenames {
                if filenames.is_empty() {
                    None
                } else {
                    let mut atts = Vec::new();
                    for filename in filenames {
                        let file_path = ctx.working_dir.join(filename);

                        // Security: ensure within working directory (canonical prefix check)
                        if !file_path.exists() {
                            return Ok(ToolOutput::error(format!(
                                "Attachment not found: '{}'",
                                filename
                            )));
                        }
                        if let Ok(canonical) = file_path.canonicalize()
                            && let Err(msg) = ctx.check_path_boundary(filename, &canonical)
                        {
                            return Ok(ToolOutput::error(msg));
                        }

                        // Determine content type from extension (simple mapping)
                        let content_type = detect_content_type(filename);

                        atts.push(OutboundAttachment {
                            filename: filename.clone(),
                            path: file_path,
                            content_type,
                        });
                    }
                    Some(atts)
                }
            } else {
                None
            };

        // Send the message
        if let Some(ref atts) = attachment_objs {
            match outbound
                .send_message_with_attachments(recipient, subject, message, Some(atts))
                .await
            {
                Ok(result) => {
                    tracing::info!(
                        recipient = %recipient,
                        attachment_count = atts.len(),
                        message_id = %result.message_id,
                        channel = ?channel,
                        "Proactive message with attachments sent"
                    );
                    Ok(ToolOutput::success(format!(
                        "Message sent to '{}' (message_id: {}) with {} attachment(s)",
                        recipient,
                        result.message_id,
                        atts.len()
                    )))
                }
                Err(e) => {
                    tracing::error!(
                        recipient = %recipient,
                        error = %e,
                        "Failed to send proactive message with attachments"
                    );
                    Ok(ToolOutput::error(format!(
                        "Failed to send message to '{}': {}",
                        recipient, e
                    )))
                }
            }
        } else {
            match outbound.send_message(recipient, subject, message).await {
                Ok(result) => {
                    tracing::info!(
                        recipient = %recipient,
                        message_id = %result.message_id,
                        channel = ?channel,
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
}

/// Register MCP bridge tools into a tool registry.
pub fn register_mcp_tools(registry: &mut crate::tools::registry::ToolRegistry) {
    registry.register(Box::new(ReplyMessageTool));
    registry.register(Box::new(SendMessageTool));
}

/// Simple content type detection from file extension.
/// Falls back to `application/octet-stream` for unknown extensions.
fn detect_content_type(filename: &str) -> String {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf".to_string(),
        "png" => "image/png".to_string(),
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "gif" => "image/gif".to_string(),
        "webp" => "image/webp".to_string(),
        "svg" => "image/svg+xml".to_string(),
        "csv" => "text/csv".to_string(),
        "json" => "application/json".to_string(),
        "xml" => "application/xml".to_string(),
        "zip" => "application/zip".to_string(),
        "txt" => "text/plain".to_string(),
        "html" | "htm" => "text/html".to_string(),
        "md" => "text/markdown".to_string(),
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
        "docx" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document".to_string()
        }
        _ => "application/octet-stream".to_string(),
    }
}
