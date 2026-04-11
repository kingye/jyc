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
    /// Track processed issue IDs to avoid re-processing unchanged issues
    processed_issue_ids: std::sync::Mutex<std::collections::HashSet<i64>>,
    /// Track processed comment IDs to avoid duplicates
    processed_comment_ids: std::sync::Mutex<std::collections::HashSet<i64>>,
}

impl GitHubInboundAdapter {
    pub fn new(config: &GitHubConfig, channel_name: String) -> Self {
        let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

        Self {
            config: config.clone(),
            channel_name,
            workspace_root,
            last_poll_timestamp: std::sync::Mutex::new(None),
            processed_issue_ids: std::sync::Mutex::new(std::collections::HashSet::new()),
            processed_comment_ids: std::sync::Mutex::new(std::collections::HashSet::new()),
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
            processed_issue_ids: std::sync::Mutex::new(std::collections::HashSet::new()),
            processed_comment_ids: std::sync::Mutex::new(std::collections::HashSet::new()),
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

            tracing::trace!("Polling GitHub for new events...");

            let since = self.get_last_poll_timestamp();
            let now: DateTime<Utc> = Utc::now();

            match self.poll_events(&client, &since, &options).await {
                Ok(()) => {
                    // Only advance timestamp on successful poll
                    // Use ISO 8601 format without nanoseconds (GitHub API requirement)
                    self.set_last_poll_timestamp(&now.format("%Y-%m-%dT%H:%M:%SZ").to_string());
                }
                Err(e) => {
                    // Don't advance timestamp on failure — retry same window next cycle
                    tracing::error!(error = %e, "Error polling GitHub");
                }
            }

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
        // Fetch all open items (issues + PRs) from GitHub.
        // GitHub's /issues API returns both issues and PRs.
        let open_items = client.get_issues(since.as_deref(), Some("open")).await
            .context("Failed to fetch open issues")?;

        // Classify open items into issues and PRs, build lookup sets
        let mut open_issue_numbers = std::collections::HashSet::new();
        let mut open_pr_numbers = std::collections::HashSet::new();
        for item in &open_items {
            if item.pull_request.is_some() {
                open_pr_numbers.insert(item.number);
            } else {
                open_issue_numbers.insert(item.number);
            }
        }

        // All open item numbers (for comment filtering — both issues and PRs)
        let all_open_numbers: std::collections::HashSet<i64> = open_issue_numbers
            .union(&open_pr_numbers)
            .cloned()
            .collect();

        // Process comments on open issues and PRs
        if self.config.events.contains(&"issue_comment".to_string()) {
            self.process_comments(client, since, &all_open_numbers, options).await?;
        }

        // Process new issues
        if self.config.events.contains(&"issues".to_string()) {
            self.process_issues(&open_items, options)?;
        }

        // Process new PRs (routed to linked issue thread or own PR thread)
        if self.config.events.contains(&"pull_request".to_string()) {
            self.process_pull_requests(&open_items, options)?;
        }

        Ok(())
    }

    /// Process new comments on open issues and PRs.
    ///
    /// Comments are routed to the thread of the issue/PR they belong to.
    /// Bot's own comments are skipped to prevent feedback loops.
    async fn process_comments(
        &self,
        client: &GitHubClient,
        since: &Option<String>,
        open_numbers: &std::collections::HashSet<i64>,
        options: &InboundAdapterOptions,
    ) -> Result<()> {
        let comments = client.get_issue_comments(since.as_deref()).await
            .context("Failed to fetch issue comments")?;

        let mut processed = self.processed_comment_ids.lock().unwrap();

        for comment in comments {
            if processed.contains(&comment.id) {
                continue;
            }

            // Skip bot's own comments to prevent feedback loops
            if self.is_bot_comment(&comment) {
                tracing::trace!(comment_id = comment.id, user = %comment.user.login, "Skipping bot comment");
                processed.insert(comment.id);
                continue;
            }

            // Extract issue/PR number from comment URL
            let item_number = Self::extract_item_number_from_url(&comment.html_url);

            // Skip comments on closed items
            if let Some(num) = item_number {
                if !open_numbers.contains(&num) {
                    tracing::trace!(comment_id = comment.id, item = num, "Skipping comment on closed item");
                    processed.insert(comment.id);
                    continue;
                }
            } else {
                tracing::trace!(comment_id = comment.id, url = %comment.html_url, "Skipping comment: cannot extract item number");
                processed.insert(comment.id);
                continue;
            }

            processed.insert(comment.id);

            let issue_number = item_number.unwrap();
            let mut metadata = HashMap::new();
            metadata.insert("repo".to_string(), serde_json::json!(format!("{}/{}", self.config.owner, self.config.repo)));
            metadata.insert("action".to_string(), serde_json::json!("created"));
            metadata.insert("event_type".to_string(), serde_json::json!("issue_comment"));
            metadata.insert("comment_id".to_string(), serde_json::json!(comment.id));
            metadata.insert("issue_number".to_string(), serde_json::json!(issue_number));

            let message = InboundMessage {
                id: Uuid::new_v4().to_string(),
                channel: self.channel_name.clone(),
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
                reply_to_id: Some(issue_number.to_string()),
                external_id: Some(comment.id.to_string()),
                attachments: vec![],
                metadata,
                matched_pattern: None,
            };

            (options.on_message)(message)?;
        }

        Ok(())
    }

    /// Process new issues (not PRs).
    ///
    /// Only open issues that haven't been processed before are sent.
    fn process_issues(
        &self,
        open_items: &[super::types::GitHubIssue],
        options: &InboundAdapterOptions,
    ) -> Result<()> {
        let mut processed = self.processed_issue_ids.lock().unwrap();

        for issue in open_items {
            // Skip PRs — handled by process_pull_requests
            if issue.pull_request.is_some() {
                continue;
            }

            if issue.state != "open" {
                continue;
            }

            if processed.contains(&issue.id) {
                continue;
            }

            processed.insert(issue.id);

            let message = self.build_issue_message(issue, "issues");
            (options.on_message)(message)?;
        }

        Ok(())
    }

    /// Process new PRs.
    ///
    /// PRs are treated similarly to issues. If a PR body references an issue
    /// (e.g., "Fixes #42"), it could be linked to the issue's thread.
    /// Otherwise, it gets its own thread (github-<pr_number>).
    fn process_pull_requests(
        &self,
        open_items: &[super::types::GitHubIssue],
        options: &InboundAdapterOptions,
    ) -> Result<()> {
        let mut processed = self.processed_issue_ids.lock().unwrap();

        for item in open_items {
            // Only PRs
            if item.pull_request.is_none() {
                continue;
            }

            if item.state != "open" {
                continue;
            }

            if processed.contains(&item.id) {
                continue;
            }

            processed.insert(item.id);

            // Check if PR body references an issue (Fixes #N, Closes #N, etc.)
            let linked_issue = item.body.as_deref()
                .and_then(Self::extract_linked_issue_number);

            let mut message = self.build_issue_message(item, "pull_request");

            // If linked to an issue, route to that issue's thread
            if let Some(issue_num) = linked_issue {
                message.metadata.insert("linked_issue".to_string(), serde_json::json!(issue_num));
                message.metadata.insert("issue_number".to_string(), serde_json::json!(issue_num));
                tracing::debug!(
                    pr = item.number,
                    linked_issue = issue_num,
                    "PR linked to issue, routing to issue thread"
                );
            }

            (options.on_message)(message)?;
        }

        Ok(())
    }

    /// Build an InboundMessage from a GitHub issue/PR.
    fn build_issue_message(&self, issue: &super::types::GitHubIssue, event_type: &str) -> InboundMessage {
        let labels: Vec<String> = issue.labels.iter().map(|l| l.name.clone()).collect();
        let labels_json: Vec<serde_json::Value> = labels.iter().map(|l| serde_json::json!(l)).collect();

        let mut metadata = HashMap::new();
        metadata.insert("repo".to_string(), serde_json::json!(format!("{}/{}", self.config.owner, self.config.repo)));
        metadata.insert("action".to_string(), serde_json::json!(issue.state.clone()));
        metadata.insert("event_type".to_string(), serde_json::json!(event_type));
        metadata.insert("issue_number".to_string(), serde_json::json!(issue.number));
        metadata.insert("labels".to_string(), serde_json::Value::Array(labels_json));
        metadata.insert("html_url".to_string(), serde_json::json!(issue.html_url.clone()));

        if issue.pull_request.is_some() {
            metadata.insert("is_pr".to_string(), serde_json::json!(true));
        }

        let body = issue.body.clone().unwrap_or_default();

        InboundMessage {
            id: Uuid::new_v4().to_string(),
            channel: self.channel_name.clone(),
            channel_uid: issue.number.to_string(),
            sender: issue.user.login.clone(),
            sender_address: issue.user.id.to_string(),
            recipients: vec![],
            topic: issue.title.clone(),
            content: MessageContent {
                text: Some(body.clone()),
                html: None,
                markdown: Some(body),
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
        }
    }

    /// Check if a comment was posted by the bot itself.
    fn is_bot_comment(&self, comment: &super::types::GitHubIssueComment) -> bool {
        // Check for [bot] suffix (GitHub App bots)
        if comment.user.login.ends_with("[bot]") {
            return true;
        }
        // Check for reply footer pattern (jyc's reply footer)
        if comment.body.contains("Model:") && comment.body.contains("Mode:") {
            return true;
        }
        false
    }

    /// Extract issue/PR number from a GitHub URL.
    /// Handles both /issues/N and /pull/N URLs.
    fn extract_item_number_from_url(url: &str) -> Option<i64> {
        // Try /issues/N#issuecomment-...
        if let Some(num_str) = url.split("/issues/").nth(1) {
            if let Some(num) = num_str.split('#').next() {
                if let Ok(n) = num.parse::<i64>() {
                    return Some(n);
                }
            }
        }
        // Try /pull/N#...
        if let Some(num_str) = url.split("/pull/").nth(1) {
            if let Some(num) = num_str.split('#').next() {
                if let Ok(n) = num.parse::<i64>() {
                    return Some(n);
                }
            }
        }
        None
    }

    /// Extract linked issue number from PR body.
    /// Looks for patterns like "Fixes #42", "Closes #42", "Resolves #42".
    fn extract_linked_issue_number(body: &str) -> Option<i64> {
        let re = regex::Regex::new(r"(?i)(?:fix(?:es)?|close[sd]?|resolve[sd]?)\s+#(\d+)").ok()?;
        re.captures(body)
            .and_then(|caps| caps.get(1))
            .and_then(|m| m.as_str().parse::<i64>().ok())
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