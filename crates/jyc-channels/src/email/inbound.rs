use std::collections::HashMap;

use regex::Regex;

use jyc_core::email_parser;
use jyc_types::{ChannelMatcher, ChannelPattern, InboundMessage, PatternMatch};
use jyc_utils::helpers::extract_domain;

/// Email-specific pattern matching and topic name derivation.
///
/// Stateless struct implementing `ChannelMatcher` — can be cheaply created
/// wherever email pattern matching is needed (e.g., ImapMonitor, tests).
pub struct EmailMatcher;

impl ChannelMatcher for EmailMatcher {
    fn channel_type(&self) -> &str {
        "email"
    }

    fn derive_topic_name(
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
        email_parser::derive_topic_name(&message.topic, &subject_prefixes)
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        match_message(message, patterns)
    }
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
                    if let Some(domain) = extract_domain(&addr)
                        && domains.iter().any(|d| d.to_lowercase() == domain)
                    {
                        any_rule_matched = true;
                        match_details.insert("sender.domain".to_string(), domain);
                    }
                }

                if let Some(ref regex_str) = sender_rule.regex {
                    any_rule_present = true;
                    if let Ok(re) = Regex::new(regex_str)
                        && re.is_match(&addr)
                    {
                        any_rule_matched = true;
                        match_details.insert("sender.regex".to_string(), addr.clone());
                    }
                }

                !any_rule_present || any_rule_matched
            };

            if !sender_matches {
                matches = false;
            }
        }

        // Check subject rules
        if matches && let Some(ref subject_rule) = pattern.rules.subject {
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
                    if let Ok(re) = Regex::new(regex_str)
                        && re.is_match(&subj)
                    {
                        any_rule_matched = true;
                        match_details.insert("subject.regex".to_string(), subj.clone());
                    }
                }

                !any_rule_present || any_rule_matched
            };

            if !subject_matches {
                matches = false;
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
    use chrono::Utc;
    use jyc_types::{MessageContent, PatternRules, SenderRule, SubjectRule};

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
            references: None,
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
                github_type: None,
                labels: None,
                assignees: None,
                exclude_labels: None,
            },
            attachments: None,
            ..Default::default()
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
