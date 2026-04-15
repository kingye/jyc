use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

use crate::channels::types::{
    ChannelMatcher, ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage,
    PatternMatch,
};
use super::config::GithubConfig;

/// GitHub channel matcher — stateless pattern matching for GitHub events.
pub struct GithubMatcher;

impl ChannelMatcher for GithubMatcher {
    fn channel_type(&self) -> &str {
        "github"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        // Thread name derived from metadata: issue-{N} or pr-{N}
        let github_type = message
            .metadata
            .get("github_type")
            .and_then(|v| v.as_str())
            .unwrap_or("issue");
        let number = message
            .metadata
            .get("github_number")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        match github_type {
            "pull_request" => format!("pr-{}", number),
            _ => format!("issue-{}", number),
        }
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        let github_type = message
            .metadata
            .get("github_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let labels: Vec<String> = message
            .metadata
            .get("github_labels")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        for pattern in patterns {
            if !pattern.enabled {
                continue;
            }

            // Check github_type rule
            if let Some(ref type_rules) = pattern.rules.github_type {
                if !type_rules.iter().any(|t| t == github_type) {
                    continue;
                }
            }

            // Check labels rule (OR logic: match if ANY label matches)
            if let Some(ref label_rules) = pattern.rules.labels {
                let has_match = label_rules
                    .iter()
                    .any(|rule| labels.contains(&rule.to_lowercase()));
                if !has_match {
                    continue;
                }
            }

            // All present rules matched
            return Some(PatternMatch {
                pattern_name: pattern.name.clone(),
                channel: "github".to_string(),
                matches: HashMap::new(),
            });
        }

        None
    }

    fn store_unmatched_messages(&self) -> bool {
        false
    }
}

/// GitHub inbound adapter — polls GitHub API for events.
pub struct GithubInboundAdapter {
    config: GithubConfig,
    channel_name: String,
}

impl GithubInboundAdapter {
    pub fn new(config: &GithubConfig, channel_name: String) -> Self {
        Self {
            config: config.clone(),
            channel_name,
        }
    }
}

#[async_trait]
impl ChannelMatcher for GithubInboundAdapter {
    fn channel_type(&self) -> &str {
        "github"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        GithubMatcher.derive_thread_name(message, patterns, pattern_match)
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        GithubMatcher.match_message(message, patterns)
    }
}

#[async_trait]
impl InboundAdapter for GithubInboundAdapter {
    async fn start(
        &self,
        _options: InboundAdapterOptions,
        cancel: CancellationToken,
    ) -> Result<()> {
        tracing::info!(
            channel = %self.channel_name,
            owner = %self.config.owner,
            repo = %self.config.repo,
            poll_interval = %self.config.poll_interval_secs,
            "GitHub inbound adapter started (stub — no polling yet)"
        );

        // Phase 1: Just wait for cancellation
        cancel.cancelled().await;

        tracing::info!(
            channel = %self.channel_name,
            "GitHub inbound adapter stopped"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_message(github_type: &str, number: u64, labels: &[&str]) -> InboundMessage {
        let mut metadata = HashMap::new();
        metadata.insert(
            "github_type".to_string(),
            serde_json::Value::String(github_type.to_string()),
        );
        metadata.insert(
            "github_number".to_string(),
            serde_json::json!(number),
        );
        metadata.insert(
            "github_labels".to_string(),
            serde_json::json!(labels),
        );

        InboundMessage {
            id: "test".to_string(),
            channel: "test_github".to_string(),
            channel_uid: format!("{}-{}", github_type, number),
            sender: "user1".to_string(),
            sender_address: "user1".to_string(),
            recipients: vec![],
            topic: format!("#{} Test issue", number),
            content: crate::channels::types::MessageContent {
                text: Some("github event".to_string()),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: None,
            attachments: vec![],
            metadata,
            matched_pattern: None,
        }
    }

    fn make_patterns() -> Vec<ChannelPattern> {
        vec![
            ChannelPattern {
                name: "planner".to_string(),
                enabled: true,
                rules: crate::channels::types::PatternRules {
                    github_type: Some(vec!["issue".to_string()]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ChannelPattern {
                name: "developer".to_string(),
                enabled: true,
                rules: crate::channels::types::PatternRules {
                    github_type: Some(vec!["pull_request".to_string()]),
                    labels: Some(vec!["ready-for-dev".to_string()]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ChannelPattern {
                name: "reviewer".to_string(),
                enabled: true,
                rules: crate::channels::types::PatternRules {
                    github_type: Some(vec!["pull_request".to_string()]),
                    labels: Some(vec!["ready-for-review".to_string()]),
                    ..Default::default()
                },
                ..Default::default()
            },
        ]
    }

    #[test]
    fn test_derive_thread_name_issue() {
        let msg = make_message("issue", 42, &[]);
        let name = GithubMatcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "issue-42");
    }

    #[test]
    fn test_derive_thread_name_pr() {
        let msg = make_message("pull_request", 43, &[]);
        let name = GithubMatcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "pr-43");
    }

    #[test]
    fn test_match_issue_to_planner() {
        let msg = make_message("issue", 42, &[]);
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "planner");
    }

    #[test]
    fn test_match_pr_with_dev_label() {
        let msg = make_message("pull_request", 43, &["ready-for-dev"]);
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "developer");
    }

    #[test]
    fn test_match_pr_with_review_label() {
        let msg = make_message("pull_request", 43, &["ready-for-review"]);
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "reviewer");
    }

    #[test]
    fn test_match_pr_without_matching_label() {
        let msg = make_message("pull_request", 43, &["wip"]);
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        // No pattern matches: developer requires ready-for-dev, reviewer requires ready-for-review
        assert!(result.is_none());
    }

    #[test]
    fn test_match_disabled_pattern_skipped() {
        let msg = make_message("issue", 42, &[]);
        let patterns = vec![ChannelPattern {
            name: "planner".to_string(),
            enabled: false,
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }
}
