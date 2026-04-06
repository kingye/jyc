//! Message formatting utilities for Feishu.
//!
//! This module provides utilities for formatting messages between
//! JYC internal format and Feishu API format.

use anyhow::Result;
use serde_json::Value;

use super::config::FeishuConfig;

/// Format message for Feishu API
pub struct FeishuFormatter {
    config: FeishuConfig,
}

impl FeishuFormatter {
    /// Create a new formatter
    pub fn new(config: FeishuConfig) -> Self {
        Self { config }
    }

    /// Format text message for Feishu API
    pub fn format_text_message(&self, text: &str) -> Result<Value> {
        match self.config.message_format.as_str() {
            "markdown" => self.format_markdown_message(text),
            "text" => self.format_plain_text_message(text),
            "html" => self.format_html_message(text),
            _ => self.format_markdown_message(text), // default to markdown
        }
    }

    /// Format markdown message
    fn format_markdown_message(&self, text: &str) -> Result<Value> {
        let content = format!(r#"{{"text":"{}"}}"#, escape_json_string(text));
        Ok(serde_json::json!({
            "msg_type": "text",
            "content": content,
        }))
    }

    /// Format plain text message
    fn format_plain_text_message(&self, text: &str) -> Result<Value> {
        let content = format!(r#"{{"text":"{}"}}"#, escape_json_string(text));
        Ok(serde_json::json!({
            "msg_type": "text",
            "content": content,
        }))
    }

    /// Format HTML message
    fn format_html_message(&self, _text: &str) -> Result<Value> {
        // TODO: Implement HTML formatting
        // For now, fall back to plain text
        self.format_plain_text_message(_text)
    }

    /// Format alert message with subject
    pub fn format_alert_message(&self, subject: &str, body: &str) -> Result<Value> {
        let formatted_text = match self.config.message_format.as_str() {
            "markdown" => format!("**{}**\n\n{}", escape_markdown(subject), body),
            "text" => format!("{}:\n\n{}", subject, body),
            "html" => format!("<strong>{}</strong><br><br>{}", escape_html(subject), body),
            _ => format!("**{}**\n\n{}", escape_markdown(subject), body),
        };

        self.format_text_message(&formatted_text)
    }

    /// Format progress update message
    pub fn format_progress_message(&self, activity: &str, elapsed_ms: u64) -> Result<Value> {
        let formatted_text = match self.config.message_format.as_str() {
            "markdown" => format!(
                "⏳ {} ({}ms elapsed)",
                escape_markdown(activity),
                elapsed_ms
            ),
            "text" => format!("[In progress] {} ({}ms)", activity, elapsed_ms),
            "html" => format!("⏳ {} ({}ms elapsed)", escape_html(activity), elapsed_ms),
            _ => format!(
                "⏳ {} ({}ms elapsed)",
                escape_markdown(activity),
                elapsed_ms
            ),
        };

        self.format_text_message(&formatted_text)
    }
}

/// Escape special characters for JSON string
fn escape_json_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Escape markdown special characters
fn escape_markdown(s: &str) -> String {
    s.replace('*', "\\*")
        .replace('_', "\\_")
        .replace('`', "\\`")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('(', "\\(")
        .replace(')', "\\)")
        .replace('#', "\\#")
        .replace('+', "\\+")
        .replace('-', "\\-")
        .replace('.', "\\.")
        .replace('!', "\\!")
}

/// Escape HTML special characters
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_json_string() {
        assert_eq!(escape_json_string(r#"test"quote"#), r#"test\"quote"#);
        assert_eq!(escape_json_string("test\nnewline"), r#"test\nnewline"#);
        assert_eq!(escape_json_string("test\\backslash"), r#"test\\backslash"#);
    }

    #[test]
    fn test_escape_markdown() {
        assert_eq!(escape_markdown("*bold*"), r#"\*bold\*"#);
        assert_eq!(escape_markdown("_italic_"), r#"\_italic\_"#);
        assert_eq!(escape_markdown("`code`"), r#"\`code\`"#);
    }

    #[test]
    fn test_escape_html() {
        assert_eq!(escape_html("<tag>"), "&lt;tag&gt;");
        assert_eq!(escape_html("\"quote\""), "&quot;quote&quot;");
        assert_eq!(escape_html("&amp;"), "&amp;amp;");
    }

    #[test]
    fn test_formatter_creation() {
        let config = FeishuConfig::default();
        let formatter = FeishuFormatter::new(config);
        // Just verify it doesn't panic
        assert!(true);
    }

    #[test]
    fn test_format_text_message() -> Result<()> {
        let config = FeishuConfig::default();
        let formatter = FeishuFormatter::new(config);

        let result = formatter.format_text_message("Hello, world!")?;
        assert!(result.is_object());
        assert_eq!(result["msg_type"], "text");
        assert!(result["content"]
            .as_str()
            .unwrap()
            .contains("Hello, world!"));

        Ok(())
    }

    #[test]
    fn test_format_alert_message() -> Result<()> {
        let config = FeishuConfig::default();
        let formatter = FeishuFormatter::new(config);

        let result = formatter.format_alert_message("Alert", "Something happened")?;
        assert!(result.is_object());
        assert_eq!(result["msg_type"], "text");
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("Alert"));
        assert!(content.contains("Something happened"));

        Ok(())
    }
}
