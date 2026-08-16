//! Builtin tool: `jyc_send_to_topic` — cross-topic/channel communication.
//!
//! Allows AI agents to inject messages into topics in other channels.
//! For example, an agent in a Feishu topic can generate a PDF and inject it
//! into an email channel's invoice_processing topic.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use tracing;

use crate::tools::{Tool, ToolContext, ToolOutput};
use jyc_types::{InboundMessage, MessageAttachment, MessageContent, PatternMatch};

/// Tool for sending messages to topics in other channels.
pub struct SendToThreadTool;

#[async_trait]
impl Tool for SendToThreadTool {
    fn name(&self) -> &str {
        "jyc_send_to_topic"
    }

    fn description(&self) -> &str {
        "Send a message to a topic in another channel. \
         Use this for cross-topic/channel communication, e.g. sending a \
         generated PDF to an invoice processing topic in another channel. \
         The target topic will be auto-created if it doesn't exist yet. \
         Set require_reply=true to request the target agent to send results \
         back to the source topic."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "channel": {
                    "type": "string",
                    "description": "Target channel name, e.g. \"jin283\" or \"feishu_work\""
                },
                "topic": {
                    "type": "string",
                    "description": "Target topic name, e.g. \"invoice_processing\" or \"support\""
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
                    "description": "Whether to request the target agent to reply back with results. When true, the target agent will be instructed to use jyc_send_to_topic to send results back to the source channel/topic. Default: false."
                }
            },
            "required": ["channel", "topic", "message"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let channel = input
            .get("channel")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'channel' parameter"))?;

        let topic_name = input
            .get("topic")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'topic' parameter"))?;

        let message = input
            .get("message")
            .and_then(|m| m.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter"))?;

        let attachments: Option<Vec<String>> = input
            .get("attachments")
            .and_then(|a| serde_json::from_value(a.clone()).ok());

        let recipient = input.get("recipient").and_then(|r| r.as_str());

        let require_reply = input
            .get("require_reply")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Validate required fields are non-empty
        if channel.trim().is_empty() {
            return Ok(ToolOutput::error("Channel cannot be empty"));
        }
        if topic_name.trim().is_empty() {
            return Ok(ToolOutput::error("Topic cannot be empty"));
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

        // Look up the target channel's TopicManager
        let topic_managers = match ctx.topic_managers.as_ref() {
            Some(tm) => tm,
            None => {
                return Ok(ToolOutput::error(
                    "No topic managers available for cross-channel communication",
                ));
            }
        };

        let tm_map = topic_managers.lock().await;
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
        let mut inbound = InboundMessage {
            id: uuid::Uuid::new_v4().to_string(),
            channel: channel.to_string(),
            channel_uid: format!("jyc-send-to-topic-{}", uuid::Uuid::new_v4()),
            sender: "Agent".to_string(),
            sender_address: recipient.unwrap_or("agent@jyc").to_string(),
            recipients: vec![],
            topic: topic_name.to_string(),
            content: MessageContent {
                text: Some(message.to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            references: None,
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
                if let Some(ref src_th) = ctx.current_topic {
                    m.insert(
                        "source_topic".to_string(),
                        serde_json::Value::String(src_th.clone()),
                    );
                }
                m.insert(
                    "require_reply".to_string(),
                    serde_json::Value::Bool(require_reply),
                );
                m
            },
            matched_pattern: None,
        };

        // Enqueue the message into the target topic. Resolve the pattern
        // named after the topic (when one exists) so injected messages
        // carry the same pattern identity — name, template/role metadata,
        // attachment config, live_injection, custom topic_path — as
        // router-matched messages (mirrors MessageRouter, #542).
        let pattern = target_tm.pattern_for_topic(topic_name);
        let (pattern_name, attachment_config, live_injection, topic_path_override) = match &pattern
        {
            Some(p) => {
                if let Some(ref template) = p.template {
                    inbound
                        .metadata
                        .insert("template".to_string(), Value::String(template.clone()));
                }
                if let Some(ref role) = p.role {
                    inbound
                        .metadata
                        .insert("role".to_string(), Value::String(role.clone()));
                }
                // Mirror MessageRouter: metadata override > pattern topic_path
                // > agent dir (with lazy migration), so agent-routed topics
                // resolve identically for cross-topic injection (#577 review).
                let topic_path_override = jyc_core::topic_path::resolve_topic_path_override(
                    Some(p),
                    topic_name,
                    target_tm.data_root(),
                    channel,
                    inbound
                        .metadata
                        .get("topic_path_override")
                        .and_then(|v| v.as_str()),
                );
                (
                    p.name.clone(),
                    p.attachments.clone(),
                    p.live_injection,
                    topic_path_override,
                )
            }
            None => (String::new(), None, true, None),
        };
        let pattern_match = PatternMatch {
            pattern_name,
            channel: channel.to_string(),
            matches: HashMap::new(),
        };

        target_tm
            .enqueue(
                inbound,
                topic_name.to_string(),
                pattern_match,
                attachment_config,
                live_injection,
                topic_path_override,
            )
            .await;

        let attachment_info = if validated_attachments.is_empty() {
            String::new()
        } else {
            format!(" with {} attachment(s)", validated_attachments.len())
        };

        tracing::info!(
            target_channel = %channel,
            target_topic = %topic_name,
            attachment_count = validated_attachments.len(),
            require_reply,
            "Cross-topic message sent"
        );

        let reply_info = if require_reply {
            " (reply requested)"
        } else {
            ""
        };

        Ok(ToolOutput::success(format!(
            "Message sent to channel '{}', topic '{}'{}{}. The target topic will process it.",
            channel, topic_name, attachment_info, reply_info
        )))
    }
}
