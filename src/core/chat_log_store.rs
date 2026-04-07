//! Chat log storage for persisting conversation history in log file format.
//!
//! Replaces the timestamp directory approach with daily rolling log files.
//! Each thread gets its own chat history files named `chat_history_YYYY-MM-DD.md`.

use anyhow::{Context, Result};
use chrono::Utc;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::channels::types::InboundMessage;

/// Metadata for reply messages.
#[derive(Debug, Clone)]
pub struct ReplyMetadata {
    pub sender: String,
    pub subject: String,
    pub model: Option<String>,
    pub mode: Option<String>,
}

/// Chat log store for a single thread.
pub struct ChatLogStore {
    thread_path: PathBuf,
    current_file: RwLock<Option<File>>,
    current_date: String,
    max_file_size: u64,
}

impl ChatLogStore {
    /// Create a new chat log store for the given thread.
    pub fn new(thread_path: &Path) -> Self {
        let current_date = Utc::now().format("%Y-%m-%d").to_string();

        Self {
            thread_path: thread_path.to_path_buf(),
            current_file: RwLock::new(None),
            current_date,
            max_file_size: 100 * 1024 * 1024, // 100 MB default
        }
    }

    /// Get the path for today's chat history file.
    fn get_today_file_path(&self) -> PathBuf {
        self.thread_path
            .join(format!("chat_history_{}.md", self.current_date))
    }

    /// Ensure the current file is open and ready for writing.
    fn ensure_file_open(&self) -> Result<()> {
        let mut file_guard = self.current_file.write().unwrap();

        if file_guard.is_none() {
            let file_path = self.get_today_file_path();

            // Create directory if it doesn't exist
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file_path)
                .with_context(|| {
                    format!("Failed to open chat log file: {}", file_path.display())
                })?;

            *file_guard = Some(file);
        }

        Ok(())
    }

    /// Append a received message to the chat log.
    pub fn append_message(&mut self, message: &InboundMessage, is_matched: bool) -> Result<()> {
        self.ensure_file_open()?;

        let matched_str = if is_matched {
            "matched:true"
        } else {
            "matched:false"
        };
        let external_id_str = message
            .external_id
            .as_deref()
            .map_or(String::new(), |id| format!(" | external_id:{}", id));

        let content = message
            .content
            .text
            .as_deref()
            .or(message.content.markdown.as_deref())
            .unwrap_or("[no text content]");

        let mut formatted = String::new();

        // Metadata comment
        formatted.push_str(&format!(
            "<!-- {} | type:received | {} | sender:{} | channel:{}{} -->\n",
            message.timestamp.to_rfc3339(),
            matched_str,
            message.sender_address,
            message.channel,
            external_id_str
        ));

        // Content header
        formatted.push_str(&format!("**FROM:** {}\n", message.sender_address));
        formatted.push_str(&format!("**SUBJECT:** {}\n\n", message.topic));

        // Content body
        formatted.push_str(content);
        formatted.push_str("\n\n---\n");

        self.append_formatted(&formatted)
    }

    /// Append a reply message to the chat log.
    pub fn append_reply(&mut self, reply_text: &str, metadata: &ReplyMetadata) -> Result<()> {
        self.ensure_file_open()?;

        let model_str = metadata
            .model
            .as_deref()
            .map_or(String::new(), |m| format!(" | model:{}", m));
        let mode_str = metadata
            .mode
            .as_deref()
            .map_or(String::new(), |m| format!(" | mode:{}", m));

        let mut formatted = String::new();

        // Metadata comment
        formatted.push_str(&format!(
            "<!-- {} | type:reply{} | matched:true | sender:{} | channel:jyc{} -->\n",
            Utc::now().to_rfc3339(),
            model_str,
            metadata.sender,
            mode_str
        ));

        // Content header
        formatted.push_str(&format!("**REPLY-FROM:** {}\n", metadata.sender));
        if !metadata.subject.is_empty() && metadata.subject != "Re:" {
            formatted.push_str(&format!("**SUBJECT:** {}\n\n", metadata.subject));
        } else {
            formatted.push_str("\n");
        }

        // Content body
        formatted.push_str(reply_text);
        formatted.push_str("\n\n---\n");

        self.append_formatted(&formatted)
    }

    /// Append formatted content to the log file.
    fn append_formatted(&mut self, content: &str) -> Result<()> {
        let mut file_guard = self.current_file.write().unwrap();
        if let Some(ref mut file) = *file_guard {
            writeln!(file, "{}", content)?;
            file.flush()?;

            // Check if we need to rotate due to size
            let metadata = file.metadata()?;
            if metadata.len() > self.max_file_size {
                drop(file_guard); // Release the lock before calling rotate_file
                self.rotate_file()?;
            }
        }

        Ok(())
    }

    /// Rotate to a new file (e.g., when current file is too large).
    fn rotate_file(&mut self) -> Result<()> {
        let mut file_guard = self.current_file.write().unwrap();
        *file_guard = None; // Close current file

        // Update date for new file
        let new_date = Utc::now().format("%Y-%m-%d").to_string();
        if new_date != self.current_date {
            // Date changed, will open new file on next write
            self.current_date = new_date;
        } else {
            // Same day, but file too large - add sequence number
            let timestamp = Utc::now().format("%Y-%m-%d_%H-%M-%S").to_string();
            self.current_date = format!("{}_{}", self.current_date, timestamp);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::{InboundMessage, MessageContent};
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn create_test_message() -> InboundMessage {
        InboundMessage {
            id: "test-1".to_string(),
            channel: "feishu_bot".to_string(),
            channel_uid: "oc_test".to_string(),
            sender: "Test User".to_string(),
            sender_address: "ou_test".to_string(),
            recipients: vec![],
            topic: "Test Subject".to_string(),
            content: MessageContent {
                text: Some("Hello, this is a test message.".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: Some("om_test".to_string()),
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        }
    }

    #[test]
    fn test_chat_log_store_creation() {
        let temp_dir = tempdir().unwrap();
        let store = ChatLogStore::new(temp_dir.path());

        assert_eq!(store.thread_path, temp_dir.path());
        assert!(store.current_file.read().unwrap().is_none());
    }

    #[test]
    fn test_append_message() {
        let temp_dir = tempdir().unwrap();
        let mut store = ChatLogStore::new(temp_dir.path());

        let message = create_test_message();
        let result = store.append_message(&message, true);

        assert!(result.is_ok());

        // Verify file was created
        let file_path = store.get_today_file_path();
        assert!(file_path.exists());

        // Read file content
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("<!--"));
        assert!(content.contains("type:received"));
        assert!(content.contains("matched:true"));
        assert!(content.contains("Hello, this is a test message."));
        assert!(content.contains("---"));
    }

    #[test]
    fn test_append_reply() {
        let temp_dir = tempdir().unwrap();
        let mut store = ChatLogStore::new(temp_dir.path());

        let metadata = ReplyMetadata {
            sender: "jyc-bot".to_string(),
            subject: "Re: Test Subject".to_string(),
            model: Some("ark/deepseek-v3.2".to_string()),
            mode: Some("build".to_string()),
        };

        let result = store.append_reply("This is a test reply.", &metadata);

        assert!(result.is_ok());

        let file_path = store.get_today_file_path();
        let content = std::fs::read_to_string(&file_path).unwrap();

        assert!(content.contains("type:reply"));
        assert!(content.contains("model:ark/deepseek-v3.2"));
        assert!(content.contains("mode:build"));
        assert!(content.contains("This is a test reply."));
    }

    #[test]
    fn test_multiple_appends() {
        let temp_dir = tempdir().unwrap();
        let mut store = ChatLogStore::new(temp_dir.path());

        let message = create_test_message();
        store.append_message(&message, true).unwrap();

        let metadata = ReplyMetadata {
            sender: "jyc-bot".to_string(),
            subject: "Re: Test".to_string(),
            model: None,
            mode: None,
        };
        store.append_reply("Reply 1", &metadata).unwrap();

        store.append_message(&message, false).unwrap();
        store.append_reply("Reply 2", &metadata).unwrap();

        let file_path = store.get_today_file_path();
        let content = std::fs::read_to_string(&file_path).unwrap();

        let lines: Vec<&str> = content.split("---\n").collect();
        // Should have 4 entries + maybe empty trailing
        assert!(lines.len() >= 4);

        // Count message types
        let received_count = content.matches("type:received").count();
        let reply_count = content.matches("type:reply").count();

        assert_eq!(received_count, 2);
        assert_eq!(reply_count, 2);
    }
}
