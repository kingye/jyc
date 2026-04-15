use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use tokio_util::sync::CancellationToken;

use crate::channels::types::{
    ChannelMatcher, ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage,
    MessageContent, PatternMatch,
};
use super::client::GithubClient;
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

    /// Build a minimal InboundMessage from a GitHub event.
    /// Contains only trigger metadata — agent uses `gh` CLI for actual content.
    fn build_trigger_message(
        &self,
        event_type: &str,
        number: u64,
        title: &str,
        github_type: &str,
        action: &str,
        actor: &str,
        labels: &[String],
        event_uid: &str,
    ) -> InboundMessage {
        let label_str = if labels.is_empty() {
            String::new()
        } else {
            format!("labels: {}\n", labels.join(", "))
        };

        let gh_cmd = match github_type {
            "pull_request" => format!(
                "Use `gh pr view {}` to read the PR.\nUse `gh pr view {} --comments` to read comments.\nUse `gh pr diff {}` to see the diff.",
                number, number, number
            ),
            _ => format!(
                "Use `gh issue view {}` to read the full issue.\nUse `gh issue view {} --comments` to read all comments.",
                number, number
            ),
        };

        let body = format!(
            "github event: {}\nnumber: {}\ntype: {}\naction: {}\nactor: {}\n{}\n{}",
            event_type, number, github_type, action, actor, label_str, gh_cmd
        );

        let mut metadata = HashMap::new();
        metadata.insert("github_event".to_string(), serde_json::json!(event_type));
        metadata.insert("github_number".to_string(), serde_json::json!(number));
        metadata.insert("github_type".to_string(), serde_json::json!(github_type));
        metadata.insert("github_action".to_string(), serde_json::json!(action));
        metadata.insert("github_labels".to_string(), serde_json::json!(labels));

        InboundMessage {
            id: uuid::Uuid::new_v4().to_string(),
            channel: self.channel_name.clone(),
            channel_uid: event_uid.to_string(),
            sender: actor.to_string(),
            sender_address: actor.to_string(),
            recipients: vec![],
            topic: format!("#{} {}", number, title),
            content: MessageContent {
                text: Some(body),
                html: None,
                markdown: None,
            },
            timestamp: chrono::Utc::now(),
            thread_refs: None,
            reply_to_id: None,
            external_id: Some(event_uid.to_string()),
            attachments: vec![],
            metadata,
            matched_pattern: None,
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
        options: InboundAdapterOptions,
        cancel: CancellationToken,
    ) -> Result<()> {
        // Create GitHub API client
        let client = GithubClient::new(&self.config)
            .context("Failed to create GitHub client")?;

        // Get bot identity (for logging — not used for comment filtering)
        let bot_user = match client.get_authenticated_user().await {
            Ok(user) => user.login,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to get bot identity, continuing without");
                "unknown".to_string()
            }
        };

        tracing::info!(
            channel = %self.channel_name,
            owner = %self.config.owner,
            repo = %self.config.repo,
            bot_user = %bot_user,
            poll_interval = %self.config.poll_interval_secs,
            "GitHub inbound adapter started"
        );

        // Track processed event IDs for deduplication
        let mut processed_events: HashSet<String> = HashSet::new();

        // Start polling from 5 minutes ago to catch recent events.
        // Deduplication ensures we don't process the same event twice.
        let mut last_poll = (chrono::Utc::now() - chrono::Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();

        let poll_interval = tokio::time::Duration::from_secs(self.config.poll_interval_secs);

        // Polling loop
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(channel = %self.channel_name, "GitHub polling cancelled");
                    break;
                }
                _ = tokio::time::sleep(poll_interval) => {
                    if let Err(e) = self.poll_once(
                        &client,
                        &options,
                        &mut processed_events,
                        &mut last_poll,
                    ).await {
                        tracing::error!(
                            channel = %self.channel_name,
                            error = %e,
                            "GitHub poll cycle failed"
                        );
                        (options.on_error)(e);
                    }
                }
            }
        }

        tracing::info!(channel = %self.channel_name, "GitHub inbound adapter stopped");
        Ok(())
    }
}

impl GithubInboundAdapter {
    /// Execute one poll cycle: fetch issues, comments, and closed items.
    async fn poll_once(
        &self,
        client: &GithubClient,
        options: &InboundAdapterOptions,
        processed_events: &mut HashSet<String>,
        last_poll: &mut String,
    ) -> Result<()> {
        let poll_start = last_poll.clone();

        tracing::debug!(
            channel = %self.channel_name,
            since = %poll_start,
            "GitHub poll cycle started"
        );

        // 1. Fetch open issues/PRs updated since last poll
        let issues = client.list_issues_since(&poll_start).await?;
        tracing::debug!(
            channel = %self.channel_name,
            count = issues.len(),
            "Fetched open issues/PRs"
        );

        for issue in &issues {
            let github_type = if issue.is_pull_request() { "pull_request" } else { "issue" };
            let event_uid = format!("{}-{}-updated-{}", github_type, issue.number, issue.updated_at);

            if processed_events.contains(&event_uid) {
                continue;
            }

            let labels: Vec<String> = issue.labels.iter().map(|l| l.name.clone()).collect();

            tracing::info!(
                channel = %self.channel_name,
                event = "issue_updated",
                number = issue.number,
                title = %issue.title,
                github_type = github_type,
                user = %issue.user.login,
                labels = ?labels,
                "GitHub event detected"
            );

            processed_events.insert(event_uid);
        }

        // 2. Fetch comments since last poll
        let comments = client.list_comments_since(&poll_start).await?;
        tracing::debug!(
            channel = %self.channel_name,
            count = comments.len(),
            "Fetched comments"
        );

        for comment in &comments {
            // Skip ALL comments posted by JYC agents (identified by role prefix).
            // We skip [Planner], [Developer], [Reviewer] prefixed comments to prevent
            // infinite loops (agent posts → poll detects → agent triggered again).
            //
            // Cross-role triggering (e.g., reviewer comment triggers developer) is
            // handled via LABEL changes, not comment routing. The developer reads
            // review comments via `gh pr view --comments` when triggered by a label.
            let body_trimmed = comment.body.trim();
            if body_trimmed.starts_with("[Planner]")
                || body_trimmed.starts_with("[Developer]")
                || body_trimmed.starts_with("[Reviewer]")
            {
                tracing::debug!(
                    channel = %self.channel_name,
                    comment_id = comment.id,
                    "Skipping JYC agent comment (role prefix detected)"
                );
                continue;
            }

            let event_uid = format!("comment-{}", comment.id);

            if processed_events.contains(&event_uid) {
                continue;
            }

            let issue_number = comment.issue_number().unwrap_or(0);

            tracing::info!(
                channel = %self.channel_name,
                event = "comment",
                comment_id = comment.id,
                issue_number = issue_number,
                user = %comment.user.login,
                body_preview = %&comment.body[..comment.body.len().min(80)],
                "GitHub comment detected"
            );

            processed_events.insert(event_uid);
        }

        // 3. Fetch recently closed issues/PRs
        let closed = client.list_closed_since(&poll_start).await?;
        tracing::debug!(
            channel = %self.channel_name,
            count = closed.len(),
            "Fetched closed issues/PRs"
        );

        for item in &closed {
            let github_type = if item.is_pull_request() { "pull_request" } else { "issue" };
            let event_uid = format!("{}-{}-closed", github_type, item.number);

            if processed_events.contains(&event_uid) {
                continue;
            }

            let is_merged = item
                .pull_request
                .as_ref()
                .and_then(|pr| pr.merged_at.as_ref())
                .is_some();

            tracing::info!(
                channel = %self.channel_name,
                event = "closed",
                number = item.number,
                github_type = github_type,
                is_merged = is_merged,
                "GitHub close event detected"
            );

            processed_events.insert(event_uid);
        }

        // Update last poll timestamp (subtract 30s buffer to avoid missing
        // events that were created just before the poll started)
        *last_poll = (chrono::Utc::now() - chrono::Duration::seconds(30))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();

        // Prune old processed events to prevent unbounded growth
        // Keep at most 10000 events
        if processed_events.len() > 10000 {
            processed_events.clear();
            tracing::debug!(channel = %self.channel_name, "Pruned processed events cache");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            content: MessageContent {
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

    #[test]
    fn test_build_trigger_message() {
        let config = GithubConfig {
            owner: "kingye".to_string(),
            repo: "jyc".to_string(),
            token: "test".to_string(),
            poll_interval_secs: 60,
        };
        let adapter = GithubInboundAdapter::new(&config, "test_github".to_string());

        let msg = adapter.build_trigger_message(
            "issue_comment",
            42,
            "Add dark mode",
            "issue",
            "created",
            "user1",
            &["planning".to_string()],
            "comment-12345",
        );

        assert_eq!(msg.channel, "test_github");
        assert_eq!(msg.sender, "user1");
        assert_eq!(msg.topic, "#42 Add dark mode");
        assert_eq!(msg.channel_uid, "comment-12345");

        let text = msg.content.text.unwrap();
        assert!(text.contains("github event: issue_comment"));
        assert!(text.contains("number: 42"));
        assert!(text.contains("type: issue"));
        assert!(text.contains("labels: planning"));
        assert!(text.contains("gh issue view 42"));

        assert_eq!(
            msg.metadata.get("github_type").unwrap().as_str().unwrap(),
            "issue"
        );
        assert_eq!(
            msg.metadata.get("github_number").unwrap().as_u64().unwrap(),
            42
        );
    }

    #[test]
    fn test_build_trigger_message_pr() {
        let config = GithubConfig {
            owner: "kingye".to_string(),
            repo: "jyc".to_string(),
            token: "test".to_string(),
            poll_interval_secs: 60,
        };
        let adapter = GithubInboundAdapter::new(&config, "test_github".to_string());

        let msg = adapter.build_trigger_message(
            "pull_request",
            43,
            "Fix issue #42",
            "pull_request",
            "opened",
            "bot",
            &[],
            "pr-43-opened",
        );

        let text = msg.content.text.unwrap();
        assert!(text.contains("gh pr view 43"));
        assert!(text.contains("gh pr diff 43"));
    }
}
