use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

const REPLY_CONTEXT_FILENAME: &str = "reply-context.json";

/// Reply context — saved to disk per-thread, read by the MCP reply tool.
///
/// Written by OpenCodeService before sending the prompt.
/// Read by reply_tool from cwd (= thread directory).
/// Overwritten on each new incoming message; persists between replies
/// to allow multiple replies in the same thread.
///
/// This replaces the old REPLY_TOKEN approach where context was passed
/// through the AI as a base64 token (prone to corruption by AI models).
/// Now the context lives on disk — the AI never sees or touches it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyContext {
    /// Config channel name (e.g., "jiny283") — routing key
    pub channel: String,
    /// Thread directory name (e.g., "weather")
    #[serde(rename = "threadName")]
    pub thread_name: String,
    /// Message subdirectory under messages/ (e.g., "2026-03-27_10-00-00")
    #[serde(rename = "incomingMessageDir")]
    pub incoming_message_dir: String,
    /// Channel-specific message ID (e.g., IMAP UID)
    pub uid: String,
    /// AI model used (e.g., "ark/deepseek-v3.2")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// AI mode used (e.g., "build", "plan")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// When this context was created
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// Save reply context to `.jyc/reply-context.json` in the thread directory.
///
/// Called by OpenCodeService before sending the prompt.
pub async fn save_reply_context(thread_path: &Path, ctx: &ReplyContext) -> Result<()> {
    let jyc_dir = thread_path.join(".jyc");
    tokio::fs::create_dir_all(&jyc_dir).await?;

    let path = jyc_dir.join(REPLY_CONTEXT_FILENAME);
    let content = serde_json::to_string_pretty(ctx)?;
    tokio::fs::write(&path, content).await?;

    tracing::debug!(
        channel = %ctx.channel,
        message_dir = %ctx.incoming_message_dir,
        "Reply context saved to disk"
    );
    Ok(())
}

/// Load reply context from `.jyc/reply-context.json` in the given directory.
///
/// Called by the MCP reply tool from its cwd (= thread directory).
pub async fn load_reply_context(thread_path: &Path) -> Result<ReplyContext> {
    let path = thread_path.join(".jyc").join(REPLY_CONTEXT_FILENAME);

    if !path.exists() {
        bail!("reply-context.json not found in {}", thread_path.display());
    }

    let content = tokio::fs::read_to_string(&path).await?;
    let ctx: ReplyContext = serde_json::from_str(&content)?;

    // Validate required fields
    if ctx.channel.is_empty() {
        bail!("missing required field: channel");
    }
    if ctx.incoming_message_dir.is_empty() {
        bail!("missing required field: incomingMessageDir");
    }

    Ok(ctx)
}

/// Delete the reply context file (used for tests and manual cleanup).
///
/// Note: Not called during normal operation; context persists to
/// support multiple replies in the same thread.
pub async fn cleanup_reply_context(thread_path: &Path) {
    let path = thread_path.join(".jyc").join(REPLY_CONTEXT_FILENAME);
    if path.exists() {
        tokio::fs::remove_file(&path).await.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_save_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ReplyContext {
            channel: "jiny283".to_string(),
            thread_name: "weather".to_string(),
            incoming_message_dir: "2026-03-27_10-00-00".to_string(),
            uid: "42".to_string(),
            model: Some("ark/deepseek-v3.2".to_string()),
            mode: Some("build".to_string()),
            created_at: "2026-03-27T10:00:00Z".to_string(),
        };

        save_reply_context(tmp.path(), &ctx).await.unwrap();
        let loaded = load_reply_context(tmp.path()).await.unwrap();

        assert_eq!(loaded.channel, "jiny283");
        assert_eq!(loaded.thread_name, "weather");
        assert_eq!(loaded.incoming_message_dir, "2026-03-27_10-00-00");
        assert_eq!(loaded.uid, "42");
        assert_eq!(loaded.model.as_deref(), Some("ark/deepseek-v3.2"));
        assert_eq!(loaded.mode.as_deref(), Some("build"));
    }

    #[tokio::test]
    async fn test_load_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load_reply_context(tmp.path()).await.is_err());
    }

    #[tokio::test]
    async fn test_cleanup() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ReplyContext {
            channel: "ch".to_string(),
            thread_name: "t".to_string(),
            incoming_message_dir: "d".to_string(),
            uid: "1".to_string(),
            model: None,
            mode: None,
            created_at: "now".to_string(),
        };

        save_reply_context(tmp.path(), &ctx).await.unwrap();
        assert!(tmp.path().join(".jyc/reply-context.json").exists());

        cleanup_reply_context(tmp.path()).await;
        assert!(!tmp.path().join(".jyc/reply-context.json").exists());
    }

    #[tokio::test]
    async fn test_load_missing_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        tokio::fs::create_dir_all(&jyc_dir).await.unwrap();
        tokio::fs::write(
            jyc_dir.join("reply-context.json"),
            r#"{"channel":"","threadName":"t","incomingMessageDir":"d","uid":"1","createdAt":"now"}"#,
        ).await.unwrap();
        assert!(load_reply_context(tmp.path()).await.is_err());
    }
}
