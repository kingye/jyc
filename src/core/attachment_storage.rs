//! Channel-agnostic attachment storage utilities.
//!
//! Provides shared logic for saving attachments to thread directories,
//! generating unique filenames, and sanitizing user-provided filenames.
//! Used by both email and Feishu inbound adapters.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::channels::types::{InboundMessage, MessageAttachment};
use crate::config::types::InboundAttachmentConfig;
use crate::utils::helpers::sanitize_for_filesystem;

/// Sanitize an attachment filename to prevent path traversal attacks.
///
/// Strips directory components, normalizes path separators, and removes
/// dangerous characters. Should be called at ingestion time (when creating
/// `MessageAttachment`), not just at save time.
pub fn sanitize_attachment_filename(filename: &str) -> String {
    // Early return for empty/whitespace-only input
    let trimmed = filename.trim();
    if trimmed.is_empty() {
        return "unnamed_attachment".to_string();
    }

    // Strip any directory components (path traversal protection)
    let name = trimmed
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(trimmed)
        .to_string();

    // Remove null bytes and other control characters
    let name: String = name.chars().filter(|c| !c.is_control()).collect();

    // Apply filesystem sanitization
    let safe = sanitize_for_filesystem(&name);

    if safe.is_empty() {
        "unnamed_attachment".to_string()
    } else {
        safe
    }
}

/// Generate a unique filename for an attachment.
///
/// Format: `<timestamp>_<uuid>_<sanitized_name>.<ext>`
///
/// - Preserves the original extension
/// - Truncates the base name to 50 characters
/// - Adds timestamp and short UUID for uniqueness
pub fn generate_attachment_filename(attachment: &MessageAttachment) -> String {
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let uuid_short = uuid::Uuid::new_v4().to_string()[..8].to_string();

    // Sanitize original filename
    let safe_name = sanitize_attachment_filename(&attachment.filename);

    // Preserve extension if possible
    let (name_no_ext, ext) = if let Some(dot_idx) = safe_name.rfind('.') {
        let (name, ext) = safe_name.split_at(dot_idx);
        (name.to_string(), Some(ext.to_string()))
    } else {
        (safe_name, None)
    };

    // Limit name length
    let truncated_name = if name_no_ext.len() > 50 {
        &name_no_ext[..50]
    } else {
        &name_no_ext
    };

    // Build final filename
    let mut final_name = format!("{}_{}_{}", timestamp, uuid_short, truncated_name);
    if let Some(ext) = ext {
        final_name.push_str(&ext);
    }

    final_name
}

/// Save attachments from an inbound message directly to a thread directory.
///
/// Simpler version that takes the resolved thread path directly.
/// Used by the thread manager where the thread path is already known.
pub async fn save_attachments_to_dir(
    message: &mut InboundMessage,
    thread_path: &Path,
    attachment_config: Option<&InboundAttachmentConfig>,
) -> Result<()> {
    if message.attachments.is_empty() {
        tracing::debug!("No attachments to save for message");
        return Ok(());
    }

    // Determine save path: use configured path or default to thread_path/attachments/
    let save_dir = match attachment_config.and_then(|c| c.save_path.as_deref()) {
        Some(path) => {
            let path_buf = PathBuf::from(path);
            if path_buf.is_absolute() {
                path_buf
            } else {
                thread_path.join(path_buf)
            }
        }
        None => thread_path.join("attachments"),
    };

    tracing::debug!("Attachment save directory: {}", save_dir.display());
    tokio::fs::create_dir_all(&save_dir)
        .await
        .context("Failed to create attachment directory")?;

    for (i, attachment) in message.attachments.iter_mut().enumerate() {
        if attachment.content.is_none() {
            tracing::warn!("Attachment has no content: {}", attachment.filename);
            continue;
        }

        let filename = generate_attachment_filename(attachment);
        let file_path = save_dir.join(&filename);

        tracing::debug!("Saving attachment to: {}", file_path.display());

        if let Some(content) = &attachment.content {
            tokio::fs::write(&file_path, content)
                .await
                .context(format!("Failed to write attachment: {}", attachment.filename))?;

            attachment.saved_path = Some(file_path.clone());

            tracing::info!(
                "Attachment saved: {} ({} bytes) -> {}",
                attachment.filename,
                attachment.size,
                file_path.display()
            );
        }
    }

    Ok(())
}

/// Save attachments from an inbound message to the thread directory.
///
/// This is the shared implementation used by all channel adapters.
/// The thread directory path is constructed from workspace_root, channel_name,
/// and thread_name following the convention:
///   `<workspace_root>/<channel_name>/workspace/<thread_name>/attachments/`
///
/// The save path can be overridden via `InboundAttachmentConfig.save_path`.
pub async fn save_attachments_to_thread_directory(
    message: &mut InboundMessage,
    workspace_root: &Path,
    channel_name: &str,
    thread_name: &str,
    attachment_config: Option<&InboundAttachmentConfig>,
) -> Result<()> {
    if message.attachments.is_empty() {
        tracing::debug!("No attachments to save for message");
        return Ok(());
    }

    tracing::debug!(
        "Saving {} attachments to thread directory for thread: {}",
        message.attachments.len(),
        thread_name
    );

    // Determine the thread directory
    let thread_dir = workspace_root
        .join(channel_name)
        .join("workspace")
        .join(thread_name);

    // Determine save path: use configured path or default to thread_dir/attachments/
    let save_dir = match attachment_config.and_then(|c| c.save_path.as_deref()) {
        Some(path) => {
            let path_buf = PathBuf::from(path);
            if path_buf.is_absolute() {
                path_buf
            } else {
                thread_dir.join(path_buf)
            }
        }
        None => thread_dir.join("attachments"),
    };

    tracing::debug!("Attachment save directory: {}", save_dir.display());

    // Ensure directory exists
    tokio::fs::create_dir_all(&save_dir)
        .await
        .context("Failed to create attachment directory")?;

    // Save each attachment
    for (i, attachment) in message.attachments.iter_mut().enumerate() {
        tracing::debug!(
            "Processing attachment {}: {} (size: {}, has content: {})",
            i + 1,
            attachment.filename,
            attachment.size,
            attachment.content.is_some()
        );

        // Skip if no content
        if attachment.content.is_none() {
            tracing::warn!("Attachment has no content: {}", attachment.filename);
            continue;
        }

        // Generate a unique filename
        let filename = generate_attachment_filename(attachment);

        // Full file path
        let file_path = save_dir.join(&filename);

        tracing::debug!("Saving attachment to: {}", file_path.display());

        // Write file content
        if let Some(content) = &attachment.content {
            tokio::fs::write(&file_path, content)
                .await
                .context(format!(
                    "Failed to write attachment file: {}",
                    attachment.filename
                ))?;

            // Update saved_path
            attachment.saved_path = Some(file_path.clone());

            tracing::info!(
                "Attachment saved: {} ({} bytes) -> {}",
                attachment.filename,
                attachment.size,
                file_path.display()
            );
        }
    }

    tracing::debug!("All attachments saved successfully");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::MessageContent;
    use chrono::Utc;
    use std::collections::HashMap;
    use tempfile::tempdir;

    #[test]
    fn test_sanitize_attachment_filename_path_traversal() {
        assert_eq!(
            sanitize_attachment_filename("../../etc/passwd"),
            "passwd"
        );
        assert_eq!(
            sanitize_attachment_filename("..\\..\\windows\\system32\\config"),
            "config"
        );
    }

    #[test]
    fn test_sanitize_attachment_filename_normal() {
        assert_eq!(
            sanitize_attachment_filename("report.pdf"),
            "report.pdf"
        );
        assert_eq!(
            sanitize_attachment_filename("my document (1).docx"),
            "my document (1).docx"
        );
    }

    #[test]
    fn test_sanitize_attachment_filename_empty() {
        assert_eq!(
            sanitize_attachment_filename(""),
            "unnamed_attachment"
        );
        // When the caller provides "unnamed" as fallback for missing names
        assert_eq!(
            sanitize_attachment_filename("unnamed"),
            "unnamed"
        );
    }

    #[test]
    fn test_sanitize_attachment_filename_control_chars() {
        assert_eq!(
            sanitize_attachment_filename("file\x00name.txt"),
            "filename.txt"
        );
    }

    #[test]
    fn test_generate_attachment_filename_preserves_extension() {
        let attachment = MessageAttachment {
            filename: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            size: 100,
            content: None,
            saved_path: None,
        };
        let name = generate_attachment_filename(&attachment);
        assert!(name.ends_with(".pdf"));
        assert!(name.len() > "report.pdf".len()); // Has timestamp + uuid prefix
    }

    #[test]
    fn test_generate_attachment_filename_truncates_long_name() {
        let long_name = "a".repeat(100) + ".txt";
        let attachment = MessageAttachment {
            filename: long_name,
            content_type: "text/plain".to_string(),
            size: 100,
            content: None,
            saved_path: None,
        };
        let name = generate_attachment_filename(&attachment);
        assert!(name.ends_with(".txt"));
        // 15 (timestamp) + 1 (_) + 8 (uuid) + 1 (_) + 50 (truncated) + 4 (.txt) = 79
        assert!(name.len() <= 80);
    }

    #[tokio::test]
    async fn test_save_attachments_to_thread_directory() {
        let tmp = tempdir().unwrap();
        let workspace_root = tmp.path().to_path_buf();

        let mut message = InboundMessage {
            id: "test".to_string(),
            channel: "email".to_string(),
            channel_uid: "1".to_string(),
            sender: "Test".to_string(),
            sender_address: "test@example.com".to_string(),
            recipients: vec![],
            topic: "Test Subject".to_string(),
            content: MessageContent::default(),
            timestamp: Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![MessageAttachment {
                filename: "test.txt".to_string(),
                content_type: "text/plain".to_string(),
                size: 12,
                content: Some(b"test content".to_vec()),
                saved_path: None,
            }],
            metadata: HashMap::new(),
            matched_pattern: None,
        };

        let result = save_attachments_to_thread_directory(
            &mut message,
            &workspace_root,
            "test_channel",
            "test_thread",
            None,
        )
        .await;

        assert!(result.is_ok());

        // Verify attachment was saved
        let expected_dir = workspace_root
            .join("test_channel")
            .join("workspace")
            .join("test_thread")
            .join("attachments");
        assert!(expected_dir.exists());

        // Verify saved_path was set
        assert!(message.attachments[0].saved_path.is_some());
        let saved = message.attachments[0].saved_path.as_ref().unwrap();
        assert!(saved.exists());
        assert!(saved.to_string_lossy().ends_with(".txt"));
    }
}
