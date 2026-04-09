use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use mail_parser::MimeHeaders;
use regex::Regex;
use uuid::Uuid;

use crate::channels::types::{
    ChannelMatcher, ChannelPattern, InboundMessage, MessageAttachment, MessageContent, PatternMatch,
};
use crate::config::types::InboundAttachmentConfig;
use crate::core::email_parser;
use crate::utils::helpers::{extract_domain, sanitize_for_filesystem};

/// Email-specific pattern matching and thread name derivation.
///
/// Stateless struct implementing `ChannelMatcher` — can be cheaply created
/// wherever email pattern matching is needed (e.g., ImapMonitor, tests).
pub struct EmailMatcher;

impl ChannelMatcher for EmailMatcher {
    fn channel_type(&self) -> &str {
        "email"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        let subject_prefixes: Vec<String> = patterns
            .iter()
            .filter_map(|p| p.rules.subject.as_ref())
            .filter_map(|s| s.prefix.as_ref())
            .flatten()
            .cloned()
            .collect();
        email_parser::derive_thread_name(&message.topic, &subject_prefixes)
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        match_message(message, patterns)
    }
}

/// Parse raw email bytes into an InboundMessage.
///
/// This is the boundary where data is cleaned:
/// - Subject: reply prefixes stripped
/// - Body: cleaned via clean_email_body
/// - HTML-only emails: converted to markdown via htmd
pub fn parse_raw_email(raw: &[u8], uid: u32) -> anyhow::Result<InboundMessage> {
    let parsed = mail_parser::MessageParser::default()
        .parse(raw)
        .ok_or_else(|| anyhow::anyhow!("failed to parse email"))?;

    // Extract basic headers
    let from = parsed
        .from()
        .and_then(|a| a.first())
        .map(|a| {
            (
                a.name().unwrap_or("").to_string(),
                a.address().unwrap_or("").to_string(),
            )
        })
        .unwrap_or_default();

    let to: Vec<String> = parsed
        .to()
        .map(|addrs| {
            addrs
                .iter()
                .filter_map(|a| a.address().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let subject = parsed.subject().unwrap_or("").to_string();
    let message_id = parsed.message_id().map(|s| s.to_string());

    // Extract threading headers
    let in_reply_to = parsed.in_reply_to().as_text().map(|s| s.to_string());

    let references: Option<Vec<String>> = {
        let refs = parsed.references();
        if let Some(list) = refs.as_text_list() {
            Some(list.into_iter().map(|s| s.to_string()).collect())
        } else {
            refs.as_text().map(|s| vec![s.to_string()])
        }
    };

    // Extract body — prefer HTML→markdown (preserves line breaks from <br>/<p>/<div>),
    // fall back to raw plain text only when no HTML is available.
    // Mobile email clients often generate poor plain text (no line breaks between
    // user content and quoted replies), while the HTML part has proper structure.
    let text_body = parsed.body_text(0).map(|s| s.to_string());
    let html_body = parsed.body_html(0).map(|s| s.to_string());

    let best_text = if let Some(ref html) = html_body {
        // HTML→markdown preserves line breaks from tags
        let md = crate::services::smtp::client::html_to_markdown(html);
        let cleaned = email_parser::clean_email_body(&md);
        if cleaned.trim().is_empty() {
            // HTML conversion produced nothing useful, fall back to plain text
            text_body.map(|t| email_parser::clean_email_body(&t))
        } else {
            Some(cleaned)
        }
    } else {
        text_body.map(|t| email_parser::clean_email_body(&t))
    };

    // Clean the subject at the boundary
    let cleaned_subject = email_parser::strip_reply_prefix(&subject);

    // Extract date
    let timestamp = parsed
        .date()
        .and_then(|d| DateTime::from_timestamp(d.to_timestamp(), 0))
        .unwrap_or_else(Utc::now);

    // Extract attachments
    let attachments_iter = parsed.attachments();
    let attachments_count = attachments_iter.count();
    tracing::debug!("Email UID={}: Found {} attachments via attachments()", uid, attachments_count);
    
    // Reset iterator after counting
    let attachments: Vec<MessageAttachment> = parsed
        .attachments()
        .map(|att| {
            let filename = att.attachment_name().unwrap_or("unnamed").to_string();
            let content_type = att
                .content_type()
                .map(|ct| {
                    let subtype = ct.subtype().unwrap_or("octet-stream");
                    format!("{}/{}", ct.ctype(), subtype)
                })
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let content = att.contents().to_vec();
            let size = content.len();

            tracing::debug!("Email UID={}: Attachment '{}' ({} bytes, {})", uid, filename, size, content_type);
            MessageAttachment {
                filename,
                content_type,
                size,
                content: Some(content),
                saved_path: None,
            }
        })
        .collect();

    // Build metadata
    let mut metadata = HashMap::new();
    if let Some(ref reply_to) = in_reply_to {
        metadata.insert(
            "in_reply_to".to_string(),
            serde_json::Value::String(reply_to.clone()),
        );
    }
    metadata.insert(
        "from".to_string(),
        serde_json::Value::String(from.1.clone()),
    );

    Ok(InboundMessage {
        id: Uuid::new_v4().to_string(),
        channel: "email".to_string(),
        channel_uid: uid.to_string(),
        sender: if from.0.is_empty() {
            from.1.clone()
        } else {
            from.0.clone()
        },
        sender_address: from.1,
        recipients: to,
        topic: cleaned_subject,
        content: MessageContent {
            text: best_text,
            html: html_body,
            markdown: None,
        },
        timestamp,
        thread_refs: references,
        reply_to_id: in_reply_to,
        external_id: message_id,
        attachments,
        metadata,
        matched_pattern: None,
    })
}

/// Match a message against email-specific patterns.
///
/// Rules within a pattern use AND logic — all present rules must match.
/// Returns the first matching pattern.
pub fn match_message(
    message: &InboundMessage,
    patterns: &[ChannelPattern],
) -> Option<PatternMatch> {
    for pattern in patterns {
        if !pattern.enabled {
            continue;
        }

        let mut matches = true;
        let mut match_details = HashMap::new();

        // Check sender rules
        if let Some(ref sender_rule) = pattern.rules.sender {
            let addr = message.sender_address.to_lowercase();

            let sender_matches = {
                let mut any_rule_present = false;
                let mut any_rule_matched = false;

                if let Some(ref exact_addrs) = sender_rule.exact {
                    any_rule_present = true;
                    if exact_addrs.iter().any(|e| e.to_lowercase() == addr) {
                        any_rule_matched = true;
                        match_details.insert("sender.exact".to_string(), addr.clone());
                    }
                }

                if let Some(ref domains) = sender_rule.domain {
                    any_rule_present = true;
                    if let Some(domain) = extract_domain(&addr) {
                        if domains.iter().any(|d| d.to_lowercase() == domain) {
                            any_rule_matched = true;
                            match_details.insert("sender.domain".to_string(), domain);
                        }
                    }
                }

                if let Some(ref regex_str) = sender_rule.regex {
                    any_rule_present = true;
                    if let Ok(re) = Regex::new(regex_str) {
                        if re.is_match(&addr) {
                            any_rule_matched = true;
                            match_details.insert("sender.regex".to_string(), addr.clone());
                        }
                    }
                }

                !any_rule_present || any_rule_matched
            };

            if !sender_matches {
                matches = false;
            }
        }

        // Check subject rules
        if matches {
            if let Some(ref subject_rule) = pattern.rules.subject {
                let subj = message.topic.to_lowercase();

                let subject_matches = {
                    let mut any_rule_present = false;
                    let mut any_rule_matched = false;

                    if let Some(ref prefixes) = subject_rule.prefix {
                        any_rule_present = true;
                        if prefixes.iter().any(|p| subj.starts_with(&p.to_lowercase())) {
                            any_rule_matched = true;
                            match_details.insert("subject.prefix".to_string(), subj.clone());
                        }
                    }

                    if let Some(ref regex_str) = subject_rule.regex {
                        any_rule_present = true;
                        if let Ok(re) = Regex::new(regex_str) {
                            if re.is_match(&subj) {
                                any_rule_matched = true;
                                match_details.insert("subject.regex".to_string(), subj.clone());
                            }
                        }
                    }

                    !any_rule_present || any_rule_matched
                };

                if !subject_matches {
                    matches = false;
                }
            }
        }

        if matches {
            return Some(PatternMatch {
                pattern_name: pattern.name.clone(),
                channel: "email".to_string(),
                matches: match_details,
            });
        }
    }

    None
}

/// Email inbound adapter for receiving messages via IMAP.
pub struct EmailInboundAdapter {
    /// Channel name from config (e.g., "email_bot")
    channel_name: String,
    /// Workspace root path (e.g., "/home/jiny/projects/jyc-data/feishu_bot/workspace/")
    workspace_root: std::path::PathBuf,
}

impl EmailInboundAdapter {
    /// Create a new Email inbound adapter.
    pub fn new(channel_name: String) -> Self {
        // Determine workspace root from current working directory
        let workspace_root = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        
        Self {
            channel_name,
            workspace_root,
        }
    }
    
    /// Create a new Email inbound adapter with custom workspace root.
    #[allow(dead_code)]
    pub fn new_with_workspace(channel_name: String, workspace_root: std::path::PathBuf) -> Self {
        Self {
            channel_name,
            workspace_root,
        }
    }

    /// Save attachments to thread directory.
    pub async fn save_attachments_to_thread_directory(
        &self,
        message: &mut InboundMessage,
        patterns: &[ChannelPattern],
        attachment_config: Option<&InboundAttachmentConfig>,
    ) -> Result<()> {
        // Check if we have attachments to save
        if message.attachments.is_empty() {
            tracing::debug!("No attachments to save for message");
            return Ok(());
        }

        // Derive thread name using EmailMatcher
        let thread_name = EmailMatcher.derive_thread_name(message, patterns, None);
        
        tracing::debug!(
            "Saving {} attachments to thread directory for thread: {}",
            message.attachments.len(),
            thread_name
        );

        // Determine the thread directory
        // Format: <workspace_root>/<channel_name>/workspace/<thread_name>/
        let thread_dir = self.workspace_root
            .join(&self.channel_name)
            .join("workspace")
            .join(&thread_name);

        // Determine save path: use configured path or default to thread_dir/attachments/
        let save_dir = match attachment_config.and_then(|c| c.save_path.as_deref()) {
            Some(path) => {
                // If path is relative, make it relative to thread_dir
                let path_buf = std::path::PathBuf::from(path);
                if path_buf.is_absolute() {
                    path_buf
                } else {
                    thread_dir.join(path_buf)
                }
            }
            None => {
                // Default path: <thread_dir>/attachments/
                thread_dir.join("attachments")
            }
        };

        tracing::debug!("Attachment save directory: {}", save_dir.display());

        // Ensure directory exists
        tokio::fs::create_dir_all(&save_dir).await
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
            let filename = self.generate_attachment_filename(attachment);
            
            // Full file path
            let file_path = save_dir.join(&filename);

            tracing::debug!("Saving attachment to: {}", file_path.display());

            // Write file content
            if let Some(content) = &attachment.content {
                tokio::fs::write(&file_path, content).await
                    .context(format!("Failed to write attachment file: {}", attachment.filename))?;
                
                // Update saved_path
                attachment.saved_path = Some(file_path.clone());
                
                tracing::info!(
                    "Attachment saved to thread directory: {} ({} bytes) -> {}",
                    attachment.filename,
                    attachment.size,
                    file_path.display()
                );
            }
        }

        tracing::debug!("All attachments saved successfully");
        Ok(())
    }

    /// Generate a unique filename for an attachment.
    fn generate_attachment_filename(&self, attachment: &MessageAttachment) -> String {
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let uuid_short = uuid::Uuid::new_v4().to_string()[..8].to_string();
        
        // Sanitize original filename
        let safe_name = sanitize_for_filesystem(&attachment.filename);
        
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::{PatternRules, SenderRule, SubjectRule};
    use tempfile::tempdir;

    fn make_message(sender_addr: &str, subject: &str) -> InboundMessage {
        InboundMessage {
            id: "test".to_string(),
            channel: "email".to_string(),
            channel_uid: "1".to_string(),
            sender: "Test".to_string(),
            sender_address: sender_addr.to_string(),
            recipients: vec![],
            topic: subject.to_string(),
            content: MessageContent::default(),
            timestamp: Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata: HashMap::new(),
            matched_pattern: None,
        }
    }

    fn make_pattern(
        name: &str,
        sender: Option<SenderRule>,
        subject: Option<SubjectRule>,
    ) -> ChannelPattern {
        ChannelPattern {
            name: name.to_string(),
            channel: "email".to_string(),
            enabled: true,
            rules: PatternRules {
                sender,
                subject,
                mentions: None,
                keywords: None,
                chat_name: None,
            },
            attachments: None,
        }
    }

    #[test]
    fn test_match_exact_sender() {
        let msg = make_message("user@example.com", "Hello");
        let patterns = vec![make_pattern(
            "test",
            Some(SenderRule {
                exact: Some(vec!["user@example.com".to_string()]),
                ..Default::default()
            }),
            None,
        )];

        let result = match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "test");
    }

    #[test]
    fn test_match_exact_sender_case_insensitive() {
        let msg = make_message("User@Example.COM", "Hello");
        let patterns = vec![make_pattern(
            "test",
            Some(SenderRule {
                exact: Some(vec!["user@example.com".to_string()]),
                ..Default::default()
            }),
            None,
        )];

        assert!(match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_match_domain() {
        let msg = make_message("anyone@company.com", "Hello");
        let patterns = vec![make_pattern(
            "test",
            Some(SenderRule {
                domain: Some(vec!["company.com".to_string()]),
                ..Default::default()
            }),
            None,
        )];

        assert!(match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_match_subject_prefix() {
        let msg = make_message("user@example.com", "jiny: Build the app");
        let patterns = vec![make_pattern(
            "test",
            None,
            Some(SubjectRule {
                prefix: Some(vec!["jiny".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_match_and_logic() {
        // Both sender AND subject must match
        let msg = make_message("user@example.com", "jiny: Task");
        let patterns = vec![make_pattern(
            "test",
            Some(SenderRule {
                exact: Some(vec!["user@example.com".to_string()]),
                ..Default::default()
            }),
            Some(SubjectRule {
                prefix: Some(vec!["jiny".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(match_message(&msg, &patterns).is_some());

        // Wrong sender → no match even with correct subject
        let msg2 = make_message("other@example.com", "jiny: Task");
        assert!(match_message(&msg2, &patterns).is_none());
    }

    #[test]
    fn test_match_disabled_pattern_skipped() {
        let msg = make_message("user@example.com", "Hello");
        let mut pattern = make_pattern(
            "test",
            Some(SenderRule {
                exact: Some(vec!["user@example.com".to_string()]),
                ..Default::default()
            }),
            None,
        );
        pattern.enabled = false;

        assert!(match_message(&msg, &[pattern]).is_none());
    }

    #[test]
    fn test_match_first_pattern_wins() {
        let msg = make_message("user@example.com", "Hello");
        let patterns = vec![
            make_pattern(
                "first",
                Some(SenderRule {
                    exact: Some(vec!["user@example.com".to_string()]),
                    ..Default::default()
                }),
                None,
            ),
            make_pattern(
                "second",
                Some(SenderRule {
                    domain: Some(vec!["example.com".to_string()]),
                    ..Default::default()
                }),
                None,
            ),
        ];

        let result = match_message(&msg, &patterns).unwrap();
        assert_eq!(result.pattern_name, "first");
    }

    #[test]
    fn test_match_sender_regex() {
        let msg = make_message("user123@company.org", "Hello");
        let patterns = vec![make_pattern(
            "test",
            Some(SenderRule {
                regex: Some(r".*@company\.org".to_string()),
                ..Default::default()
            }),
            None,
        )];

        assert!(match_message(&msg, &patterns).is_some());
    }

    #[tokio::test]
    async fn test_save_attachments_to_thread_directory() {
        // Create a temporary directory for workspace
        let temp_dir = tempdir().unwrap();
        let workspace_root = temp_dir.path().to_path_buf();

        // Create EmailInboundAdapter with custom workspace root
        let adapter = EmailInboundAdapter::new_with_workspace(
            "test_channel".to_string(),
            workspace_root.clone(),
        );

        // Create a test message with attachments
        let mut message = InboundMessage {
            id: "test-msg".to_string(),
            channel: "email".to_string(),
            channel_uid: "123".to_string(),
            sender: "Test Sender".to_string(),
            sender_address: "test@example.com".to_string(),
            recipients: vec![],
            topic: "Test Subject".to_string(),
            content: MessageContent::default(),
            timestamp: Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![
                MessageAttachment {
                    filename: "test1.txt".to_string(),
                    content_type: "text/plain".to_string(),
                    size: 11,
                    content: Some(b"Hello World".to_vec()),
                    saved_path: None,
                },
                MessageAttachment {
                    filename: "test2.pdf".to_string(),
                    content_type: "application/pdf".to_string(),
                    size: 20,
                    content: Some(b"PDF content here...".to_vec()),
                    saved_path: None,
                },
            ],
            metadata: HashMap::new(),
            matched_pattern: None,
        };

        // Empty patterns for test
        let patterns = vec![];

        // Save attachments
        let result = adapter.save_attachments_to_thread_directory(
            &mut message,
            &patterns,
            None,
        ).await;

        // Verify the operation succeeded
        assert!(result.is_ok(), "Failed to save attachments: {:?}", result.err());

        // Verify attachments have saved_path set
        for attachment in &message.attachments {
            assert!(attachment.saved_path.is_some(), 
                "Attachment {} should have saved_path set", attachment.filename);
            
            let saved_path = attachment.saved_path.as_ref().unwrap();
            assert!(saved_path.exists(), 
                "File should exist at: {}", saved_path.display());
            
            // Verify file content
            let content = std::fs::read(saved_path).unwrap();
            assert_eq!(content, *attachment.content.as_ref().unwrap());
        }

        // Verify the directory structure
        let thread_name = EmailMatcher.derive_thread_name(&message, &patterns, None);
        let expected_attachments_dir = workspace_root
            .join(&thread_name)
            .join("attachments");
        
        assert!(expected_attachments_dir.exists(), 
            "Attachments directory should exist: {}", expected_attachments_dir.display());
        
        // List files in directory
        let entries: Vec<_> = std::fs::read_dir(&expected_attachments_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        
        assert_eq!(entries.len(), 2, "Should have 2 files in attachments directory");
        
        // Verify filename patterns
        for entry in entries {
            assert!(entry.contains("_test1") || entry.contains("_test2"), 
                "Filename should contain original name: {}", entry);
            assert!(entry.ends_with(".txt") || entry.ends_with(".pdf"),
                "Filename should preserve extension: {}", entry);
        }
    }
}
