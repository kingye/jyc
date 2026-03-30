use anyhow::Result;
use std::path::Path;

use crate::channels::types::InboundMessage;
use crate::core::email_parser;
use crate::utils::constants::MAX_BODY_IN_PROMPT;

/// Build the system prompt for OpenCode.
///
/// Includes:
/// - Configured system prompt (from agent config)
/// - Security: directory boundary rules
/// - Reply instructions (mode-specific: plan = text-only, build = use reply tool)
/// - Optional thread-specific system.md
pub async fn build_system_prompt(
    thread_path: &Path,
    config_system_prompt: Option<&str>,
    mode: Option<&str>,
) -> String {
    let mut prompt = String::new();

    // Config-level system prompt
    if let Some(sp) = config_system_prompt {
        prompt.push_str(sp);
        prompt.push_str("\n\n");
    }

    // Security: directory boundaries
    prompt.push_str(&format!(
        r#"Your working directory is "{}". You MUST only read, write, and access files within this directory. Do NOT access files outside this directory.

## Security: Directory Boundaries
- NEVER use `..` or any relative path that resolves outside your working directory.
- Do NOT access, read, write, list, or reference any parent directories or sibling workspaces.
- Do NOT use absolute paths outside your working directory.
- If a task requires files outside this directory, refuse and explain you cannot access them.

## Important: Focus on the Current Message
You MUST only respond to the CURRENT "Incoming Message". Do NOT continue work from previous messages.
After you have replied to the current message, STOP. Do not do anything else.
"#,
        thread_path.display()
    ));

    // Mode-specific reply instructions
    if mode == Some("plan") {
        prompt.push_str(
            r#"## PLAN MODE: Read-Only

## Reply Instructions
- DO NOT use the jiny_reply_reply_message tool — it requires write permissions which are blocked.
- Simply provide your text response as natural language output.
- Focus on analysis, planning, and recommendations.
"#,
        );
    } else {
        prompt.push_str(
            r#"## BUILD MODE: Full Execution

## Reply Instructions
When replying to a message, use the jiny_reply_reply_message tool:
- `message`: Your reply text
- `attachments`: Optional filenames to attach from the working directory
After a successful reply, STOP immediately. Do NOT call any other tools or perform further actions.
CRITICAL: Always use jiny_reply_reply_message tool.
"#,
        );
    }

    // Thread-specific system.md
    let system_md_path = thread_path.join("system.md");
    if let Ok(content) = tokio::fs::read_to_string(&system_md_path).await {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            tracing::info!(
                path = %system_md_path.display(),
                len = trimmed.len(),
                "system.md loaded"
            );
            prompt.push('\n');
            prompt.push_str(trimmed);
            prompt.push('\n');
        }
    } else {
        tracing::debug!(
            path = %system_md_path.display(),
            "No system.md found"
        );
    }

    prompt
}

/// Build the user prompt for a single inbound message.
///
/// Includes:
/// - Incoming message body (stripped, truncated)
/// Note: Reply context is saved to disk (.jyc/reply-context.json), NOT embedded in prompt.
pub async fn build_prompt(
    message: &InboundMessage,
    thread_path: &Path,
    message_dir: &str,
) -> Result<String> {
    let mut prompt = String::new();

    // Incoming message
    prompt.push_str("## Incoming Message\n");
    prompt.push_str(&format!(
        "**From:** {} <{}>\n",
        message.sender, message.sender_address
    ));
    prompt.push_str(&format!("**Subject:** {}\n", message.topic));
    prompt.push_str(&format!("**Date:** {}\n\n", message.timestamp.to_rfc3339()));

    // Body (stripped + truncated)
    let body = message
        .content
        .text
        .as_deref()
        .or(message.content.markdown.as_deref())
        .unwrap_or("[no text content]");
    let stripped = email_parser::strip_quoted_history(body);
    let truncated = email_parser::truncate_text(&stripped, MAX_BODY_IN_PROMPT);
    prompt.push_str("**Body:**\n");
    prompt.push_str(&truncated);
    prompt.push('\n');

    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::{InboundMessage, MessageContent};
    use std::collections::HashMap;

    fn test_message() -> InboundMessage {
        InboundMessage {
            id: "test".to_string(),
            channel: "email".to_string(),
            channel_uid: "42".to_string(),
            sender: "John".to_string(),
            sender_address: "john@example.com".to_string(),
            recipients: vec![],
            topic: "Test Subject".to_string(),
            content: MessageContent {
                text: Some("Hello, help me.".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: Some("<msg123@example.com>".to_string()),
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        }
    }

    #[tokio::test]
    async fn test_build_system_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let prompt = build_system_prompt(tmp.path(), Some("Be helpful."), Some("build")).await;

        assert!(prompt.contains("Be helpful."));
        assert!(prompt.contains("jiny_reply_reply_message"));
        assert!(prompt.contains("Directory Boundaries"));
        assert!(prompt.contains("BUILD MODE"));
    }

    #[tokio::test]
    async fn test_build_system_prompt_with_system_md() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("system.md"), "You are a code reviewer.")
            .await
            .unwrap();

        let prompt = build_system_prompt(tmp.path(), None, None).await;
        assert!(prompt.contains("You are a code reviewer."));
        assert!(prompt.contains("BUILD MODE"));
    }

    #[tokio::test]
    async fn test_build_system_prompt_plan_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let prompt = build_system_prompt(tmp.path(), Some("Be helpful."), Some("plan")).await;

        assert!(prompt.contains("Be helpful."));
        assert!(prompt.contains("PLAN MODE"));
        assert!(prompt.contains("DO NOT use the jiny_reply_reply_message tool"));
        assert!(!prompt.contains("Always use jiny_reply_reply_message tool"));
    }

    #[tokio::test]
    async fn test_build_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let msg = test_message();

        let prompt = build_prompt(&msg, tmp.path(), "2026-03-27_10-00-00")
            .await
            .unwrap();

        assert!(prompt.contains("## Incoming Message"));
        assert!(prompt.contains("John"));
        assert!(prompt.contains("john@example.com"));
        assert!(prompt.contains("Hello, help me."));
        // No REPLY_TOKEN in prompt — context is on disk
        assert!(!prompt.contains("REPLY_TOKEN="));
    }
}
