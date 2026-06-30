//! Builtin tool: `jyc_send_to_thread` — cross-thread/channel communication.
//!
//! Allows AI agents to inject messages into threads in other channels.
//! For example, an agent in a Feishu thread can generate a PDF and inject it
//! into an email channel's invoice_processing thread.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use tracing;

use crate::tools::{Tool, ToolContext, ToolOutput};
use jyc_types::{InboundMessage, MessageAttachment, MessageContent, PatternMatch};

/// Tool for sending messages to threads in other channels.
pub struct SendToThreadTool;

#[async_trait]
impl Tool for SendToThreadTool {
    fn name(&self) -> &str {
        "jyc_send_to_thread"
    }

    fn description(&self) -> &str {
        "Send a message to a thread in another channel. \
         Use this for cross-thread/channel communication, e.g. sending a \
         generated PDF to an invoice processing thread in another channel. \
         The target thread will be auto-created if it doesn't exist yet."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Target channel name, e.g. \"jin283\" or \"feishu_work\""
                },
                "thread": {
                    "type": "string",
                    "description": "Target thread name, e.g. \"invoice_processing\" or \"support\""
                },
                "message": {
                    "type": "string",
                    "description": "Message body to inject into the target thread"
                },
                "attachments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of filenames within the current thread directory to attach"
                },
                "recipient": {
                    "type": "string",
                    "description": "Optional recipient address/ID. Sets the sender_address on the injected message, enabling channel-appropriate reply routing"
                }
            },
            "required": ["channel", "thread", "message"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let channel = input
            .get("channel")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'channel' parameter"))?;

        let thread_name = input
            .get("thread")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'thread' parameter"))?;

        let message = input
            .get("message")
            .and_then(|m| m.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter"))?;

        let attachments: Option<Vec<String>> = input
            .get("attachments")
            .and_then(|a| serde_json::from_value(a.clone()).ok());

        let recipient = input.get("recipient").and_then(|r| r.as_str());

        // Validate required fields are non-empty
        if channel.trim().is_empty() {
            return Ok(ToolOutput::error("Channel cannot be empty"));
        }
        if thread_name.trim().is_empty() {
            return Ok(ToolOutput::error("Thread cannot be empty"));
        }
        if message.trim().is_empty() {
            return Ok(ToolOutput::error("Message cannot be empty"));
        }

        // Validate attachments (same logic as ReplyMessageTool)
        let validated_attachments = if let Some(ref filenames) = attachments {
            let mut valid = Vec::new();
            for filename in filenames {
                let file_path = ctx.working_dir.join(filename);
                if !file_path.exists() {
                    return Ok(ToolOutput::error(format!(
                        "Attachment not found: '{}'",
                        filename
                    )));
                }
                // Security: ensure within working directory
                if let Ok(canonical) = file_path.canonicalize() {
                    let working_canonical = ctx
                        .working_dir
                        .canonicalize()
                        .unwrap_or_else(|_| ctx.working_dir.to_path_buf());
                    if !canonical.starts_with(&working_canonical) {
                        return Ok(ToolOutput::error(format!(
                            "Attachment '{}' is outside the working directory",
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

        // Look up the target channel's ThreadManager
        let thread_managers = match ctx.thread_managers.as_ref() {
            Some(tm) => tm,
            None => {
                return Ok(ToolOutput::error(
                    "No thread managers available for cross-channel communication",
                ));
            }
        };

        let tm_map = thread_managers.lock().await;
        let target_tm = match tm_map.get(channel) {
            Some(tm) => tm.clone(),
            None => {
                return Ok(ToolOutput::error(format!(
                    "Channel '{}' not found. Available channels: {}",
                    channel,
                    tm_map
                        .keys()
                        .map(|k| format!("\"{}\"", k))
                        .collect::<Vec<_>>()
                        .join(", "),
                )));
            }
        };
        drop(tm_map);

        // Build InboundMessage with source metadata
        let inbound = InboundMessage {
            id: uuid::Uuid::new_v4().to_string(),
            channel: channel.to_string(),
            channel_uid: format!("jyc-send-to-thread-{}", uuid::Uuid::new_v4()),
            sender: "Agent".to_string(),
            sender_address: recipient.unwrap_or("agent@jyc").to_string(),
            recipients: vec![],
            topic: "Message from cross-thread tool".to_string(),
            content: MessageContent {
                text: Some(message.to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: validated_attachments
                .iter()
                .map(|filename| {
                    let file_path = ctx.working_dir.join(filename);
                    let size = std::fs::metadata(&file_path)
                        .map(|m| m.len() as usize)
                        .unwrap_or(0);
                    let file_bytes = std::fs::read(&file_path).ok();
                    MessageAttachment {
                        filename: filename.clone(),
                        content_type: "application/octet-stream".to_string(),
                        size,
                        content: file_bytes,
                        saved_path: None,
                    }
                })
                .collect(),
            metadata: {
                let mut m = HashMap::new();
                if let Some(ref src_ch) = ctx.current_channel {
                    m.insert(
                        "source_channel".to_string(),
                        serde_json::Value::String(src_ch.clone()),
                    );
                }
                if let Some(ref src_th) = ctx.current_thread {
                    m.insert(
                        "source_thread".to_string(),
                        serde_json::Value::String(src_th.clone()),
                    );
                }
                m
            },
            matched_pattern: None,
        };

        // Enqueue the message into the target thread
        let pattern_match = PatternMatch {
            pattern_name: String::new(),
            channel: channel.to_string(),
            matches: HashMap::new(),
        };

        target_tm
            .enqueue(
                inbound,
                thread_name.to_string(),
                pattern_match,
                None,
                true,
                None,
            )
            .await;

        let attachment_info = if validated_attachments.is_empty() {
            String::new()
        } else {
            format!(" with {} attachment(s)", validated_attachments.len())
        };

        tracing::info!(
            target_channel = %channel,
            target_thread = %thread_name,
            attachment_count = validated_attachments.len(),
            "Cross-thread message sent"
        );

        Ok(ToolOutput::success(format!(
            "Message sent to channel '{}', thread '{}'{}. The target thread will process it.",
            channel, thread_name, attachment_info
        )))
    }
}
