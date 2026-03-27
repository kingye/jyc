use anyhow::{bail, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};

/// Minimal reply context token — channel-agnostic routing + file location.
///
/// The token is intentionally minimal to reduce corruption risk from AI models.
/// All message metadata (sender, recipient, topic, threading headers) is read
/// from the stored received.md file by the reply tool — NOT from the token.
///
/// Token fields:
/// - `channel`: config channel name (routing key for outbound adapter)
/// - `threadName`: thread directory name (for logging)
/// - `incomingMessageDir`: message subdirectory name (to find received.md)
/// - `uid`: channel-specific message ID
/// - `_nonce`: integrity nonce
/// - `model`: OpenCode model ID (optional)
/// - `mode`: OpenCode mode (optional)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyContext {
    /// Config channel name (e.g., "jiny283") — routing key
    pub channel: String,
    /// Thread directory name (e.g., "weather") — for logging
    #[serde(rename = "threadName")]
    pub thread_name: String,
    /// Message subdirectory under messages/ (e.g., "2026-03-27_10-00-00")
    #[serde(rename = "incomingMessageDir")]
    pub incoming_message_dir: String,
    /// Channel-specific message ID (e.g., IMAP UID)
    pub uid: String,
    /// Integrity nonce
    #[serde(rename = "_nonce")]
    pub nonce: Option<String>,
    /// OpenCode model ID (optional)
    pub model: Option<String>,
    /// OpenCode mode (optional)
    pub mode: Option<String>,
}

/// Serialize a reply context token (struct → JSON → base64).
///
/// Uses standard base64 (with padding) to match jiny-m's format.
pub fn serialize_context(
    channel: &str,
    thread_name: &str,
    incoming_message_dir: &str,
    uid: &str,
) -> String {
    serialize_context_with_options(channel, thread_name, incoming_message_dir, uid, None, None)
}

/// Serialize a reply context token with optional model and mode.
pub fn serialize_context_with_options(
    channel: &str,
    thread_name: &str,
    incoming_message_dir: &str,
    uid: &str,
    model: Option<&str>,
    mode: Option<&str>,
) -> String {
    let nonce = format!(
        "{}-{}",
        chrono::Utc::now().timestamp_millis(),
        &uuid::Uuid::new_v4().to_string()[..8]
    );

    let context = ReplyContext {
        channel: channel.to_string(),
        thread_name: thread_name.to_string(),
        incoming_message_dir: incoming_message_dir.to_string(),
        uid: uid.to_string(),
        nonce: Some(nonce),
        model: model.map(|m| m.to_string()),
        mode: mode.map(|m| m.to_string()),
    };

    let json = serde_json::to_string(&context).unwrap_or_default();
    base64::engine::general_purpose::STANDARD.encode(json)
}

/// Deserialize and validate a reply context token.
///
/// base64 → JSON → ReplyContext with integrity checks.
pub fn deserialize_context(encoded: &str) -> Result<ReplyContext> {
    // Try standard base64 first, then URL-safe (backward compat)
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded))
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
    if ctx.incoming_message_dir.is_empty() {
        bail!("missing required field: incomingMessageDir");
    }

    Ok(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_deserialize_round_trip() {
        let token = serialize_context("jiny283", "weather", "2026-03-27_10-00-00", "42");
        let ctx = deserialize_context(&token).unwrap();
        assert_eq!(ctx.channel, "jiny283");
        assert_eq!(ctx.thread_name, "weather");
        assert_eq!(ctx.incoming_message_dir, "2026-03-27_10-00-00");
        assert_eq!(ctx.uid, "42");
        assert!(ctx.nonce.is_some());
        assert!(ctx.model.is_none());
        assert!(ctx.mode.is_none());
    }

    #[test]
    fn test_deserialize_missing_channel() {
        let json = r#"{"channel":"","threadName":"t","incomingMessageDir":"d","uid":"1"}"#;
        let token = base64::engine::general_purpose::STANDARD.encode(json);
        assert!(deserialize_context(&token).is_err());
    }

    #[test]
    fn test_deserialize_missing_message_dir() {
        let json = r#"{"channel":"ch","threadName":"t","incomingMessageDir":"","uid":"1"}"#;
        let token = base64::engine::general_purpose::STANDARD.encode(json);
        assert!(deserialize_context(&token).is_err());
    }

    #[test]
    fn test_deserialize_invalid_base64() {
        assert!(deserialize_context("not-valid!!!").is_err());
    }

    #[test]
    fn test_deserialize_tampered_backticks() {
        let json = r#"`{"channel":"ch"}`"#;
        let token = base64::engine::general_purpose::STANDARD.encode(json);
        assert!(deserialize_context(&token).is_err());
    }

    #[test]
    fn test_minimal_token_is_short() {
        let token = serialize_context("jiny283", "weather", "2026-03-27_10-00-00", "42");
        // Minimal token should be well under 300 chars
        assert!(token.len() < 300, "token too long: {} chars", token.len());
    }

    #[test]
    fn test_serialize_with_model_and_mode() {
        let token = serialize_context_with_options(
            "jiny283",
            "weather",
            "2026-03-27_10-00-00",
            "42",
            Some("claude-3-5-sonnet"),
            Some("plan"),
        );
        let ctx = deserialize_context(&token).unwrap();
        assert_eq!(ctx.model, Some("claude-3-5-sonnet".to_string()));
        assert_eq!(ctx.mode, Some("plan".to_string()));
    }

    #[test]
    fn test_backward_compat_with_minimal_token() {
        // Old token format without new fields should still work
        let json = r#"{"channel":"jiny283","threadName":"weather","incomingMessageDir":"2026-03-27_10-00-00","uid":"42","_nonce":"123456789-abc12345"}"#;
        let token = base64::engine::general_purpose::STANDARD.encode(json);
        let ctx = deserialize_context(&token).unwrap();
        assert_eq!(ctx.channel, "jiny283");
        assert_eq!(ctx.thread_name, "weather");
        assert_eq!(ctx.incoming_message_dir, "2026-03-27_10-00-00");
        assert_eq!(ctx.uid, "42");
        // New fields should be None when not present
        assert!(ctx.model.is_none());
        assert!(ctx.mode.is_none());
    }

    #[test]
    fn test_deserialize_with_model_and_mode() {
        // Token with model and mode fields
        let json = r#"{"channel":"jiny283","threadName":"weather","incomingMessageDir":"2026-03-27_10-00-00","uid":"42","_nonce":"123456789-abc12345","model":"claude-3-5-sonnet","mode":"plan"}"#;
        let token = base64::engine::general_purpose::STANDARD.encode(json);
        let ctx = deserialize_context(&token).unwrap();
        assert_eq!(ctx.channel, "jiny283");
        assert_eq!(ctx.thread_name, "weather");
        assert_eq!(ctx.model, Some("claude-3-5-sonnet".to_string()));
        assert_eq!(ctx.mode, Some("plan".to_string()));
    }
}
