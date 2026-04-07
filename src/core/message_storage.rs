use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};

use crate::channels::types::{AttachmentConfig, InboundMessage, MessageAttachment};
use crate::utils::helpers::parse_file_size;

/// Result of storing a message.
#[derive(Debug, Clone)]
pub struct StoreResult {
    /// Full path to the thread directory
    pub thread_path: PathBuf,
    /// Name of the message directory (e.g., "2026-03-19_23-02-20")
    pub message_dir: String,
    /// Full path to the message directory
    #[allow(dead_code)]
    pub message_path: PathBuf,
}

/// Persist messages and replies as markdown files per thread.
pub struct MessageStorage {
    /// Base workspace directory
    workspace: PathBuf,
}

impl MessageStorage {
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
        }
    }

    /// Store an inbound message with match status.
    ///
    /// Creates the thread directory and message subdirectory,
    /// saves attachments (if configured), writes received.md,
    /// and appends to chat log.
    pub async fn store_with_match(
        &self,
        message: &InboundMessage,
        thread_name: &str,
        is_matched: bool,
        attachment_config: Option<&AttachmentConfig>,
    ) -> Result<StoreResult> {
        let thread_path = self.workspace.join(thread_name);
        let message_dir = self.make_message_dir_name();
        let message_path = thread_path.join("messages").join(&message_dir);

        // Create directories
        tokio::fs::create_dir_all(&message_path)
            .await
            .with_context(|| {
                format!("failed to create message dir: {}", message_path.display())
            })?;

        // Save attachments first (so we can include saved_path in received.md)
        let mut saved_attachments = Vec::new();
        if let Some(att_config) = attachment_config {
            if att_config.enabled {
                saved_attachments =
                    self.save_attachments(&message.attachments, &message_path, att_config)
                        .await?;
            }
        }

        // Write received.md
        let content = self.format_received_md(message, &saved_attachments);
        let received_path = message_path.join("received.md");
        tokio::fs::write(&received_path, &content)
            .await
            .with_context(|| {
                format!("failed to write {}", received_path.display())
            })?;

        tracing::info!(
            thread = %thread_name,
            message_dir = %message_dir,
            attachments = saved_attachments.len(),
            "Message stored"
        );

        // Also append to chat log (dual-write mode)
        self.append_to_chat_log(&thread_path, message, is_matched).await?;

        Ok(StoreResult {
            thread_path,
            message_dir,
            message_path,
        })
    }

    /// Store an inbound message (backward compatibility).
    ///
    /// Calls store_with_match with is_matched = true.
    pub async fn store(
        &self,
        message: &InboundMessage,
        thread_name: &str,
        attachment_config: Option<&AttachmentConfig>,
    ) -> Result<StoreResult> {
        self.store_with_match(message, thread_name, true, attachment_config).await
    }

    /// Store a reply for an existing message.
    pub async fn store_reply(
        &self,
        thread_path: &Path,
        reply_text: &str,
        message_dir: &str,
    ) -> Result<()> {
        let reply_path = thread_path
            .join("messages")
            .join(message_dir)
            .join("reply.md");

        tokio::fs::write(&reply_path, reply_text)
            .await
            .with_context(|| format!("failed to write {}", reply_path.display()))?;

        tracing::debug!(path = %reply_path.display(), "Reply stored");
        
        // Also append to chat log (dual-write mode)
        self.append_reply_to_chat_log(thread_path, reply_text, message_dir).await?;
        
        Ok(())
    }

    /// Append a message to the chat log.
    async fn append_to_chat_log(
        &self,
        thread_path: &Path,
        message: &InboundMessage,
        is_matched: bool,
    ) -> Result<()> {
        use crate::core::chat_log_store::ChatLogStore;
        
        let mut chat_log = ChatLogStore::new(thread_path);
        chat_log.append_message(message, is_matched)
            .with_context(|| format!("Failed to append to chat log in {}", thread_path.display()))?;
        
        tracing::debug!("Message appended to chat log");
        Ok(())
    }

    /// Append a reply to the chat log.
    async fn append_reply_to_chat_log(
        &self,
        thread_path: &Path,
        reply_text: &str,
        _message_dir: &str,
    ) -> Result<()> {
        use crate::core::chat_log_store::{ChatLogStore, ReplyMetadata};
        
        // For now, use simple metadata
        let metadata = ReplyMetadata {
            sender: "jyc-bot".to_string(),
            subject: "Re: Message".to_string(),
            model: None,
            mode: None,
        };
        
        let mut chat_log = ChatLogStore::new(thread_path);
        chat_log.append_reply(reply_text, &metadata)
            .with_context(|| format!("Failed to append reply to chat log in {}", thread_path.display()))?;
        
        tracing::debug!("Reply appended to chat log");
        Ok(())
    }

    /// Generate a unique message directory name based on current timestamp.
    /// Handles collisions by appending a counter suffix.
    fn make_message_dir_name(&self) -> String {
        Utc::now().format("%Y-%m-%d_%H-%M-%S").to_string()
    }

    /// Save allowed attachments to the message directory.
    async fn save_attachments(
        &self,
        attachments: &[MessageAttachment],
        message_path: &Path,
        config: &AttachmentConfig,
    ) -> Result<Vec<SavedAttachment>> {
        let max_size = config
            .max_file_size
            .as_deref()
            .map(parse_file_size)
            .transpose()?;
        let max_count = config.max_per_message.unwrap_or(10);
        let allowed_ext: Vec<String> = config
            .allowed_extensions
            .iter()
            .map(|e| e.to_lowercase())
            .collect();

        let mut saved = Vec::new();

        for att in attachments {
            if saved.len() >= max_count {
                tracing::debug!(
                    filename = %att.filename,
                    "Skipping attachment: max count reached"
                );
                break;
            }

            // Check extension
            let ext = Path::new(&att.filename)
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
                .unwrap_or_default();

            if !allowed_ext.is_empty() && !allowed_ext.contains(&ext) {
                tracing::debug!(
                    filename = %att.filename,
                    ext = %ext,
                    "Skipping attachment: extension not allowed"
                );
                saved.push(SavedAttachment {
                    filename: att.filename.clone(),
                    content_type: att.content_type.clone(),
                    size: att.size,
                    status: "skipped".to_string(),
                    path: None,
                });
                continue;
            }

            // Check size
            if let Some(max) = max_size {
                if att.size as u64 > max {
                    tracing::debug!(
                        filename = %att.filename,
                        size = att.size,
                        max = max,
                        "Skipping attachment: too large"
                    );
                    saved.push(SavedAttachment {
                        filename: att.filename.clone(),
                        content_type: att.content_type.clone(),
                        size: att.size,
                        status: "skipped".to_string(),
                        path: None,
                    });
                    continue;
                }
            }

            // Sanitize filename (basename only, no traversal)
            let safe_name = sanitize_attachment_filename(&att.filename);
            let target = resolve_collision(message_path, &safe_name).await;

            if let Some(ref content) = att.content {
                tokio::fs::write(&target, content)
                    .await
                    .with_context(|| {
                        format!("failed to save attachment: {}", target.display())
                    })?;

                tracing::debug!(
                    filename = %safe_name,
                    size = att.size,
                    "Attachment saved"
                );

                saved.push(SavedAttachment {
                    filename: att.filename.clone(),
                    content_type: att.content_type.clone(),
                    size: att.size,
                    status: "saved".to_string(),
                    path: Some(target),
                });
            }
        }

        Ok(saved)
    }

    /// Format a received.md file with YAML frontmatter.
    fn format_received_md(
        &self,
        message: &InboundMessage,
        saved_attachments: &[SavedAttachment],
    ) -> String {
        let mut md = String::new();

        // YAML frontmatter — includes all metadata needed by the MCP reply tool.
        // The reply tool reads these fields from disk instead of trusting the AI-passed token.
        md.push_str("---\n");
        md.push_str(&format!("channel: {}\n", message.channel));
        md.push_str(&format!("uid: \"{}\"\n", message.channel_uid));
        md.push_str(&format!("sender: \"{}\"\n", message.sender));
        md.push_str(&format!("sender_address: \"{}\"\n", message.sender_address));
        if let Some(ref ext_id) = message.external_id {
            md.push_str(&format!("external_id: \"{ext_id}\"\n"));
        }
        if let Some(ref reply_to) = message.reply_to_id {
            md.push_str(&format!("reply_to_id: \"{reply_to}\"\n"));
        }
        if let Some(ref refs) = message.thread_refs {
            if !refs.is_empty() {
                let refs_str = refs.iter()
                    .map(|r| format!("\"{r}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                md.push_str(&format!("thread_refs: [{refs_str}]\n"));
            }
        }
        if let Some(ref pattern) = message.matched_pattern {
            md.push_str(&format!("matched_pattern: \"{pattern}\"\n"));
        }
        md.push_str(&format!("topic: \"{}\"\n", message.topic));
        md.push_str(&format!(
            "timestamp: \"{}\"\n",
            message.timestamp.to_rfc3339()
        ));
        md.push_str("---\n\n");

        // Header line
        let time_str = message.timestamp.format("%H:%M").to_string();
        md.push_str(&format!(
            "## {} ({})\n\n",
            message.sender, time_str
        ));

        // Body
        let body = message
            .content
            .text
            .as_deref()
            .or(message.content.markdown.as_deref())
            .unwrap_or("[no text content]");
        md.push_str(body);
        md.push('\n');

        // Attachments summary
        if !saved_attachments.is_empty() {
            md.push_str("\n*Attachments:*\n");
            for att in saved_attachments {
                md.push_str(&format!(
                    "  - **{}** ({}, {} bytes) {}\n",
                    att.filename, att.content_type, att.size, att.status
                ));
            }
        }

        md.push_str("---\n");
        md
    }
}

/// A saved (or skipped) attachment record for inclusion in received.md.
#[derive(Debug)]
struct SavedAttachment {
    filename: String,
    content_type: String,
    size: usize,
    status: String,
    #[allow(dead_code)]
    path: Option<PathBuf>,
}

/// Sanitize an attachment filename: basename only, no traversal.
fn sanitize_attachment_filename(filename: &str) -> String {
    let basename = Path::new(filename)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unnamed".to_string());

    // Remove null bytes and control chars
    let cleaned: String = basename
        .chars()
        .filter(|c| !c.is_control() && *c != '\0')
        .collect();

    // Limit length
    if cleaned.len() > 200 {
        let ext = Path::new(&cleaned)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let stem = &cleaned[..200 - ext.len().min(200)];
        format!("{stem}{ext}")
    } else if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

/// Resolve filename collisions by appending a counter suffix.
async fn resolve_collision(dir: &Path, filename: &str) -> PathBuf {
    let target = dir.join(filename);
    if !target.exists() {
        return target;
    }

    let stem = Path::new(filename)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let ext = Path::new(filename)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    for i in 2..=100 {
        let candidate = dir.join(format!("{stem}_{i}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }

    // Fallback: UUID suffix
    dir.join(format!("{stem}_{}{ext}", uuid::Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::MessageContent;
    use chrono::Utc;
    use std::collections::HashMap;

    fn test_message() -> InboundMessage {
        InboundMessage {
            id: "test-id".to_string(),
            channel: "email".to_string(),
            channel_uid: "42".to_string(),
            sender: "John Doe".to_string(),
            sender_address: "john@example.com".to_string(),
            recipients: vec!["me@example.com".to_string()],
            topic: "Help with X".to_string(),
            content: MessageContent {
                text: Some("Hello, I need help.".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: Some("<abc@mail.example.com>".to_string()),
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: Some("support".to_string()),
        }
    }

    #[tokio::test]
    async fn test_store_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = MessageStorage::new(tmp.path());
        let msg = test_message();

        let result = storage.store(&msg, "test-thread", None).await.unwrap();

        assert!(result.thread_path.exists());
        assert!(result.message_path.exists());

        let received = tokio::fs::read_to_string(result.message_path.join("received.md"))
            .await
            .unwrap();
        assert!(received.contains("channel: email"));
        assert!(received.contains("uid: \"42\""));
        assert!(received.contains("## John Doe"));
        assert!(received.contains("Hello, I need help."));
        assert!(received.contains("matched_pattern: \"support\""));
    }

    #[tokio::test]
    async fn test_store_reply() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = MessageStorage::new(tmp.path());
        let msg = test_message();

        let result = storage.store(&msg, "test-thread", None).await.unwrap();
        storage
            .store_reply(&result.thread_path, "Here is my reply.", &result.message_dir)
            .await
            .unwrap();

        let reply_path = result.thread_path
            .join("messages")
            .join(&result.message_dir)
            .join("reply.md");
        let reply = tokio::fs::read_to_string(reply_path).await.unwrap();
        assert_eq!(reply, "Here is my reply.");
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_attachment_filename("report.pdf"), "report.pdf");
        assert_eq!(
            sanitize_attachment_filename("../../../etc/passwd"),
            "passwd"
        );
        assert_eq!(
            sanitize_attachment_filename("path/to/file.txt"),
            "file.txt"
        );
        assert_eq!(sanitize_attachment_filename(""), "unnamed");
    }

    #[tokio::test]
    async fn test_collision_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        tokio::fs::write(tmp.path().join("file.txt"), "a").await.unwrap();

        let resolved = resolve_collision(tmp.path(), "file.txt").await;
        assert_eq!(
            resolved.file_name().unwrap().to_string_lossy(),
            "file_2.txt"
        );
    }
}
