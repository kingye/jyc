use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use crate::channels::types::{
    ChannelMatcher, ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage,
    MessageContent, PatternMatch,
};
use crate::utils::helpers::truncate_str;
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
        pattern_match: Option<&PatternMatch>,
    ) -> String {
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

        // Check if this is a hand-over with a specific role
        // Reviewer gets a separate thread prefix: review-pr-{N}
        if let Some(ref pm) = pattern_match {
            if pm.pattern_name == "reviewer" {
                return format!("review-pr-{}", number);
            }
        }

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
        // Routing is mention-driven: only comments containing @j:<role> trigger agents.
        // The handover_role metadata is set by poll_once() when @j:<role> is detected.
        // No mention = no routing.
        let handover_role = message.metadata.get("handover_role").and_then(|v| v.as_str())?;

        for pattern in patterns {
            if !pattern.enabled {
                continue;
            }
            if let Some(ref role) = pattern.role {
                if role.eq_ignore_ascii_case(handover_role) {
                    // Self-loop prevention: skip if comment is from this pattern's own role.
                    // A [Developer] comment with @j:developer should NOT re-trigger developer.
                    if let Some(comment_role) = message.metadata.get("comment_role").and_then(|v| v.as_str()) {
                        if role.eq_ignore_ascii_case(comment_role) {
                            continue;
                        }
                    }

                    return Some(PatternMatch {
                        pattern_name: pattern.name.clone(),
                        channel: "github".to_string(),
                        matches: HashMap::new(),
                    });
                }
            }
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
    /// Directory for persistent state: <workdir>/<channel>/.github/
    state_dir: PathBuf,
}

impl GithubInboundAdapter {
    pub fn new(config: &GithubConfig, channel_name: String, workdir: &Path) -> Self {
        let state_dir = workdir.join(&channel_name).join(".github");
        Self {
            config: config.clone(),
            channel_name,
            state_dir,
        }
    }

    /// Load processed comment IDs from persistent storage.
    /// File format: one comment ID per line in `.github/processed-comments.txt`.
    async fn load_processed_comments(&self) -> HashSet<u64> {
        let file = self.state_dir.join("processed-comments.txt");
        if !file.exists() {
            return HashSet::new();
        }
        match tokio::fs::read_to_string(&file).await {
            Ok(content) => {
                let set: HashSet<u64> = content
                    .lines()
                    .filter_map(|line| line.trim().parse::<u64>().ok())
                    .collect();
                tracing::debug!(
                    channel = %self.channel_name,
                    count = set.len(),
                    "Loaded processed comment IDs"
                );
                set
            }
            Err(e) => {
                tracing::warn!(
                    channel = %self.channel_name,
                    error = %e,
                    "Failed to load processed comments, starting fresh"
                );
                HashSet::new()
            }
        }
    }

    /// Persist a comment ID as processed (append to file).
    async fn track_comment(&self, comment_id: u64, processed: &mut HashSet<u64>) {
        processed.insert(comment_id);

        let file = self.state_dir.join("processed-comments.txt");
        use tokio::io::AsyncWriteExt;
        if let Ok(mut f) = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .await
        {
            let _ = f.write_all(format!("{comment_id}\n").as_bytes()).await;
        }

        // Compact when >5000 entries: rewrite with only what's in memory
        if processed.len() > 5000 {
            self.compact_processed_comments(processed).await;
        }
    }

    /// Compact processed comments file by keeping only the latest entries.
    async fn compact_processed_comments(&self, processed: &mut HashSet<u64>) {
        // Keep only the 2000 highest IDs (most recent)
        if processed.len() <= 2000 {
            return;
        }

        let mut ids: Vec<u64> = processed.iter().copied().collect();
        ids.sort_unstable();
        let keep_from = ids.len() - 2000;
        let keep: HashSet<u64> = ids[keep_from..].iter().copied().collect();

        let before = processed.len();
        *processed = keep;

        let file = self.state_dir.join("processed-comments.txt");
        let content: String = processed
            .iter()
            .map(|id| format!("{id}\n"))
            .collect();
        if let Err(e) = tokio::fs::write(&file, content).await {
            tracing::warn!(error = %e, "Failed to compact processed comments file");
        } else {
            tracing::info!(
                channel = %self.channel_name,
                before = before,
                after = processed.len(),
                "Compacted processed comments"
            );
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
        assignees: &[String],
        event_uid: &str,
    ) -> InboundMessage {
        let label_str = if labels.is_empty() {
            String::new()
        } else {
            format!("labels: {}\n", labels.join(", "))
        };

        let assignee_str = if assignees.is_empty() {
            String::new()
        } else {
            format!("assignees: {}\n", assignees.join(", "))
        };

        let gh_cmd = match github_type {
            "pull_request" => format!(
                "Repository: {}/{}\n\nSetup:\n  cd repo  # or: gh repo clone {}/{} repo && cd repo\n\nRead PR:\n  gh pr view {}\n  gh pr view {} --comments\n  gh pr diff {}",
                self.config.owner, self.config.repo,
                self.config.owner, self.config.repo,
                number, number, number
            ),
            _ => format!(
                "Repository: {}/{}\n\nSetup:\n  cd repo  # or: gh repo clone {}/{} repo && cd repo\n\nRead issue:\n  gh issue view {}\n  gh issue view {} --comments",
                self.config.owner, self.config.repo,
                self.config.owner, self.config.repo,
                number, number
            ),
        };

        let body = format!(
            "github event: {}\nrepository: {}/{}\nnumber: {}\ntype: {}\naction: {}\nactor: {}\n{}{}{}",
            event_type, self.config.owner, self.config.repo, number, github_type, action, actor, label_str, assignee_str, gh_cmd
        );

        let mut metadata = HashMap::new();
        metadata.insert("github_event".to_string(), serde_json::json!(event_type));
        metadata.insert("github_number".to_string(), serde_json::json!(number));
        metadata.insert("github_type".to_string(), serde_json::json!(github_type));
        metadata.insert("github_action".to_string(), serde_json::json!(action));
        metadata.insert("github_labels".to_string(), serde_json::json!(labels));
        metadata.insert("github_assignees".to_string(), serde_json::json!(assignees));

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

        // Create state directory and load persistent processed comments
        tokio::fs::create_dir_all(&self.state_dir).await
            .with_context(|| format!("failed to create state directory: {}", self.state_dir.display()))?;
        let mut processed_comments: HashSet<u64> = self.load_processed_comments().await;

        // Track processed event IDs for non-comment deduplication (close events)
        let mut processed_events: HashSet<String> = HashSet::new();

        // Cache issue info for comment routing (number → title, type, labels, assignees)
        let mut issue_cache: HashMap<u64, (String, String, Vec<String>, Vec<String>)> = HashMap::new();

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
                        &mut processed_comments,
                        &mut processed_events,
                        &mut issue_cache,
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
    /// Execute one poll cycle: fetch comments with @j:<role> mentions.
    /// Routes events to threads via on_message callback.
    async fn poll_once(
        &self,
        client: &GithubClient,
        options: &InboundAdapterOptions,
        processed_comments: &mut HashSet<u64>,
        processed_events: &mut HashSet<String>,
        issue_cache: &mut HashMap<u64, (String, String, Vec<String>, Vec<String>)>, // number → (title, type, labels, assignees)
        last_poll: &mut String,
    ) -> Result<()> {
        let poll_start = last_poll.clone();

        tracing::trace!(
            channel = %self.channel_name,
            since = %poll_start,
            "GitHub poll cycle started"
        );

        // 1. Fetch ALL open issues/PRs to populate the cache and detect closures.
        // We fetch the complete set (not just recently-updated) so cache comparison
        // for close detection is reliable.
        let issues = client.list_all_open_issues().await?;
        tracing::trace!(
            channel = %self.channel_name,
            count = issues.len(),
            "Fetched all open issues/PRs"
        );

        for issue in &issues {
            let github_type = if issue.is_pull_request() { "pull_request" } else { "issue" };
            let labels: Vec<String> = issue.labels.iter().map(|l| l.name.clone()).collect();
            let assignees: Vec<String> = issue.assignees.iter().map(|a| a.login.clone()).collect();

            issue_cache.insert(
                issue.number,
                (issue.title.clone(), github_type.to_string(), labels, assignees),
            );
        }

        // 2. Fetch and process comments — only route those with @j:<role> mentions.
        // The issue cache is now populated, so lookups work correctly.
        let comments = client.list_comments_since(&poll_start).await?;
        tracing::trace!(
            channel = %self.channel_name,
            count = comments.len(),
            "Fetched comments"
        );

        let mention_re = Regex::new(r"(?i)@j:(\w+)").unwrap();

        for comment in &comments {
            // Skip already-processed comments (persistent dedup)
            if processed_comments.contains(&comment.id) {
                continue;
            }

            let body_trimmed = comment.body.trim();

            // Extract @j:<role> mention — only route if present
            let handover_role = mention_re
                .captures(body_trimmed)
                .and_then(|caps| caps.get(1))
                .map(|m| m.as_str().to_lowercase());

            let handover_role = match handover_role {
                Some(role) => role,
                None => {
                    // No @j:<role> mention — skip, but mark as processed
                    self.track_comment(comment.id, processed_comments).await;
                    continue;
                }
            };

            // Extract [Role] prefix for self-loop prevention
            let comment_role = extract_comment_role(body_trimmed);

            let issue_number = comment.issue_number().unwrap_or(0);

            // Look up issue info from cache
            let (title, github_type, labels, assignees) = issue_cache
                .get(&issue_number)
                .cloned()
                .unwrap_or_else(|| (format!("#{}", issue_number), "issue".to_string(), vec![], vec![]));

            let event_uid = format!("comment-{}", comment.id);

            tracing::info!(
                channel = %self.channel_name,
                event = "mention",
                comment_id = comment.id,
                issue_number = issue_number,
                target_role = %handover_role,
                user = %comment.user.login,
                body_preview = %truncate_str(&comment.body, 80),
                "Comment with @j:{} detected → routing", handover_role,
            );

            // Build trigger message with handover_role metadata
            let mut message = self.build_trigger_message(
                "issue_comment",
                issue_number,
                &title,
                &github_type,
                "mentioned",
                &comment.user.login,
                &labels,
                &assignees,
                &event_uid,
            );

            message.metadata.insert(
                "handover_role".to_string(),
                serde_json::Value::String(handover_role),
            );

            if let Some(ref role) = comment_role {
                message.metadata.insert(
                    "comment_role".to_string(),
                    serde_json::Value::String(role.clone()),
                );
            }

            if let Err(e) = (options.on_message)(message) {
                tracing::error!(error = %e, number = issue_number, "Failed to route comment event");
            }

            self.track_comment(comment.id, processed_comments).await;
        }

        // 3. Detect closed issues/PRs by comparing cache with full open set.
        // Since we fetched ALL open issues (not just recently-updated ones),
        // the comparison is reliable: if an issue was in the cache but is not
        // in the current open set, it was genuinely closed.
        //
        // Build set of current open issue numbers for comparison
        let current_open_numbers: HashSet<u64> = issues.iter().map(|i| i.number).collect();

        // Find issues that were in cache but not in current open list
        let cached_numbers: Vec<u64> = issue_cache.keys().cloned().collect();
        for cached_number in cached_numbers {
            if !current_open_numbers.contains(&cached_number) {
                // Get cached info before removing
                if let Some((_title, github_type, _labels, _assignees)) = issue_cache.get(&cached_number) {
                    let event_uid = format!("{}-{}-closed", github_type, cached_number);

                    if !processed_events.contains(&event_uid) {
                        tracing::info!(
                            channel = %self.channel_name,
                            event = "closed",
                            number = cached_number,
                            github_type = github_type,
                            "GitHub close event detected (via cache comparison) → closing threads"
                        );

                        if let Some(ref on_close) = options.on_thread_close {
                            match github_type.as_str() {
                                "pull_request" => {
                                    let _ = (on_close)(format!("pr-{}", cached_number));
                                    let _ = (on_close)(format!("review-pr-{}", cached_number));
                                }
                                _ => {
                                    let _ = (on_close)(format!("issue-{}", cached_number));
                                }
                            }
                        }

                        processed_events.insert(event_uid);
                    }
                }

                issue_cache.remove(&cached_number);
            }
        }

        // 4. Fetch recently closed issues/PRs as backup (for edge cases).
        // This catches issues that were closed but never cached (e.g., closed before first poll).
        let closed = client.list_closed_since(&poll_start).await?;
        tracing::trace!(
            channel = %self.channel_name,
            count = closed.len(),
            "Fetched closed issues/PRs (backup)"
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
                "GitHub close event detected → closing threads"
            );

            // Close threads (Phase 6 will delete directories)
            if let Some(ref on_close) = options.on_thread_close {
                match github_type {
                    "pull_request" => {
                        // Close PR thread and review thread
                        let _ = (on_close)(format!("pr-{}", item.number));
                        let _ = (on_close)(format!("review-pr-{}", item.number));
                        // If merged, also close linked issue thread
                        // (GitHub auto-closes the linked issue)
                        // TODO: detect linked issue number from PR body "Fixes #N"
                    }
                    _ => {
                        // Close issue thread
                        let _ = (on_close)(format!("issue-{}", item.number));
                    }
                }
            }

            // Remove from issue cache
            issue_cache.remove(&item.number);
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

/// Extract agent role from `[Role]` prefix in comment body.
///
/// Examples:
///   "[Developer] some text" → Some("Developer")
///   "[Reviewer] code looks good" → Some("Reviewer")
///   "[Planner] questions about requirements" → Some("Planner")
///   "normal comment" → None
///   "[Unknown] something" → None
///
/// Only recognizes known agent roles to avoid false positives.
fn extract_comment_role(text: &str) -> Option<String> {
    if text.starts_with('[') {
        if let Some(end) = text.find(']') {
            let role = &text[1..end];
            match role {
                "Planner" | "Developer" | "Reviewer" => return Some(role.to_string()),
                _ => {}
            }
        }
    }
    None
}

/// Extract @j:<role> mention from comment text.
///
/// Examples:
///   "@j:developer Please implement this" → Some("developer")
///   "@j:Reviewer Ready for review" → Some("reviewer")
///   "Normal comment without mention" → None
fn extract_mention_role(text: &str) -> Option<String> {
    let re = Regex::new(r"(?i)@j:(\w+)").ok()?;
    re.captures(text)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(github_type: &str, number: u64) -> InboundMessage {
        let mut metadata = HashMap::new();
        metadata.insert(
            "github_type".to_string(),
            serde_json::Value::String(github_type.to_string()),
        );
        metadata.insert(
            "github_number".to_string(),
            serde_json::json!(number),
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
                role: Some("Planner".to_string()),
                rules: crate::channels::types::PatternRules {
                    github_type: Some(vec!["issue".to_string()]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ChannelPattern {
                name: "developer".to_string(),
                enabled: true,
                role: Some("Developer".to_string()),
                rules: crate::channels::types::PatternRules {
                    github_type: Some(vec!["pull_request".to_string()]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ChannelPattern {
                name: "reviewer".to_string(),
                enabled: true,
                role: Some("Reviewer".to_string()),
                rules: crate::channels::types::PatternRules {
                    github_type: Some(vec!["pull_request".to_string()]),
                    ..Default::default()
                },
                ..Default::default()
            },
        ]
    }

    // --- Thread name derivation ---

    #[test]
    fn test_derive_thread_name_issue() {
        let msg = make_message("issue", 42);
        let name = GithubMatcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "issue-42");
    }

    #[test]
    fn test_derive_thread_name_pr() {
        let msg = make_message("pull_request", 43);
        let name = GithubMatcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "pr-43");
    }

    #[test]
    fn test_derive_thread_name_reviewer() {
        let msg = make_message("pull_request", 43);
        let pm = PatternMatch {
            pattern_name: "reviewer".to_string(),
            channel: "github".to_string(),
            matches: HashMap::new(),
        };
        let name = GithubMatcher.derive_thread_name(&msg, &[], Some(&pm));
        assert_eq!(name, "review-pr-43");
    }

    #[test]
    fn test_derive_thread_name_developer() {
        let msg = make_message("pull_request", 43);
        let pm = PatternMatch {
            pattern_name: "developer".to_string(),
            channel: "github".to_string(),
            matches: HashMap::new(),
        };
        let name = GithubMatcher.derive_thread_name(&msg, &[], Some(&pm));
        assert_eq!(name, "pr-43");
    }

    // --- Mention-based routing ---

    #[test]
    fn test_no_mention_no_match() {
        // Comment without @j:<role> → no routing
        let msg = make_message("issue", 42);
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    #[test]
    fn test_mention_planner_matches() {
        let mut msg = make_message("issue", 42);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "planner");
    }

    #[test]
    fn test_mention_developer_matches() {
        let mut msg = make_message("pull_request", 43);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "developer");
    }

    #[test]
    fn test_mention_reviewer_matches() {
        let mut msg = make_message("pull_request", 43);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("reviewer"));
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "reviewer");
    }

    #[test]
    fn test_mention_case_insensitive() {
        let mut msg = make_message("pull_request", 43);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("Reviewer"));
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "reviewer");
    }

    #[test]
    fn test_mention_unknown_role_no_match() {
        let mut msg = make_message("issue", 42);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("unknown"));
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    #[test]
    fn test_disabled_pattern_skipped() {
        let mut msg = make_message("issue", 42);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![ChannelPattern {
            name: "planner".to_string(),
            enabled: false,
            role: Some("Planner".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    // --- Self-loop prevention ---

    #[test]
    fn test_self_loop_developer_mention_own_role() {
        // [Developer] posts "@j:developer" — should NOT re-trigger developer
        let mut msg = make_message("pull_request", 43);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        msg.metadata.insert("comment_role".to_string(), serde_json::json!("Developer"));
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    #[test]
    fn test_self_loop_reviewer_mention_own_role() {
        // [Reviewer] posts "@j:reviewer" — should NOT re-trigger reviewer
        let mut msg = make_message("pull_request", 43);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("reviewer"));
        msg.metadata.insert("comment_role".to_string(), serde_json::json!("Reviewer"));
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    #[test]
    fn test_cross_role_reviewer_to_developer() {
        // [Reviewer] posts "@j:developer" — should trigger developer
        let mut msg = make_message("pull_request", 43);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        msg.metadata.insert("comment_role".to_string(), serde_json::json!("Reviewer"));
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "developer");
    }

    #[test]
    fn test_cross_role_developer_to_reviewer() {
        // [Developer] posts "@j:reviewer" — should trigger reviewer
        let mut msg = make_message("pull_request", 43);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("reviewer"));
        msg.metadata.insert("comment_role".to_string(), serde_json::json!("Developer"));
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "reviewer");
    }

    // --- Helper function tests ---

    #[test]
    fn test_extract_comment_role() {
        assert_eq!(extract_comment_role("[Developer] some text"), Some("Developer".to_string()));
        assert_eq!(extract_comment_role("[Reviewer] code looks good"), Some("Reviewer".to_string()));
        assert_eq!(extract_comment_role("[Planner] questions"), Some("Planner".to_string()));
        assert_eq!(extract_comment_role("normal comment"), None);
        assert_eq!(extract_comment_role("[Unknown] something"), None);
        assert_eq!(extract_comment_role(""), None);
    }

    #[test]
    fn test_extract_mention_role() {
        assert_eq!(extract_mention_role("@j:developer Please implement"), Some("developer".to_string()));
        assert_eq!(extract_mention_role("@j:Reviewer Ready for review"), Some("reviewer".to_string()));
        assert_eq!(extract_mention_role("Normal comment"), None);
        assert_eq!(extract_mention_role("Some text @j:planner more text"), Some("planner".to_string()));
        assert_eq!(extract_mention_role("[Planner] This is a reply"), None);
        // Case insensitive — always returns lowercase
        assert_eq!(extract_mention_role("@j:DEVELOPER"), Some("developer".to_string()));
        // Old @jyc: format should NOT match
        assert_eq!(extract_mention_role("@jyc:developer"), None);
        // Empty and edge cases
        assert_eq!(extract_mention_role(""), None);
        assert_eq!(extract_mention_role("@j:"), None);
        // First match wins
        assert_eq!(extract_mention_role("@j:developer @j:reviewer"), Some("developer".to_string()));
    }

    // --- Persistent comment tracking ---

    #[tokio::test]
    async fn test_load_processed_comments_empty() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = GithubConfig {
            owner: "test".to_string(),
            repo: "test".to_string(),
            token: "test".to_string(),
            api_url: "https://api.github.com".to_string(),
            poll_interval_secs: 60,
        };
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let comments = adapter.load_processed_comments().await;
        assert!(comments.is_empty());
    }

    #[tokio::test]
    async fn test_track_and_load_comments() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = GithubConfig {
            owner: "test".to_string(),
            repo: "test".to_string(),
            token: "test".to_string(),
            api_url: "https://api.github.com".to_string(),
            poll_interval_secs: 60,
        };
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut processed = HashSet::new();

        // Track some comments
        adapter.track_comment(100, &mut processed).await;
        adapter.track_comment(200, &mut processed).await;
        adapter.track_comment(300, &mut processed).await;

        assert_eq!(processed.len(), 3);
        assert!(processed.contains(&100));
        assert!(processed.contains(&200));
        assert!(processed.contains(&300));

        // Reload from disk — should get same set
        let reloaded = adapter.load_processed_comments().await;
        assert_eq!(reloaded.len(), 3);
        assert!(reloaded.contains(&100));
        assert!(reloaded.contains(&200));
        assert!(reloaded.contains(&300));
    }

    #[tokio::test]
    async fn test_track_comment_dedup() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = GithubConfig {
            owner: "test".to_string(),
            repo: "test".to_string(),
            token: "test".to_string(),
            api_url: "https://api.github.com".to_string(),
            poll_interval_secs: 60,
        };
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut processed = HashSet::new();

        // Track same comment twice
        adapter.track_comment(100, &mut processed).await;
        adapter.track_comment(100, &mut processed).await;

        // In-memory set should have exactly 1
        assert_eq!(processed.len(), 1);
    }

    #[tokio::test]
    async fn test_compact_processed_comments() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = GithubConfig {
            owner: "test".to_string(),
            repo: "test".to_string(),
            token: "test".to_string(),
            api_url: "https://api.github.com".to_string(),
            poll_interval_secs: 60,
        };
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        // Create a set with 3000 entries
        let mut processed: HashSet<u64> = (1..=3000).collect();

        // Compact should keep only the 2000 highest IDs (1001..=3000)
        adapter.compact_processed_comments(&mut processed).await;

        assert_eq!(processed.len(), 2000);
        // Lowest kept should be 1001
        assert!(!processed.contains(&1));
        assert!(!processed.contains(&1000));
        assert!(processed.contains(&1001));
        assert!(processed.contains(&3000));

        // Verify file was rewritten correctly
        let reloaded = adapter.load_processed_comments().await;
        assert_eq!(reloaded.len(), 2000);
        assert!(reloaded.contains(&1001));
        assert!(reloaded.contains(&3000));
    }

    #[tokio::test]
    async fn test_compact_no_op_under_threshold() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = GithubConfig {
            owner: "test".to_string(),
            repo: "test".to_string(),
            token: "test".to_string(),
            api_url: "https://api.github.com".to_string(),
            poll_interval_secs: 60,
        };
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        // Set with fewer than 2000 entries — compact should be a no-op
        let mut processed: HashSet<u64> = (1..=100).collect();
        adapter.compact_processed_comments(&mut processed).await;
        assert_eq!(processed.len(), 100);
    }

    // --- Build trigger message ---

    #[test]
    fn test_build_trigger_message() {
        let config = GithubConfig {
            owner: "kingye".to_string(),
            repo: "jyc".to_string(),
            token: "test".to_string(),
            api_url: "https://api.github.com".to_string(),
            poll_interval_secs: 60,
        };
        let tmpdir = tempfile::tempdir().unwrap();
        let adapter = GithubInboundAdapter::new(&config, "test_github".to_string(), tmpdir.path());

        let msg = adapter.build_trigger_message(
            "issue_comment",
            42,
            "Add dark mode",
            "issue",
            "mentioned",
            "user1",
            &["planning".to_string()],
            &["alice".to_string()],
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
        assert!(text.contains("assignees: alice"));
        assert!(text.contains("gh issue view 42"));
    }

    #[test]
    fn test_build_trigger_message_pr() {
        let config = GithubConfig {
            owner: "kingye".to_string(),
            repo: "jyc".to_string(),
            token: "test".to_string(),
            api_url: "https://api.github.com".to_string(),
            poll_interval_secs: 60,
        };
        let tmpdir = tempfile::tempdir().unwrap();
        let adapter = GithubInboundAdapter::new(&config, "test_github".to_string(), tmpdir.path());

        let msg = adapter.build_trigger_message(
            "pull_request",
            43,
            "Fix issue #42",
            "pull_request",
            "mentioned",
            "bot",
            &[],
            &["alice".to_string(), "bob".to_string()],
            "pr-43-opened",
        );

        let text = msg.content.text.unwrap();
        assert!(text.contains("gh pr view 43"));
        assert!(text.contains("gh pr diff 43"));
        assert!(text.contains("assignees: alice, bob"));
    }
}
