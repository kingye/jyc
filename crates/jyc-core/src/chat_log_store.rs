//! Chat log storage for persisting conversation history in JSONL format.
//!
//! Each thread gets its own chat history files named `chat_history_YYYY-MM-DD.jsonl`.
//! Each line is a JSON object representing one message or reply.

use anyhow::{Context, Result};
use chrono::Utc;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use jyc_types::InboundMessage;
use jyc_types::inspect::ChatMessageEntry;

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

/// List chat history JSONL files in a thread directory.
///
/// Tries `.jyc/` first (new location), falls back to thread root (legacy).
/// Returns sorted paths (oldest first) and the directory they were found in.
pub fn list_chat_history_files(thread_path: &Path) -> (Vec<PathBuf>, PathBuf) {
    let new_dir = thread_path.join(".jyc");
    let files = read_chat_history_dir(&new_dir);
    if !files.is_empty() {
        return (files, new_dir);
    }
    // Fallback: legacy location (thread root)
    let files = read_chat_history_dir(thread_path);
    (files, thread_path.to_path_buf())
}

fn read_chat_history_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("chat_history_") && n.ends_with(".jsonl"))
        })
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

/// Load recent chat history entries from a thread directory.
///
/// Reads `chat_history_*.jsonl` files (`.jyc/` first, falls back to thread root),
/// parses each line, and returns up to `max_messages` most recent entries.
/// Entries are mapped: `"received"` → sender `"user"`, `"reply"` → sender `"ai"`.
pub fn load_recent_chat_history(thread_path: &Path, max_messages: usize) -> Vec<ChatMessageEntry> {
    if !thread_path.exists() {
        return vec![];
    }

    let (mut files, _dir) = list_chat_history_files(thread_path);
    files.sort_by(|a, b| b.cmp(a)); // newest first

    let mut entries = Vec::new();
    for file in files {
        if entries.len() >= max_messages {
            break;
        }
        let content = match std::fs::read_to_string(&file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut file_entries: Vec<ChatMessageEntry> = content
            .lines()
            .rev()
            .filter_map(|line| {
                let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
                let msg_type = parsed.get("type")?.as_str()?;
                let content = parsed.get("content")?.as_str()?;
                let sender = match msg_type {
                    "received" => "user",
                    "reply" => "ai",
                    _ => return None,
                };
                Some(ChatMessageEntry {
                    sender: sender.to_string(),
                    text: content.to_string(),
                    timestamp: parsed
                        .get("ts")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                })
            })
            .collect();
        file_entries.reverse();
        entries.splice(0..0, file_entries);
    }

    if entries.len() > max_messages {
        let drain_count = entries.len() - max_messages;
        entries.drain(0..drain_count);
    }
    entries
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
    /// New location: `.jyc/chat_history_YYYY-MM-DD.jsonl`
    fn get_today_file_path(&self) -> PathBuf {
        self.thread_path
            .join(".jyc")
            .join(format!("chat_history_{}.jsonl", self.current_date))
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

        // Extract user_name from metadata (e.g., WeCom KF provides display names)
        let user_name = message
            .metadata
            .get("user_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        // Fallback: read from thread.json if metadata lacks user_name
        let user_name = user_name.or_else(|| {
            if message.channel == "wecomkf"
                && let Ok(Some(thread_json)) =
                    crate::thread_json::ThreadJson::read_sync(&self.thread_path)
                && let Some(data) = thread_json.data
                && let Some(name) = data.get("user_name").and_then(|v| v.as_str())
            {
                Some(name.to_string())
            } else {
                None
            }
        });

        // Compute sender_name (display name)
        let sender_name = user_name.as_deref().or_else(|| {
            if message.sender != message.sender_address && !message.sender.is_empty() {
                Some(message.sender.as_str())
            } else {
                None
            }
        });

        // Compute from_display
        let from_display = if let Some(ref name) = user_name {
            if !message.sender_address.is_empty() {
                format!("{} ({})", name, message.sender_address)
            } else {
                name.clone()
            }
        } else if !message.sender.is_empty() && message.sender != message.sender_address {
            format!("{} ({})", message.sender, message.sender_address)
        } else {
            message.sender_address.clone()
        };

        let content = message
            .content
            .text
            .as_deref()
            .or(message.content.markdown.as_deref())
            .unwrap_or("[no text content]");

        let mut record = serde_json::json!({
            "ts": message.timestamp.to_rfc3339(),
            "type": "received",
            "matched": is_matched,
            "sender": message.sender_address,
            "channel": message.channel,
            "topic": message.topic,
            "from": from_display,
            "content": content,
        });

        if let Some(ref name) = sender_name {
            record["sender_name"] = serde_json::json!(name);
        }

        if let Some(ref ext_id) = message.external_id {
            record["external_id"] = serde_json::json!(ext_id);
        }

        let line = serde_json::to_string(&record)?;
        self.append_formatted(&line)
    }

    /// Append a reply message to the chat log.
    pub fn append_reply(&mut self, reply_text: &str, metadata: &ReplyMetadata) -> Result<()> {
        self.ensure_file_open()?;

        let mut record = serde_json::json!({
            "ts": Utc::now().to_rfc3339(),
            "type": "reply",
            "matched": true,
            "sender": metadata.sender,
            "channel": "jyc",
            "content": reply_text,
        });

        if let Some(ref model) = metadata.model {
            record["model"] = serde_json::json!(model);
        }

        if let Some(ref mode) = metadata.mode {
            record["mode"] = serde_json::json!(mode);
        }

        let line = serde_json::to_string(&record)?;
        self.append_formatted(&line)
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
    use jyc_types::{InboundMessage, MessageContent};
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
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(parsed["type"], "received");
        assert_eq!(parsed["matched"], true);
        assert_eq!(parsed["sender"], "ou_test");
        assert_eq!(parsed["sender_name"], "Test User");
        assert_eq!(parsed["from"], "Test User (ou_test)");
        assert_eq!(parsed["content"], "Hello, this is a test message.");
        assert!(parsed["ts"].is_string());
        assert!(parsed["external_id"].is_string());
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
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(parsed["type"], "reply");
        assert_eq!(parsed["model"], "ark/deepseek-v3.2");
        assert_eq!(parsed["mode"], "build");
        assert_eq!(parsed["content"], "This is a test reply.");
        assert_eq!(parsed["matched"], true);
        assert!(parsed["ts"].is_string());
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

        let lines: Vec<&str> = content.lines().collect();
        // Should have 4 JSONL lines
        assert_eq!(lines.len(), 4);

        // Parse each line and verify types
        let types: Vec<String> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .and_then(|v| v["type"].as_str().map(String::from))
                    .unwrap()
            })
            .collect();
        assert_eq!(types, vec!["received", "reply", "received", "reply"]);

        // Verify matched values
        let matched: Vec<bool> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .and_then(|v| v["matched"].as_bool())
                    .unwrap()
            })
            .collect();
        assert_eq!(matched, vec![true, true, false, true]);
    }

    #[test]
    fn test_append_message_with_thread_json_fallback() {
        let temp_dir = tempdir().unwrap();
        let mut store = ChatLogStore::new(temp_dir.path());

        // Write thread.json with user_name
        let thread_json = crate::thread_json::ThreadJson {
            channel_type: "wecomkf".to_string(),
            version: 1,
            data: Some(serde_json::json!({
                "external_userid": "wm123",
                "user_name": "张三",
            })),
        };
        thread_json.write_sync(temp_dir.path()).unwrap();

        // Create message WITHOUT user_name in metadata
        let mut message = create_test_message();
        message.channel = "wecomkf".to_string();
        message.sender_address = "wecomkf:wm123".to_string();
        message.metadata = HashMap::new();

        let result = store.append_message(&message, true);
        assert!(result.is_ok());

        let file_path = store.get_today_file_path();
        let content = std::fs::read_to_string(&file_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        // Should use user_name from thread.json
        assert_eq!(parsed["sender_name"], "张三");
        assert_eq!(parsed["from"], "张三 (wecomkf:wm123)");
    }

    #[test]
    fn test_load_recent_chat_history_empty_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let entries = load_recent_chat_history(tmp.path(), 100);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_load_recent_chat_history_reads_jyc_subdir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("chat_history_2026-07-22.jsonl"),
            r#"{"ts":"2026-07-22T10:00:00Z","type":"received","matched":true,"sender":"user","channel":"test","topic":"test","from":"user","content":"hello"}"#,
        )
        .unwrap();

        let entries = load_recent_chat_history(tmp.path(), 100);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sender, "user");
        assert_eq!(entries[0].text, "hello");
    }

    #[test]
    fn test_load_recent_chat_history_reads_reply() {
        let tmp = tempfile::TempDir::new().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("chat_history_2026-07-22.jsonl"),
            r#"{"ts":"2026-07-22T10:00:00Z","type":"reply","matched":true,"sender":"bot","channel":"test","content":"AI reply"}"#,
        )
        .unwrap();

        let entries = load_recent_chat_history(tmp.path(), 100);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sender, "ai");
        assert_eq!(entries[0].text, "AI reply");
    }

    #[test]
    fn test_load_recent_chat_history_respects_max() {
        let tmp = tempfile::TempDir::new().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        let mut lines = String::new();
        for i in 0..10 {
            lines.push_str(&format!(
                r#"{{"ts":"2026-07-22T10:00:{:02}Z","type":"received","content":"msg {}"}}"#,
                i, i
            ));
            lines.push('\n');
        }
        std::fs::write(jyc_dir.join("chat_history_2026-07-22.jsonl"), lines).unwrap();

        let entries = load_recent_chat_history(tmp.path(), 3);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].text, "msg 7");
        assert_eq!(entries[2].text, "msg 9");
    }

    #[test]
    fn test_load_recent_chat_history_skips_unknown_types() {
        let tmp = tempfile::TempDir::new().unwrap();
        let jyc_dir = tmp.path().join(".jyc");
        std::fs::create_dir_all(&jyc_dir).unwrap();
        std::fs::write(
            jyc_dir.join("chat_history_2026-07-22.jsonl"),
            concat!(
                r#"{"ts":"2026-07-22T10:00:00Z","type":"received","content":"hello"}"#,
                "\n",
                r#"{"ts":"2026-07-22T10:01:00Z","type":"unknown","content":"skip"}"#,
                "\n",
                r#"{"ts":"2026-07-22T10:02:00Z","type":"reply","content":"world"}"#,
            ),
        )
        .unwrap();

        let entries = load_recent_chat_history(tmp.path(), 100);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "hello");
        assert_eq!(entries[1].text, "world");
    }
}
