use std::collections::HashMap;

use chrono::{DateTime, Utc};
use mail_parser::MimeHeaders;
use regex::Regex;
use uuid::Uuid;

use crate::channels::types::{
    ChannelPattern, InboundMessage, MessageAttachment, MessageContent, PatternMatch,
};
use crate::core::email_parser;
use crate::utils::helpers::extract_domain;

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

    // Extract body — prefer text, fall back to HTML→markdown
    let text_body = parsed.body_text(0).map(|s| s.to_string());
    let html_body = parsed.body_html(0).map(|s| s.to_string());

    let markdown_body = if text_body.is_none() {
        html_body
            .as_deref()
            .map(crate::services::smtp::client::html_to_markdown)
    } else {
        None
    };

    // Clean the text body at the boundary
    let cleaned_text = text_body.map(|t| email_parser::clean_email_body(&t));

    // Clean the subject at the boundary
    let cleaned_subject = email_parser::strip_reply_prefix(&subject);

    // Extract date
    let timestamp = parsed
        .date()
        .and_then(|d| DateTime::from_timestamp(d.to_timestamp(), 0))
        .unwrap_or_else(Utc::now);

    // Extract attachments
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
            text: cleaned_text,
            html: html_body,
            markdown: markdown_body,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::{PatternRules, SenderRule, SubjectRule};

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
            rules: PatternRules { sender, subject },
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
}
