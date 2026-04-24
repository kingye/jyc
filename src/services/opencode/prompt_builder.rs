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
///
/// Note: Project-specific instructions are handled by OpenCode's native
/// AGENTS.md (rules) and SKILL.md (skills) discovery — not injected here.
pub fn build_system_prompt(
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

## Tool Usage
- Use `webfetch` for web searches and fetching web content. Do NOT use `curl` or `wget`.
- Always try multiple approaches before saying you cannot do something.
- When one tool or approach fails, try alternatives. Do NOT give up after a single failure.
- If a URL fails, try alternative websites or search engines for the same information.

## Important: Focus on the Current Message
You MUST only respond to the CURRENT "Incoming Message". Do NOT continue work from previous messages.
After you have replied to the current message, STOP. Do not do anything else.

**CRITICAL: The current message has ABSOLUTE PRIORITY over your prior conclusions.**
If the current message asks you to do something (e.g., run tests, fix a bug, make a change)
that contradicts what you previously concluded (e.g., "the PR is done", "no further action needed"),
you MUST obey the current message — NOT your prior conclusion. Your previous "Done" or "Completed"
statements are irrelevant; the current message is your new task.
"#,
        thread_path.display()
    ));

    // Mode-specific reply instructions
    if mode == Some("plan") {
        prompt.push_str(
            r#"<system-reminder>
PLAN MODE: READ-ONLY ANALYSIS ONLY.
STRICT: No file edits, modifications, or system changes.
ABSOLUTE: Do NOT use edit, write, or modifying bash commands.
</system-reminder>

## PLAN MODE: Analysis & Planning
ALLOWED: read, glob, grep, task:explore, websearch.
ALLOWED bash: ls, grep, find, cat, curl (read-only only).
PROHIBITED: edit, write, bash commands that modify files.

## Your Task
1. Analyze code and requirements
2. Create implementation plan
3. Identify risks and dependencies
4. Ask clarifying questions

## Output
Natural language analysis and plan only.
No code execution or file modifications.
"#,
        );
    } else {
        // BUILD mode (default)
        prompt.push_str(
            r#"## BUILD MODE: Full Execution
You are in BUILD mode with full tool access.

## How to Handle Messages
1. **Information questions** (weather, news, facts, translations, calculations, etc.):
   - Use `bash` with `curl` to fetch information directly. Examples:
     - Weather: `curl -s "wttr.in/CityName?format=3"` or `curl -s "wttr.in/CityName"`
     - Web content: `curl -s "https://..."`
   - Do NOT search the codebase for APIs or integrations. Just use curl directly.
   - Compose your answer from the fetched data, then reply immediately.

2. **Coding/engineering tasks** (build features, fix bugs, edit files, etc.):
   - Use all available tools (bash, read, write, edit, glob, grep) to complete the task.
   - Work only within your working directory.

3. **General conversation** (greetings, opinions, explanations):
   - Reply directly with your knowledge. No tools needed.

## Reply Instructions
When you have your answer ready, use the jyc_reply_reply_message tool:
- `message`: Your reply text
- `attachments`: Optional filenames to attach from the working directory
After a successful reply, STOP immediately. Do NOT call any other tools or perform further actions.
CRITICAL: Always use jyc_reply_reply_message tool to send your reply.
"#,
        );
    }

    // Chat history access
    prompt.push_str(
        r#"
## Chat History Access
This thread maintains a chronological chat history in Markdown format. The history includes all received messages and replies.

### Location
- `chat_history_YYYY-MM-DD.md` in the thread directory (e.g., `chat_history_2026-04-07.md`)

### Format
Each entry in the chat history has:
```markdown
<!-- timestamp | type:received/reply | matched:true/false | sender:... | channel:... | external_id:... -->
**FROM:** sender_address
**SUBJECT:** topic

message content...

---
```

### How to Access
Use the available tools to read chat history:
1. **Find current day's log**: `glob "chat_history_*.md"`
2. **Read specific file**: `read "chat_history_2026-04-07.md"`
3. **Search history**: `grep "keyword" --include "chat_history_*.md"`

### Important Notes
- **Read-only access**: You can read chat history but do NOT modify these files directly
- **Context-aware**: When user asks about previous conversations, check the chat history
- **Security boundary**: Stay within the thread directory when accessing history
- **Privacy**: Respect user data privacy; do not expose sensitive information

The chat history provides context about ongoing discussions, past decisions, and implementation details.
"#,
    );

    prompt
}

/// Build the user prompt for a single inbound message.
///
/// Includes:
/// - Incoming message body (stripped, truncated)
/// - Optional session reset notification if session was reset due to token limit
/// Note: Reply context is saved to disk (.jyc/reply-context.json), NOT embedded in prompt.
pub async fn build_prompt(
    message: &InboundMessage,
    _thread_path: &Path,
    _message_dir: &str,
    session_was_reset_due_to_tokens: bool,
    mode: Option<&str>,
) -> Result<String> {
    let mut prompt = String::new();

    // Add mode prefix if in PLAN mode
    if mode == Some("plan") {
        prompt.push_str("[PLAN: READ-ONLY]\n\n");
    }

    // Session reset notification (if applicable)
    if session_was_reset_due_to_tokens {
        tracing::info!("Session reset note injected into prompt (input token limit exceeded)");
        prompt.push_str("⚠️ **Note:** This is a NEW session. The previous session was reset because the input token limit was exceeded.\n");
        prompt.push_str("You have lost all previous conversation context. To understand what was discussed and worked on before, read the chat history log file `chat_history_<date>.md` in the thread directory. It is a chronological record of all messages and replies.\n");
        prompt.push_str("Continue your work based on the latest entries in that file.\n\n");
    }

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
        let prompt = build_system_prompt(tmp.path(), Some("Be helpful."), Some("build"));

        assert!(prompt.contains("Be helpful."));
        assert!(prompt.contains("jyc_reply_reply_message"));
        assert!(prompt.contains("Directory Boundaries"));
        assert!(prompt.contains("BUILD MODE"));
        assert!(!prompt.contains("Previous Session Summary"));
    }

    #[test]
    fn test_build_system_prompt_no_system_md() {
        // system.md is no longer loaded by prompt_builder;
        // OpenCode handles project instructions via AGENTS.md and skills natively.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("system.md"), "You are a code reviewer.").unwrap();

        let prompt = build_system_prompt(tmp.path(), None, None);
        // system.md content should NOT appear in the system prompt
        assert!(!prompt.contains("You are a code reviewer."));
        assert!(prompt.contains("BUILD MODE"));
    }

    #[test]
    fn test_build_system_prompt_plan_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let prompt = build_system_prompt(tmp.path(), Some("Be helpful."), Some("plan"));

        assert!(prompt.contains("Be helpful."));
        assert!(prompt.contains("PLAN MODE"));
        assert!(prompt.contains("PROHIBITED: edit, write, bash commands that modify files"));
        assert!(!prompt.contains("jyc_reply_reply_message"));
    }

    #[tokio::test]
    async fn test_build_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let msg = test_message();

        let prompt = build_prompt(&msg, tmp.path(), "2026-03-27_10-00-00", false, None)
            .await
            .unwrap();

        assert!(prompt.contains("## Incoming Message"));
        assert!(prompt.contains("John"));
        assert!(prompt.contains("john@example.com"));
        assert!(prompt.contains("Hello, help me."));
        // No REPLY_TOKEN in prompt — context is on disk
        assert!(!prompt.contains("REPLY_TOKEN="));
        // No session reset notification
        assert!(!prompt.contains("session has been reset"));
    }

    #[tokio::test]
    async fn test_build_prompt_with_session_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let msg = test_message();

        let prompt = build_prompt(&msg, tmp.path(), "2026-03-27_10-ONLY", true, None)
            .await
            .unwrap();

        assert!(prompt.contains("## Incoming Message"));
        assert!(prompt.contains("John"));
        assert!(prompt.contains("john@example.com"));
        assert!(prompt.contains("Hello, help me."));
        // Should contain session reset notification
        assert!(prompt.contains("NEW session"));
        assert!(prompt.contains("input token limit"));
        assert!(prompt.contains("chat_history_"));
    }
}
