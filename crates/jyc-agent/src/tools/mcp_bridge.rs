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
use jyc_types::channel::{OutboundAttachment, OutboundMessage};

/// Send message tool — sends a message through the pre-warmed outbound
/// adapter.
///
/// This is the in-process equivalent of the `jyc_send_message` MCP tool.
/// It sends directly to arbitrary recipients for proactive alerts,
/// cross-channel messages, and replies that carry attachments; ordinary
/// text-only replies use the in-topic delivery path.
///
/// Supports:
/// - `channel`: optional target channel name for cross-channel messaging.
///   When omitted, uses the current channel's outbound adapter (backward compatible).
/// - `attachments`: optional list of file paths (relative to working directory)
///   to include as attachments. Supported by the channels whose adapter can
///   send files (email, feishu, wecom_bot); any other channel returns an error
///   when attachments are provided.
pub struct SendMessageTool;

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "jyc_send_message"
    }

    fn description(&self) -> &str {
        "Send a channel message with optional attachments. Use for proactive messages \
         and attachment replies; text replies are automatic."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": {
                    "type": "string",
                    "description": "Channel-specific recipient identifier (e.g. \"wecomkf:kf001:user123\")"
                },
                "subject": {
                    "type": "string",
                    "description": "Message subject (optional, channel-dependent)"
                },
                "message": {
                    "type": "string"
                },
                "channel": {
                    "type": "string",
                    "description": "Optional target channel name for cross-channel sending; \
                                     defaults to the current channel. The recipient format \
                                     must match that channel type."
                },
                "attachments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional file paths within the working directory to attach. \
                                     Supported by email, feishu and wecom_bot; every other \
                                     channel rejects the send."
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

        // The channel this message is addressed to: the caller's `channel`
        // parameter when it named one, else the channel the user actually wrote
        // on — a turn piped in from feishu runs on the `agents` hub, so the hub
        // channel alone would name the wrong one. The pipe stamps the origin
        // (see `ORIGIN_CHANNEL_METADATA_KEY`); empty leaves the adapter to
        // resolve it from its own pipe routing.
        let addressed_channel = match channel.as_deref() {
            Some(name) => name,
            None => ctx
                .reply_target
                .as_ref()
                .and_then(|target| {
                    target
                        .original
                        .metadata
                        .get(jyc_types::ORIGIN_CHANNEL_METADATA_KEY)
                        .and_then(|value| value.as_str())
                })
                .unwrap_or(""),
        };

        // Attachments live in this topic's directory. A channel relayed through
        // the pipe process downloads them from the inspect server's file
        // endpoint, which is addressed by the channel owning the topic and the
        // topic name — both known here and nowhere on the pipe side.
        let outbound_message = OutboundMessage {
            recipient,
            channel: addressed_channel,
            subject,
            body: message,
            attachments: attachment_objs.as_deref(),
            origin_channel: ctx.current_channel.as_deref().unwrap_or(""),
            origin_topic: ctx.current_topic.as_deref().unwrap_or(""),
        };

        match outbound.send_proactive(&outbound_message).await {
            Ok(result) => {
                let attachment_count = attachment_objs.as_ref().map_or(0, |atts| atts.len());
                tracing::info!(
                    recipient = %recipient,
                    attachment_count,
                    message_id = %result.message_id,
                    channel = ?channel,
                    "Proactive message sent"
                );
                let attachments_suffix = if attachment_count == 0 {
                    String::new()
                } else {
                    format!(" with {attachment_count} attachment(s)")
                };
                Ok(ToolOutput::success(format!(
                    "Message sent to '{}' (message_id: {}){attachments_suffix}",
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
