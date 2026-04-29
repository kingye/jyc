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

/// Strip trailing `---` separators from reply text to prevent duplicate footers.
///
/// This function removes any trailing `---` separators (with optional whitespace)
/// from the end of the reply text. This prevents duplicate separators when the
/// system adds its own footer starting with `---\n\n`.
pub fn strip_trailing_separators(text: &str) -> String {
    let trimmed = text.trim_end();
    
    // Check if text ends with `---` (with optional preceding whitespace)
    if trimmed.ends_with("---") {
        // Find the start of the last `---` sequence
        let without_separators = trimmed
            .trim_end_matches(|c: char| c.is_whitespace() || c == '-')
            .trim_end();
        
        without_separators.to_string()
    } else {
        trimmed.to_string()
    }
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

/// Regex for collapsing excessive blank lines (4+ newlines → 3).
static BLANK_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{4,}").unwrap());

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
    result = BLANK_LINE_RE.replace_all(&result, "\n\n\n").to_string();

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

/// Parse a stored message file (legacy format), extracting all frontmatter and body.
#[derive(Debug)]
pub struct ParsedStoredMessage {
    pub sender: Option<String>,
    #[allow(dead_code)]
    pub sender_address: Option<String>,
    pub timestamp: Option<String>,
    pub topic: Option<String>,
    pub body: String,
    #[allow(dead_code)]
    pub channel: Option<String>,
    #[allow(dead_code)]
    pub uid: Option<String>,
    #[allow(dead_code)]
    pub external_id: Option<String>,
    #[allow(dead_code)]
    pub reply_to_id: Option<String>,
    #[allow(dead_code)]
    pub thread_refs: Option<Vec<String>>,
    #[allow(dead_code)]
    pub matched_pattern: Option<String>,
}

/// Parse a stored message file (legacy format).
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

/// Parse a stored reply file (legacy format), extracting only the AI's response text.
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

/// Build a footer with model, mode, and token information.
///
/// When `footer_enabled` is `false`, returns an empty string immediately.
pub fn build_footer(model: Option<&str>, mode: Option<&str>, input_tokens: Option<u64>, max_tokens: Option<u64>, footer_enabled: bool) -> String {
    if !footer_enabled {
        return String::new();
    }

    let mut parts = Vec::new();

    if let Some(m) = model {
        parts.push(format!("Model: {}", m));
    }
    if let Some(md) = mode {
        parts.push(format!("Mode: {}", md));
    }
    match (input_tokens, max_tokens) {
        (Some(current), Some(max)) => {
            let current_k = current as f64 / 1024.0;
            let max_k = max as f64 / 1024.0;
            parts.push(format!("Tokens: {:.1}K/{:.0}K", current_k, max_k));
        }
        (Some(current), None) => {
            let current_k = current as f64 / 1024.0;
            parts.push(format!("Tokens: {:.1}K", current_k));
        }
        _ => {}
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!("---\n\n{}", parts.join(" | "))
    }
}

/// Build reply text with footer (no quoted history).
///
/// Format:
/// ```text
/// <AI reply text>
///
/// ---
/// Model: <model> | Mode: <mode> | Tokens: <current>K/<max>K
/// ```
pub async fn build_full_reply_text(
    reply_text: &str,
    _thread_path: &std::path::Path,
    _sender: &str,
    _timestamp: &str,
    _topic: &str,
    _body_text: &str,
    _message_dir: &str,
    model: Option<&str>,
    mode: Option<&str>,
    input_tokens: Option<u64>,
    max_tokens: Option<u64>,
    footer_enabled: bool,
) -> String {
    let footer = build_footer(model, mode, input_tokens, max_tokens, footer_enabled);
    
    // Clean reply text to remove any trailing `---` separators
    let clean_reply = strip_trailing_separators(reply_text);

    if footer.is_empty() {
        clean_reply
    } else {
        format!("{}\n\n{}", clean_reply, footer)
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


    // --- strip_trailing_separators tests ---

    #[test]
    fn test_strip_trailing_separators_none() {
        let text = "This is a reply.";
        assert_eq!(strip_trailing_separators(text), "This is a reply.");
    }

    #[test]
    fn test_strip_trailing_separators_single() {
        let text = "This is a reply.\n\n---";
        assert_eq!(strip_trailing_separators(text), "This is a reply.");
    }

    #[test]
    fn test_strip_trailing_separators_with_whitespace() {
        let text = "This is a reply.\n\n---\n";
        assert_eq!(strip_trailing_separators(text), "This is a reply.");
        
        let text2 = "This is a reply.  ---  ";
        assert_eq!(strip_trailing_separators(text2), "This is a reply.");
    }

    #[test]
    fn test_strip_trailing_separators_multiple() {
        let text = "This is a reply.\n\n---\n\n---";
        assert_eq!(strip_trailing_separators(text), "This is a reply.");
    }

    #[test]
    fn test_strip_trailing_separators_only_separators() {
        let text = "---";
        assert_eq!(strip_trailing_separators(text), "");
        
        let text2 = "---\n\n---";
        assert_eq!(strip_trailing_separators(text2), "");
    }

    #[test]
    fn test_strip_trailing_separators_within_body() {
        let text = "This is a --- reply --- with separators inside.";
        assert_eq!(
            strip_trailing_separators(text),
            "This is a --- reply --- with separators inside."
        );
    }

    #[test]
    fn test_strip_trailing_separators_empty() {
        let text = "";
        assert_eq!(strip_trailing_separators(text), "");
        
        let text2 = "   ";
        assert_eq!(strip_trailing_separators(text2), "");
    }

    // --- build_footer tests ---

    #[test]
    fn test_build_footer_enabled_with_model() {
        let footer = build_footer(Some("test-model"), None, None, None, true);
        assert!(footer.contains("Model: test-model"));
        assert!(footer.starts_with("---\n\n"));
    }

    #[test]
    fn test_build_footer_disabled_returns_empty() {
        let footer = build_footer(Some("test-model"), Some("opencode"), Some(5000), Some(120000), false);
        assert!(footer.is_empty());
    }

    #[test]
    fn test_build_footer_disabled_no_model_returns_empty() {
        let footer = build_footer(None, None, None, None, false);
        assert!(footer.is_empty());
    }

    #[test]
    fn test_build_footer_enabled_all_fields() {
        let footer = build_footer(Some("gpt-4"), Some("opencode"), Some(10240), Some(122880), true);
        assert!(footer.contains("Model: gpt-4"));
        assert!(footer.contains("Mode: opencode"));
        assert!(footer.contains("Tokens:"));
    }

    // --- build_full_reply_text footer tests ---

    #[tokio::test]
    async fn test_build_full_reply_text_footer_disabled() {
        let reply = build_full_reply_text(
            "Hello world",
            std::path::Path::new("/tmp"),
            "sender",
            "2026-01-01T00:00:00Z",
            "topic",
            "body",
            "msg",
            Some("model"),
            Some("opencode"),
            Some(5000),
            Some(120000),
            false,
        )
        .await;
        assert_eq!(reply, "Hello world");
        assert!(!reply.contains("Model:"));
    }

    #[tokio::test]
    async fn test_build_full_reply_text_footer_enabled() {
        let reply = build_full_reply_text(
            "Hello world",
            std::path::Path::new("/tmp"),
            "sender",
            "2026-01-01T00:00:00Z",
            "topic",
            "body",
            "msg",
            Some("model"),
            Some("opencode"),
            Some(5000),
            Some(120000),
            true,
        )
        .await;
        assert!(reply.contains("Hello world"));
        assert!(reply.contains("Model: model"));
    }
}
