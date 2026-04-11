//! GitHub inbound adapter implementation.
//!
//! This module handles receiving messages from GitHub via polling.
//! It provides channel-specific pattern matching and thread name derivation.

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::channels::types::{
    ChannelMatcher, ChannelPattern, InboundAdapterOptions, InboundMessage, MessageContent,
    PatternMatch,
};

use super::client::GitHubClient;
use super::config::GitHubConfig;

pub struct GitHubInboundAdapter {
    config: GitHubConfig,
    channel_name: String,
    workspace_root: std::path::PathBuf,
    last_poll_timestamp: std::sync::Mutex<Option<String>>,
}

impl GitHubInboundAdapter {
    pub fn new(config: &GitHubConfig, channel_name: String) -> Self {
        let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

        Self {
            config: config.clone(),
            channel_name,
            workspace_root,
            last_poll_timestamp: std::sync::Mutex::new(None),
        }
    }

    #[allow(dead_code)]
    pub fn new_with_workspace(
        config: &GitHubConfig,
        channel_name: String,
        workspace_root: std::path::PathBuf,
    ) -> Self {
        Self {
            config: config.clone(),
            channel_name,
            workspace_root,
            last_poll_timestamp: std::sync::Mutex::new(None),
        }
    }

    fn get_last_poll_timestamp(&self) -> Option<String> {
        self.last_poll_timestamp.lock().unwrap().clone()
    }

    fn set_last_poll_timestamp(&self, timestamp: &str) {
        *self.last_poll_timestamp.lock().unwrap() = Some(timestamp.to_string());
    }
}

impl ChannelMatcher for GitHubInboundAdapter {
    fn channel_type(&self) -> &str {
        "github"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        _patterns: &[ChannelPattern],
        _pattern_match: Option<&PatternMatch>,
    ) -> String {
        let issue_number = message.metadata.get("issue_number")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .unwrap_or_else(|| message.channel_uid.clone());
        
        format!("github-{}", issue_number)
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        github_match_message(message, patterns)
    }

    fn store_unmatched_messages(&self) -> bool {
        true
    }
}

pub fn github_match_message(
    message: &InboundMessage,
    patterns: &[ChannelPattern],
) -> Option<PatternMatch> {
    for pattern in patterns {
        if !pattern.enabled {
            continue;
        }

        let mut matches = true;
        let mut match_details = HashMap::new();

        if let Some(ref labels) = pattern.rules.labels {
            let msg_labels = message.metadata.get("labels")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            let label_matches = labels.iter().any(|l| msg_labels.contains(&l.to_lowercase()));

            if !label_matches {
                matches = false;
            } else {
                match_details.insert(
                    "labels".to_string(),
                    labels.join(","),
                );
            }
        }

        if matches {
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

                    if let Some(ref regex_str) = sender_rule.regex {
                        any_rule_present = true;
                        if let Ok(re) = regex::Regex::new(regex_str) {
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
        }

        if matches {
            return Some(PatternMatch {
                pattern_name: pattern.name.clone(),
                channel: "github".to_string(),
                matches: match_details,
            });
        }
    }

    None
}

#[async_trait]
impl crate::channels::types::InboundAdapter for GitHubInboundAdapter {
    async fn start(
        &self,
        options: InboundAdapterOptions,
        cancel: CancellationToken,
    ) -> Result<()> {
        let client = Arc::new(GitHubClient::new(self.config.clone()));
        let poll_interval = Duration::from_secs(self.config.poll_interval_secs);

        loop {
            if cancel.is_cancelled() {
                break;
            }

            tracing::info!("Polling GitHub for new events...");

            let since = self.get_last_poll_timestamp();
            let now: DateTime<Utc> = Utc::now();

            if let Err(e) = self.poll_events(&client, &since, &options).await {
                tracing::error!(error = %e, "Error polling GitHub");
            }

            self.set_last_poll_timestamp(&now.to_rfc3339());

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(poll_interval) => continue,
            }
        }

        tracing::info!("GitHub inbound adapter stopped");
        Ok(())
    }
}

impl GitHubInboundAdapter {
    async fn poll_events(
        &self,
        client: &GitHubClient,
        since: &Option<String>,
        options: &InboundAdapterOptions,
    ) -> Result<()> {
        if self.config.events.contains(&"issue_comment".to_string()) {
            let comments = client.get_issue_comments(since.as_deref()).await
                .context("Failed to fetch issue comments")?;

            for comment in comments {
                let mut metadata = HashMap::new();
                metadata.insert("repo".to_string(), serde_json::Value::String(format!("{}/{}", self.config.owner, self.config.repo)));
                metadata.insert("action".to_string(), serde_json::Value::String("created".to_string()));
                metadata.insert("event_type".to_string(), serde_json::Value::String("issue_comment".to_string()));
                metadata.insert("comment_id".to_string(), serde_json::Value::Number(comment.id.into()));

                if let Some(issue_num) = comment.html_url.split("/issues/").nth(1) {
                    if let Some(num) = issue_num.split('#').next() {
                        if let Ok(n) = num.parse::<i64>() {
                            metadata.insert("issue_number".to_string(), serde_json::Value::Number(n.into()));
                        }
                    }
                }

                let message = InboundMessage {
                    id: Uuid::new_v4().to_string(),
                    channel: "github".to_string(),
                    channel_uid: comment.id.to_string(),
                    sender: comment.user.login.clone(),
                    sender_address: comment.user.id.to_string(),
                    recipients: vec![],
                    topic: "".to_string(),
                    content: MessageContent {
                        text: Some(comment.body.clone()),
                        html: None,
                        markdown: Some(comment.body.clone()),
                    },
                    timestamp: DateTime::parse_from_rfc3339(&comment.created_at)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                    thread_refs: None,
                    reply_to_id: metadata.get("issue_number").and_then(|v| v.as_i64()).map(|n| n.to_string()),
                    external_id: Some(comment.id.to_string()),
                    attachments: vec![],
                    metadata,
                    matched_pattern: None,
                };

                (options.on_message)(message)?;
            }
        }

        if self.config.events.contains(&"issues".to_string()) {
            let issues = client.get_issues(since.as_deref(), None).await
                .context("Failed to fetch issues")?;

            for issue in issues {
                if issue.pull_request.is_some() {
                    continue;
                }

                let labels: Vec<String> = issue.labels.iter().map(|l| l.name.clone()).collect();
                let labels_json: Vec<serde_json::Value> = labels.iter().map(|l| serde_json::Value::String(l.clone())).collect();

                let mut metadata = HashMap::new();
                metadata.insert("repo".to_string(), serde_json::Value::String(format!("{}/{}", self.config.owner, self.config.repo)));
                metadata.insert("action".to_string(), serde_json::Value::String(issue.state.clone()));
                metadata.insert("event_type".to_string(), serde_json::Value::String("issues".to_string()));
                metadata.insert("issue_number".to_string(), serde_json::Value::Number(issue.number.into()));
                metadata.insert("labels".to_string(), serde_json::Value::Array(labels_json));
                metadata.insert("html_url".to_string(), serde_json::Value::String(issue.html_url.clone()));

                let body = issue.body.unwrap_or_default();

                let message = InboundMessage {
                    id: Uuid::new_v4().to_string(),
                    channel: "github".to_string(),
                    channel_uid: issue.number.to_string(),
                    sender: issue.user.login.clone(),
                    sender_address: issue.user.id.to_string(),
                    recipients: vec![],
                    topic: issue.title.clone(),
                    content: MessageContent {
                        text: Some(body.clone()),
                        html: None,
                        markdown: Some(body.clone()),
                    },
                    timestamp: DateTime::parse_from_rfc3339(&issue.created_at)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                    thread_refs: None,
                    reply_to_id: Some(issue.number.to_string()),
                    external_id: Some(issue.id.to_string()),
                    attachments: vec![],
                    metadata,
                    matched_pattern: None,
                };

                (options.on_message)(message)?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::types::{MessageContent, PatternRules, SenderRule};

    fn make_github_message(
        sender_addr: &str,
        body: &str,
        issue_number: i64,
        labels: Vec<&str>,
    ) -> InboundMessage {
        let labels_json: Vec<serde_json::Value> = labels
            .iter()
            .map(|l| serde_json::Value::String(l.to_string()))
            .collect();

        let mut metadata = HashMap::new();
        metadata.insert(
            "issue_number".to_string(),
            serde_json::Value::Number(issue_number.into()),
        );
        metadata.insert(
            "labels".to_string(),
            serde_json::Value::Array(labels_json),
        );

        InboundMessage {
            id: "test".to_string(),
            channel: "github".to_string(),
            channel_uid: issue_number.to_string(),
            sender: "testuser".to_string(),
            sender_address: sender_addr.to_string(),
            recipients: vec![],
            topic: "Test Issue".to_string(),
            content: MessageContent {
                text: Some(body.to_string()),
                html: None,
                markdown: Some(body.to_string()),
            },
            timestamp: Utc::now(),
            thread_refs: None,
            reply_to_id: Some(issue_number.to_string()),
            external_id: Some("123".to_string()),
            attachments: vec![],
            metadata,
            matched_pattern: None,
        }
    }

    fn make_github_pattern(
        name: &str,
        labels: Option<Vec<String>>,
        sender: Option<SenderRule>,
    ) -> ChannelPattern {
        ChannelPattern {
            name: name.to_string(),
            channel: "github".to_string(),
            enabled: true,
            rules: PatternRules {
                sender,
                labels,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_match_by_labels() {
        let msg = make_github_message("user1", "Hello", 42, vec!["bug", "urgent"]);
        let patterns = vec![make_github_pattern(
            "bug_triage",
            Some(vec!["bug".to_string()]),
            None,
        )];

        let result = github_match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "bug_triage");
    }

    #[test]
    fn test_no_match_wrong_labels() {
        let msg = make_github_message("user1", "Hello", 42, vec!["enhancement"]);
        let patterns = vec![make_github_pattern(
            "bug_triage",
            Some(vec!["bug".to_string()]),
            None,
        )];

        assert!(github_match_message(&msg, &patterns).is_none());
    }

    #[test]
    fn test_match_by_sender() {
        let msg = make_github_message("12345", "Hello", 42, vec![]);
        let patterns = vec![make_github_pattern(
            "vip_user",
            None,
            Some(SenderRule {
                exact: Some(vec!["12345".to_string()]),
                ..Default::default()
            }),
        )];

        assert!(github_match_message(&msg, &patterns).is_some());
    }

    #[test]
    fn test_derive_thread_name() {
        let msg = make_github_message("user1", "Hello", 42, vec![]);
        let matcher = GitHubInboundAdapter::new(&GitHubConfig::default(), "test".to_string());
        let name = matcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "github-42");
    }

    #[test]
    fn test_disabled_pattern_skipped() {
        let msg = make_github_message("user1", "Hello", 42, vec!["bug"]);
        let mut pattern = make_github_pattern("bug_triage", Some(vec!["bug".to_string()]), None);
        pattern.enabled = false;

        assert!(github_match_message(&msg, &[pattern]).is_none());
    }

    #[test]
    fn test_empty_rules_matches_all() {
        let msg = make_github_message("user1", "Hello", 42, vec![]);
        let patterns = vec![make_github_pattern("catch_all", None, None)];

        assert!(github_match_message(&msg, &patterns).is_some());
    }
}