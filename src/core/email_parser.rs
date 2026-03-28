use regex::Regex;
use std::sync::LazyLock;

use crate::utils::helpers::sanitize_for_filesystem;

/// Regex for stripping reply/forward prefixes from email subjects.
/// Handles: Re:, Fwd:, Fw:, 回复:, 转发:, RE:, FW: and combinations.
static REPLY_PREFIX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(\s*(re|fwd?|回复|转发)\s*[:：]\s*)+").unwrap());

/// Regex for detecting quoted reply headers (e.g., "On ... wrote:", "发件人:", "From:")
/// Matches at start of trimmed line, so leading whitespace is handled.
static QUOTED_HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(on\s+.+\s+wrote\s*:|发件人\s*[:：]|from\s*[:：]|sent\s*[:：]|date\s*[:：]|to\s*[:：]|subject\s*[:：]|收件人\s*[:：]|日期\s*[:：]|主题\s*[:：]|发件时间\s*[:：])").unwrap()
});

/// Regex for detecting divider lines (e.g., "---", "___", "===")
static DIVIDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\s]*[-_=]{3,}[\s]*$").unwrap());

/// Strip reply/forward prefixes from a subject line.
///
/// Handles Re:, Fwd:, Fw:, 回复:, 转发: and nested combinations like "Re: Re: Fwd:".
pub fn strip_reply_prefix(subject: &str) -> String {
    REPLY_PREFIX_RE.replace(subject, "").trim().to_string()
}

/// Derive a thread name from an email subject.
///
/// 1. Strip reply/forward prefixes (Re:, Fwd:, 回复:, etc.)
/// 2. Strip configured pattern prefixes (sorted longest-first)
/// 3. Sanitize for use as a filesystem directory name
pub fn derive_thread_name(subject: &str, pattern_prefixes: &[String]) -> String {
    let mut name = strip_reply_prefix(subject);

    // Strip configured prefixes (longest first to avoid partial matches)
    let mut sorted_prefixes: Vec<&String> = pattern_prefixes.iter().collect();
    sorted_prefixes.sort_by(|a, b| b.len().cmp(&a.len()));

    for prefix in sorted_prefixes {
        let lower_name = name.to_lowercase();
        let lower_prefix = prefix.to_lowercase();
        if lower_name.starts_with(&lower_prefix) {
            name = name[prefix.len()..].to_string();
            // Strip separator characters after the prefix
            name = name
                .trim_start_matches(|c: char| {
                    matches!(c, ':' | '-' | '_' | '~' | '|' | '/' | '&' | '$' | ' ')
                })
                .to_string();
            break; // Only strip first matching prefix
        }
    }

    let sanitized = sanitize_for_filesystem(name.trim());
    if sanitized.is_empty() {
        "unnamed".to_string()
    } else {
        sanitized
    }
}

/// Strip quoted reply history from an email body.
///
/// Scans line-by-line and removes:
/// - Lines starting with `>` at depth 2+ (`>> ...`)
/// - Reply header blocks (e.g., "On ... wrote:", "发件人:", "From:")
/// - Divider lines (`---`, `___`, `===`)
///
/// Returns the text above the quoted history.
pub fn strip_quoted_history(text: &str) -> String {
    let mut result_lines = Vec::new();
    let mut in_quote_block = false;

    for line in text.lines() {
        let trimmed = line.trim();

        // Detect start of quoted block
        if !in_quote_block {
            // Depth-2+ quotes: >> ... (always part of quoted history)
            if trimmed.starts_with(">>") {
                in_quote_block = true;
                continue;
            }

            // Reply headers: "On ... wrote:", "发件人:", etc.
            if QUOTED_HEADER_RE.is_match(trimmed) {
                in_quote_block = true;
                continue;
            }

            // Divider lines that typically precede quoted content
            if DIVIDER_RE.is_match(trimmed) && !result_lines.is_empty() {
                // Check if next content looks like a quote — for now, treat dividers
                // at the end as the boundary
                in_quote_block = true;
                continue;
            }

            result_lines.push(line);
        }
        // Once we're in a quote block, skip everything
    }

    // Trim trailing empty lines
    while result_lines.last().is_some_and(|l| l.trim().is_empty()) {
        result_lines.pop();
    }

    result_lines.join("\n")
}

/// Clean an email body at the inbound boundary.
///
/// - Normalize whitespace
/// - Fix common email body artifacts
pub fn clean_email_body(text: &str) -> String {
    let mut result = text.to_string();

    // Normalize CRLF to LF
    result = result.replace("\r\n", "\n");

    // Remove null bytes
    result = result.replace('\0', "");

    // Collapse more than 3 consecutive blank lines into 2
    let blank_line_re = Regex::new(r"\n{4,}").unwrap();
    result = blank_line_re.replace_all(&result, "\n\n\n").to_string();

    result.trim().to_string()
}

/// Truncate text to a maximum length, breaking at word boundaries.
pub fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }

    // Find a safe truncation point
    let mut end = max_chars;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }

    // Try to break at a word boundary
    if let Some(space_pos) = text[..end].rfind(char::is_whitespace) {
        if space_pos > max_chars / 2 {
            return format!("{}...", &text[..space_pos]);
        }
    }

    format!("{}...", &text[..end])
}

/// Parse a stored received.md file, extracting all frontmatter and body.
#[derive(Debug)]
pub struct ParsedStoredMessage {
    pub sender: Option<String>,
    pub sender_address: Option<String>,
    pub timestamp: Option<String>,
    pub topic: Option<String>,
    pub body: String,
    pub channel: Option<String>,
    pub uid: Option<String>,
    pub external_id: Option<String>,
    pub reply_to_id: Option<String>,
    pub thread_refs: Option<Vec<String>>,
    pub matched_pattern: Option<String>,
}

/// Parse a stored received.md file.
///
/// Expected format:
/// ```text
/// ---
/// channel: email
/// uid: "12345"
/// topic: "Help with feature X"
/// ---
/// ## Sender Name (10:15 AM)
///
/// Message body here
/// ---
/// ```
pub fn parse_stored_message(content: &str) -> ParsedStoredMessage {
    let mut channel = None;
    let mut uid = None;
    let mut topic = None;
    let mut sender = None;
    let mut sender_address = None;
    let mut timestamp = None;
    let mut external_id = None;
    let mut reply_to_id = None;
    let mut thread_refs = None;
    let mut matched_pattern = None;
    let mut body = String::new();

    let mut in_frontmatter = false;
    let mut frontmatter_ended = false;
    let mut found_header = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if !frontmatter_ended {
            if trimmed == "---" {
                if in_frontmatter {
                    frontmatter_ended = true;
                    in_frontmatter = false;
                } else {
                    in_frontmatter = true;
                }
                continue;
            }

            if in_frontmatter {
                if let Some((key, value)) = trimmed.split_once(':') {
                    let key = key.trim();
                    let value = value.trim().trim_matches('"');
                    match key {
                        "channel" => channel = Some(value.to_string()),
                        "uid" => uid = Some(value.to_string()),
                        "sender" => {
                            // "sender" in frontmatter is the display name
                            sender = Some(value.to_string());
                        }
                        "sender_address" => sender_address = Some(value.to_string()),
                        "topic" => topic = Some(value.to_string()),
                        "external_id" => external_id = Some(value.to_string()),
                        "reply_to_id" => reply_to_id = Some(value.to_string()),
                        "thread_refs" => {
                            // Parse YAML-style array: ["ref1", "ref2"]
                            let refs_str = value.trim_matches(|c| c == '[' || c == ']');
                            let refs: Vec<String> = refs_str
                                .split(',')
                                .map(|s| s.trim().trim_matches('"').to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                            if !refs.is_empty() {
                                thread_refs = Some(refs);
                            }
                        }
                        "matched_pattern" => matched_pattern = Some(value.to_string()),
                        "timestamp" => timestamp = Some(value.to_string()),
                        _ => {}
                    }
                }
                continue;
            }
        }

        // Parse the header line: ## Sender Name (10:15 AM)
        if frontmatter_ended && !found_header && trimmed.starts_with("## ") {
            let header = &trimmed[3..];
            if let Some(paren_start) = header.rfind('(') {
                sender = Some(header[..paren_start].trim().to_string());
                if let Some(paren_end) = header.rfind(')') {
                    timestamp = Some(header[paren_start + 1..paren_end].to_string());
                }
            } else {
                sender = Some(header.to_string());
            }
            found_header = true;
            continue;
        }

        if found_header {
            // Stop at the trailing divider
            if trimmed == "---" {
                break;
            }
            if !body.is_empty() || !trimmed.is_empty() {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(line);
            }
        }
    }

    ParsedStoredMessage {
        sender,
        sender_address,
        timestamp,
        topic,
        body,
        channel,
        uid,
        external_id,
        reply_to_id,
        thread_refs,
        matched_pattern,
    }
}

/// Parse a stored reply.md file, extracting only the AI's response text.
///
/// Stops before quoted history blocks (lines starting with `### ` that look
/// like quoted reply headers, or `---` dividers).
pub fn parse_stored_reply(content: &str) -> String {
    let mut lines = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Stop at divider (start of quoted history)
        if trimmed == "---" && !lines.is_empty() {
            break;
        }

        // Stop at quoted reply headers (### SenderName (timestamp))
        if trimmed.starts_with("### ") && trimmed.contains('(') && trimmed.contains(')') {
            break;
        }

        lines.push(line);
    }

    // Trim trailing empty lines
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

/// Format a date-time for display in quoted history headers.
/// Output format: "YYYY-MM-DD HH:MM"
pub fn format_datetime_iso(dt: &chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M").to_string()
}

/// Format a single quoted reply entry for inclusion in an email.
///
/// Matches jiny-m's format:
/// ```text
/// ---
/// ### Sender Name (2026-03-27 10:00)
/// > Subject
/// >
/// > Body text quoted...
/// ```
///
/// Returns empty string if body_text is empty.
pub fn format_quoted_reply(
    sender: &str,
    timestamp: &str,
    subject: &str,
    body_text: &str,
) -> String {
    if body_text.trim().is_empty() {
        return String::new();
    }

    // Clean sender display name
    let from_name = if sender.contains('<') {
        sender
            .split('<')
            .next()
            .unwrap_or(sender)
            .trim()
            .trim_matches(|c| c == '\'' || c == '"')
            .to_string()
    } else {
        sender.to_string()
    };
    let from_name = if from_name.is_empty() {
        sender.to_string()
    } else {
        from_name
    };

    let mut lines = Vec::new();
    lines.push("---".to_string());
    lines.push(format!("### {from_name} ({timestamp})"));

    if !subject.is_empty() {
        lines.push(format!("> {subject}"));
    }

    lines.push(String::new());

    for line in body_text.lines() {
        lines.push(format!("> {line}"));
    }

    lines.join("\n")
}

// --- Thread Trail & Reply Building ---

/// A single entry in a thread trail (for building quoted history).
#[derive(Debug)]
pub struct TrailEntry {
    pub sender: String,
    pub timestamp: String,
    pub topic: String,
    pub body_text: String,
    /// "received" or "reply"
    pub entry_type: String,
}

/// Build a thread trail from stored messages.
///
/// Reads message directories in reverse chronological order (newest first).
/// For each directory, reads reply.md (AI response) then received.md (user message).
/// The current message directory is excluded.
///
/// Trail order: current received → prev reply → prev received → older reply → ...
pub async fn build_thread_trail(
    thread_path: &std::path::Path,
    current_message: Option<TrailCurrentMessage>,
    max_entries: usize,
    exclude_message_dir: Option<&str>,
) -> Vec<TrailEntry> {
    let mut trail = Vec::new();

    // Prepend current message (stripped) if provided
    if let Some(ref current) = current_message {
        let stripped = strip_quoted_history(&current.body_text);
        trail.push(TrailEntry {
            sender: current.sender.clone(),
            timestamp: current.timestamp.clone(),
            topic: current.topic.clone(),
            body_text: stripped,
            entry_type: "received".to_string(),
        });
    }

    let messages_dir = thread_path.join("messages");
    if !messages_dir.exists() {
        return trail;
    }

    // Read and sort message directories (newest first)
    let mut dirs: Vec<String> = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&messages_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(ft) = entry.file_type().await {
                if ft.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with('.') {
                        dirs.push(name);
                    }
                }
            }
        }
    }
    dirs.sort();
    dirs.reverse(); // newest first

    // Exclude current message dir
    if let Some(exclude) = exclude_message_dir {
        dirs.retain(|d| d != exclude);
    } else if current_message.is_some() && !dirs.is_empty() {
        // Assume most recent is the current message, skip it
        dirs.remove(0);
    }

    for dir_name in &dirs {
        if trail.len() >= max_entries {
            break;
        }

        let dir_path = messages_dir.join(dir_name);

        // Reply first (more recent — AI responded after receiving)
        if trail.len() < max_entries {
            if let Ok(content) = tokio::fs::read_to_string(dir_path.join("reply.md")).await {
                let reply_text = parse_stored_reply(&content);
                if !reply_text.trim().is_empty() {
                    trail.push(TrailEntry {
                        sender: "AI Assistant".to_string(),
                        timestamp: dir_name.clone(),
                        topic: String::new(),
                        body_text: reply_text,
                        entry_type: "reply".to_string(),
                    });
                }
            }
        }

        // Then received
        if trail.len() < max_entries {
            if let Ok(content) = tokio::fs::read_to_string(dir_path.join("received.md")).await {
                let parsed = parse_stored_message(&content);
                let stripped = strip_quoted_history(&parsed.body);
                if !stripped.trim().is_empty() {
                    trail.push(TrailEntry {
                        sender: parsed.sender.unwrap_or_else(|| "Unknown".to_string()),
                        timestamp: parsed.timestamp.unwrap_or_else(|| dir_name.clone()),
                        topic: parsed.topic.unwrap_or_default(),
                        body_text: stripped,
                        entry_type: "received".to_string(),
                    });
                }
            }
        }
    }

    trail.truncate(max_entries);
    trail
}

/// Current message info for building the thread trail.
pub struct TrailCurrentMessage {
    pub sender: String,
    pub timestamp: String,
    pub topic: String,
    pub body_text: String,
}

/// Prepare quoted history for a reply.
///
/// Builds a thread trail and formats each entry as a quoted block.
/// Returns the combined quoted history string (empty if no messages).
pub async fn prepare_body_for_quoting(
    thread_path: &std::path::Path,
    current_message: TrailCurrentMessage,
    max_history: Option<usize>,
    exclude_message_dir: Option<&str>,
) -> String {
    let trail = build_thread_trail(
        thread_path,
        Some(current_message),
        max_history.unwrap_or(crate::utils::constants::MAX_HISTORY_QUOTE),
        exclude_message_dir,
    )
    .await;

    let quoted_blocks: Vec<String> = trail
        .iter()
        .filter_map(|entry| {
            let quoted =
                format_quoted_reply(&entry.sender, &entry.timestamp, &entry.topic, &entry.body_text);
            if quoted.is_empty() {
                None
            } else {
                Some(quoted)
            }
        })
        .collect();

    quoted_blocks.join("\n\n")
}

/// Build a footer with model and mode information.
///
/// Returns empty string if both model and mode are None.
/// Format: `---\n\nModel: <model> | Mode: <mode>`
pub fn build_footer(model: Option<&str>, mode: Option<&str>) -> String {
    match (model, mode) {
        (Some(m), Some(md)) => format!("---\n\nModel: {} | Mode: {}", m, md),
        (Some(m), None) => format!("---\n\nModel: {}", m),
        (None, Some(md)) => format!("---\n\nMode: {}", md),
        (None, None) => String::new(),
    }
}

/// Build of full reply text with quoted history.
///
/// This is the single reply formatting function used by BOTH:
/// - ThreadManager fallback (when MCP tool wasn't used)
/// - MCP reply tool (Phase 5)
///
/// Format:
/// ```text
/// <AI reply text>
///
/// ---
/// Model: <model> | Mode: <mode>
///
/// ---
/// ### Sender Name (2026-03-27 10:00)
/// > Subject
/// >
/// > Message body quoted...
///
/// ---
/// ### AI Assistant (2026-03-27 09:55)
/// > Previous AI reply quoted...
/// ```
pub async fn build_full_reply_text(
    reply_text: &str,
    thread_path: &std::path::Path,
    sender: &str,
    timestamp: &str,
    topic: &str,
    body_text: &str,
    message_dir: &str,
    model: Option<&str>,
    mode: Option<&str>,
) -> String {
    let current_message = TrailCurrentMessage {
        sender: sender.to_string(),
        timestamp: timestamp.to_string(),
        topic: topic.to_string(),
        body_text: body_text.to_string(),
    };

    let quoted_history = prepare_body_for_quoting(
        thread_path,
        current_message,
        None,
        Some(message_dir),
    )
    .await;

    let footer = build_footer(model, mode);

    if quoted_history.is_empty() && footer.is_empty() {
        reply_text.to_string()
    } else if quoted_history.is_empty() {
        format!("{}\n\n{}", reply_text, footer)
    } else if footer.is_empty() {
        format!("{}\n\n{}", reply_text, quoted_history)
    } else {
        format!("{}\n\n{}\n\n{}", reply_text, footer, quoted_history)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- strip_reply_prefix tests ---

    #[test]
    fn test_strip_simple_re() {
        assert_eq!(strip_reply_prefix("Re: Hello"), "Hello");
    }

    #[test]
    fn test_strip_nested_re() {
        assert_eq!(strip_reply_prefix("Re: Re: Re: Hello"), "Hello");
    }

    #[test]
    fn test_strip_fwd() {
        assert_eq!(strip_reply_prefix("Fwd: Important"), "Important");
    }

    #[test]
    fn test_strip_fw() {
        assert_eq!(strip_reply_prefix("Fw: Important"), "Important");
    }

    #[test]
    fn test_strip_chinese_reply() {
        assert_eq!(strip_reply_prefix("回复: 你好"), "你好");
    }

    #[test]
    fn test_strip_chinese_forward() {
        assert_eq!(strip_reply_prefix("转发: 重要"), "重要");
    }

    #[test]
    fn test_strip_mixed_prefixes() {
        assert_eq!(strip_reply_prefix("Re: Fwd: Re: Topic"), "Topic");
    }

    #[test]
    fn test_strip_case_insensitive() {
        assert_eq!(strip_reply_prefix("RE: HELLO"), "HELLO");
        assert_eq!(strip_reply_prefix("FWD: HELLO"), "HELLO");
    }

    #[test]
    fn test_strip_no_prefix() {
        assert_eq!(strip_reply_prefix("Just a topic"), "Just a topic");
    }

    #[test]
    fn test_strip_with_spaces() {
        assert_eq!(strip_reply_prefix("Re:  Hello"), "Hello");
        assert_eq!(strip_reply_prefix("  Re: Hello"), "Hello");
    }

    // --- derive_thread_name tests ---

    #[test]
    fn test_derive_simple() {
        let name = derive_thread_name("Re: Help with feature X", &[]);
        assert_eq!(name, "Help with feature X");
    }

    #[test]
    fn test_derive_with_prefix_strip() {
        let prefixes = vec!["jiny".to_string()];
        let name = derive_thread_name("Re: jiny: Build the app", &prefixes);
        assert_eq!(name, "Build the app");
    }

    #[test]
    fn test_derive_prefix_with_separator() {
        let prefixes = vec!["jiny".to_string()];
        let name = derive_thread_name("jiny - My Task", &prefixes);
        assert_eq!(name, "My Task");
    }

    #[test]
    fn test_derive_longest_prefix_first() {
        let prefixes = vec!["dev".to_string(), "dev-team".to_string()];
        let name = derive_thread_name("dev-team: Fix bug", &prefixes);
        assert_eq!(name, "Fix bug");
    }

    #[test]
    fn test_derive_sanitizes_filename() {
        let name = derive_thread_name("Re: path/to:file*name", &[]);
        assert_eq!(name, "path_to_file_name");
    }

    #[test]
    fn test_derive_empty_subject() {
        let name = derive_thread_name("", &[]);
        assert_eq!(name, "unnamed");
    }

    #[test]
    fn test_derive_preserves_cjk() {
        let name = derive_thread_name("Re: 你好世界", &[]);
        assert_eq!(name, "你好世界");
    }

    // --- strip_quoted_history tests ---

    #[test]
    fn test_strip_no_quotes() {
        let body = "Hello,\n\nHow are you?";
        assert_eq!(strip_quoted_history(body), body);
    }

    #[test]
    fn test_strip_depth2_quotes() {
        let body = "My reply\n\n>> Original text\n>> More original";
        assert_eq!(strip_quoted_history(body), "My reply");
    }

    #[test]
    fn test_strip_on_wrote_header() {
        let body = "Thanks!\n\nOn 2026-03-20 10:00, user@example.com wrote:\n> Old message";
        assert_eq!(strip_quoted_history(body), "Thanks!");
    }

    #[test]
    fn test_strip_chinese_headers() {
        let body = "好的\n\n发件人: user@example.com\n主题: 你好";
        assert_eq!(strip_quoted_history(body), "好的");
    }

    #[test]
    fn test_strip_divider() {
        let body = "My text\n\n---\n\nQuoted stuff below";
        assert_eq!(strip_quoted_history(body), "My text");
    }

    // --- clean_email_body tests ---

    #[test]
    fn test_clean_crlf() {
        assert_eq!(clean_email_body("Hello\r\nWorld"), "Hello\nWorld");
    }

    #[test]
    fn test_clean_null_bytes() {
        assert_eq!(clean_email_body("Hello\0World"), "HelloWorld");
    }

    #[test]
    fn test_clean_excessive_blank_lines() {
        let body = "Hello\n\n\n\n\n\nWorld";
        let cleaned = clean_email_body(body);
        assert_eq!(cleaned, "Hello\n\n\nWorld");
    }

    #[test]
    fn test_clean_trims() {
        assert_eq!(clean_email_body("  Hello  \n\n"), "Hello");
    }

    // --- truncate_text tests ---

    #[test]
    fn test_truncate_short_text() {
        assert_eq!(truncate_text("Hello", 100), "Hello");
    }

    #[test]
    fn test_truncate_at_word_boundary() {
        let result = truncate_text("Hello World Test", 12);
        assert_eq!(result, "Hello World...");
    }

    // --- parse_stored_message tests ---

    #[test]
    fn test_parse_stored_message() {
        let content = r#"---
channel: email
uid: "12345"
topic: "Help me"
---
## John Doe (10:15 AM)

Hello, I need help with X.
---
"#;
        let parsed = parse_stored_message(content);
        assert_eq!(parsed.channel.as_deref(), Some("email"));
        assert_eq!(parsed.uid.as_deref(), Some("12345"));
        assert_eq!(parsed.topic.as_deref(), Some("Help me"));
        assert_eq!(parsed.sender.as_deref(), Some("John Doe"));
        assert_eq!(parsed.timestamp.as_deref(), Some("10:15 AM"));
        assert_eq!(parsed.body.trim(), "Hello, I need help with X.");
    }

    // --- parse_stored_reply tests ---

    #[test]
    fn test_parse_stored_reply_simple() {
        let content = "Here is my response.\n\nWith details.\n---\n### User (10:00)\n> quoted";
        let reply = parse_stored_reply(content);
        assert_eq!(reply, "Here is my response.\n\nWith details.");
    }

    #[test]
    fn test_parse_stored_reply_no_divider() {
        let content = "Just a reply with no history.";
        let reply = parse_stored_reply(content);
        assert_eq!(reply, "Just a reply with no history.");
    }

    // --- format_quoted_reply tests ---

    #[test]
    fn test_format_quoted_reply() {
        let result = format_quoted_reply("John", "2026-03-22 14:30", "Topic", "Hello\nWorld");
        assert!(result.contains("---"));
        assert!(result.contains("### John (2026-03-22 14:30)"));
        assert!(result.contains("> Topic"));
        assert!(result.contains("> Hello"));
        assert!(result.contains("> World"));
    }

    #[test]
    fn test_format_quoted_reply_empty_body() {
        let result = format_quoted_reply("John", "2026-03-22 14:30", "Topic", "");
        assert!(result.is_empty());
    }

    #[test]
    fn test_format_quoted_reply_sender_with_brackets() {
        let result = format_quoted_reply("John <john@example.com>", "2026-03-22 14:30", "", "Hi");
        assert!(result.contains("### John (2026-03-22 14:30)"));
        assert!(!result.contains("<john@example.com>"));
    }
}
