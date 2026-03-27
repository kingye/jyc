use anyhow::{bail, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::channels::types::InboundMessage;
use crate::channels::types::MessageContent;

/// Reply context — metadata passed opaquely through the AI to the reply tool.
///
/// Serialized as JSON → base64url. The AI passes it unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyContext {
    pub channel: String,
    #[serde(rename = "threadName")]
    pub thread_name: String,
    pub sender: String,
    pub recipient: String,
    pub topic: String,
    pub timestamp: String,
    #[serde(rename = "incomingMessageDir")]
    pub incoming_message_dir: Option<String>,
    #[serde(rename = "externalId")]
    pub external_id: Option<String>,
    #[serde(rename = "threadRefs")]
    pub thread_refs: Option<Vec<String>>,
    pub uid: String,
    #[serde(rename = "_nonce")]
    pub nonce: Option<String>,
    #[serde(rename = "channelMetadata")]
    pub channel_metadata: Option<HashMap<String, serde_json::Value>>,
}

/// Deserialize and validate a reply context token.
///
/// base64url → JSON → ReplyContext with integrity checks.
pub fn deserialize_context(encoded: &str) -> Result<ReplyContext> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|e| anyhow::anyhow!("invalid base64 token: {e}"))?;

    let json =
        String::from_utf8(bytes).map_err(|e| anyhow::anyhow!("invalid UTF-8 in token: {e}"))?;

    // Check for tampering indicators
    if json.contains('`') || json.contains("\\n") || json.contains("\\\"") {
        bail!("token appears modified — DO NOT decode or modify the token");
    }

    let ctx: ReplyContext =
        serde_json::from_str(&json).map_err(|e| anyhow::anyhow!("invalid JSON in token: {e}"))?;

    // Validate required fields
    if ctx.channel.is_empty() {
        bail!("missing required field: channel");
    }
    if ctx.recipient.is_empty() {
        bail!("missing required field: recipient");
    }

    Ok(ctx)
}

/// Reconstruct a minimal InboundMessage from a ReplyContext.
///
/// Used by the reply tool to call OutboundAdapter.send_reply().
pub fn context_to_inbound_message(ctx: &ReplyContext) -> InboundMessage {
    InboundMessage {
        id: uuid::Uuid::new_v4().to_string(),
        channel: ctx.channel.clone(),
        channel_uid: ctx.uid.clone(),
        sender: ctx.sender.clone(),
        sender_address: ctx.recipient.clone(),
        recipients: vec![],
        topic: ctx.topic.clone(),
        content: MessageContent::default(),
        timestamp: chrono::Utc::now(),
        thread_refs: ctx.thread_refs.clone(),
        reply_to_id: None,
        external_id: ctx.external_id.clone(),
        attachments: vec![],
        metadata: ctx.channel_metadata.clone().unwrap_or_default(),
        matched_pattern: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_token(json: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
    }

    #[test]
    fn test_deserialize_valid() {
        let json = r#"{"channel":"email","threadName":"test","sender":"John","recipient":"john@example.com","topic":"Test","timestamp":"2026-03-27","uid":"42"}"#;
        let token = make_token(json);
        let ctx = deserialize_context(&token).unwrap();
        assert_eq!(ctx.channel, "email");
        assert_eq!(ctx.recipient, "john@example.com");
        assert_eq!(ctx.uid, "42");
    }

    #[test]
    fn test_deserialize_missing_channel() {
        let json = r#"{"channel":"","threadName":"t","sender":"s","recipient":"r","topic":"t","timestamp":"t","uid":"1"}"#;
        let token = make_token(json);
        assert!(deserialize_context(&token).is_err());
    }

    #[test]
    fn test_deserialize_missing_recipient() {
        let json = r#"{"channel":"email","threadName":"t","sender":"s","recipient":"","topic":"t","timestamp":"t","uid":"1"}"#;
        let token = make_token(json);
        assert!(deserialize_context(&token).is_err());
    }

    #[test]
    fn test_deserialize_tampered_backticks() {
        let json = r#"`{"channel":"email"}`"#;
        let token = make_token(json);
        assert!(deserialize_context(&token).is_err());
    }

    #[test]
    fn test_deserialize_invalid_base64() {
        assert!(deserialize_context("not-valid-base64!!!").is_err());
    }

    #[test]
    fn test_context_to_inbound_message() {
        let ctx = ReplyContext {
            channel: "email".to_string(),
            thread_name: "test".to_string(),
            sender: "John".to_string(),
            recipient: "john@example.com".to_string(),
            topic: "Test Subject".to_string(),
            timestamp: "2026-03-27T10:00:00Z".to_string(),
            incoming_message_dir: Some("2026-03-27_10-00-00".to_string()),
            external_id: Some("<msg@example.com>".to_string()),
            thread_refs: Some(vec!["<ref1@example.com>".to_string()]),
            uid: "42".to_string(),
            nonce: None,
            channel_metadata: None,
        };

        let msg = context_to_inbound_message(&ctx);
        assert_eq!(msg.channel, "email");
        assert_eq!(msg.sender_address, "john@example.com");
        assert_eq!(msg.topic, "Test Subject");
        assert_eq!(msg.external_id.as_deref(), Some("<msg@example.com>"));
    }
}
