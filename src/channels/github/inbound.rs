use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use crate::channels::types::{
    ChannelMatcher, ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage,
    LabelRule, MessageContent, PatternMatch, PatternRules,
};
use crate::utils::helpers::truncate_str;
use super::client::{GithubClient, GithubComment};
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
        for pattern in patterns {
            if !pattern.enabled {
                continue;
            }

            let Some(ref pattern_role) = pattern.role else {
                continue;
            };

            if !self.rules_match(&pattern.rules, message) {
                tracing::debug!(
                    pattern = %pattern.name,
                    "Rules did not match, skipping"
                );
                continue;
            }

            if let Some(comment_role) = message.metadata.get("comment_role").and_then(|v| v.as_str()) {
                if pattern_role.eq_ignore_ascii_case(comment_role) {
                    continue;
                }
            }

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

impl GithubMatcher {
    /// Check whether the GitHub-specific rules (github_type, labels, assignees) all match.
    ///
    /// All present rules use AND logic (all must pass).
    /// Within each rule, OR logic applies (any value in the list suffices).
    /// Rules that are `None` are considered matched (no constraint).
    fn rules_match(&self, rules: &PatternRules, message: &InboundMessage) -> bool {
        // Check github_type rule
        if let Some(ref allowed_types) = rules.github_type {
            let msg_type = message
                .metadata
                .get("github_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !allowed_types.iter().any(|t| t.eq_ignore_ascii_case(msg_type)) {
                return false;
            }
        }

        // Extract github_labels once for labels and exclude_labels checks
        let msg_labels: Vec<String> = message
            .metadata
            .get("github_labels")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        // Check labels rule (delegates to LabelRule::matches for flat OR / nested AND-OR logic)
        if let Some(ref label_rule) = rules.labels {
            if !label_rule.matches(&msg_labels) {
                return false;
            }
        }

        // Check exclude_labels rule (OR logic: if ANY exclude label is present, pattern does not match)
        if let Some(ref exclude_labels) = rules.exclude_labels {
            let has_excluded = exclude_labels
                .iter()
                .any(|l| msg_labels.contains(&l.to_lowercase()));
            if has_excluded {
                return false;
            }
        }

        // Check assignees rule (OR logic: match if ANY assignee on the issue/PR is in the rule list)
        if let Some(ref allowed_assignees) = rules.assignees {
            let msg_assignees: Vec<String> = message
                .metadata
                .get("github_assignees")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                        .collect()
                })
                .unwrap_or_default();
            let has_match = allowed_assignees
                .iter()
                .any(|a| msg_assignees.contains(&a.to_lowercase()));
            if !has_match {
                return false;
            }
        }

        true
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

    /// Load processed comment keys from persistent storage.
    /// File format: one key per line (`{comment_id}:{updated_at}`).
    /// Using `id:updated_at` ensures edited comments are re-processed.
    async fn load_processed_comments(&self) -> HashSet<String> {
        let file = self.state_dir.join("processed-comments.txt");
        if !file.exists() {
            return HashSet::new();
        }
        match tokio::fs::read_to_string(&file).await {
            Ok(content) => {
                let set: HashSet<String> = content
                    .lines()
                    .map(|line| line.trim().to_string())
                    .filter(|line| !line.is_empty())
                    .collect();
                tracing::debug!(
                    channel = %self.channel_name,
                    count = set.len(),
                    "Loaded processed comment keys"
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

    /// Persist a comment key as processed (append to file).
    /// Key format: `{comment_id}:{updated_at}`
    async fn track_comment(&self, key: &str, processed: &mut HashSet<String>) {
        processed.insert(key.to_string());

        let file = self.state_dir.join("processed-comments.txt");
        use tokio::io::AsyncWriteExt;
        if let Ok(mut f) = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .await
        {
            let _ = f.write_all(format!("{key}\n").as_bytes()).await;
        }

        // Compact when >5000 entries: rewrite with only what's in memory
        if processed.len() > 5000 {
            self.compact_processed_comments(processed).await;
        }
    }

    /// Compact processed comments file by keeping only the latest entries.
    async fn compact_processed_comments(&self, processed: &mut HashSet<String>) {
        if processed.len() <= 2000 {
            return;
        }

        // Keep only the 2000 most recent entries.
        // Sort by the comment ID prefix (numeric) to determine recency.
        let mut entries: Vec<(u64, String)> = processed
            .iter()
            .map(|key| {
                let id = key.split(':').next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                (id, key.clone())
            })
            .collect();
        entries.sort_unstable_by_key(|(id, _)| *id);
        let keep_from = entries.len() - 2000;
        let keep: HashSet<String> = entries[keep_from..]
            .iter()
            .map(|(_, key)| key.clone())
            .collect();

        let before = processed.len();
        *processed = keep;

        let file = self.state_dir.join("processed-comments.txt");
        let content: String = processed
            .iter()
            .map(|key| format!("{key}\n"))
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

    /// Load seen issues from persistent storage.
    /// File format: one line per issue (`{number}:{labels}:{updated_at}`).
    async fn load_seen_issues(&self) -> HashSet<String> {
        let file = self.state_dir.join("seen-issues.txt");
        if !file.exists() {
            return HashSet::new();
        }
        match tokio::fs::read_to_string(&file).await {
            Ok(content) => {
                let set: HashSet<String> = content
                    .lines()
                    .map(|line| line.trim().to_string())
                    .filter(|line| !line.is_empty())
                    .collect();
                tracing::debug!(
                    channel = %self.channel_name,
                    count = set.len(),
                    "Loaded seen issues"
                );
                set
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to load seen issues, starting fresh"
                );
                HashSet::new()
            }
        }
    }

    /// Track a seen issue (append to file).
    /// Key format: `{number}:{labels}:{updated_at}`
    async fn track_seen_issue(&self, key: &str, seen: &mut HashSet<String>) {
        if seen.insert(key.to_string()) {
            let file = self.state_dir.join("seen-issues.txt");
            use tokio::io::AsyncWriteExt;
            if let Ok(mut f) = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&file)
                .await
            {
                let _ = f.write_all(format!("{key}\n").as_bytes()).await;
            }

            if seen.len() > 5000 {
                self.compact_seen_issues(seen).await;
            }
        }
    }

    /// Compact seen issues file by keeping only the latest entries.
    async fn compact_seen_issues(&self, seen: &mut HashSet<String>) {
        if seen.len() <= 2000 {
            return;
        }

        let mut entries: Vec<(u64, String)> = seen
            .iter()
            .map(|key| {
                let number = key.split(':').next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                (number, key.clone())
            })
            .collect();
        entries.sort_unstable_by_key(|(number, _)| *number);
        let keep_from = entries.len() - 2000;
        let keep: HashSet<String> = entries[keep_from..]
            .iter()
            .map(|(_, key)| key.clone())
            .collect();

        let before = seen.len();
        *seen = keep;

        let file = self.state_dir.join("seen-issues.txt");
        let content: String = seen
            .iter()
            .map(|key| format!("{key}\n"))
            .collect();
        if let Err(e) = tokio::fs::write(&file, content).await {
            tracing::warn!(error = %e, "Failed to compact seen issues file");
        } else {
            tracing::info!(
                channel = %self.channel_name,
                before = before,
                after = seen.len(),
                "Compacted seen issues"
            );
        }
    }

    /// Load CI status tracking from persistent storage.
    /// File format: `{pr_number}:{head_sha}:{overall_status}` per line.
    /// Returns map of pr_number → (head_sha, overall_status).
    async fn load_ci_status(&self) -> HashMap<u64, (String, String)> {
        let file = self.state_dir.join("ci-status.txt");
        if !file.exists() {
            return HashMap::new();
        }
        match tokio::fs::read_to_string(&file).await {
            Ok(content) => {
                let map: HashMap<u64, (String, String)> = content
                    .lines()
                    .filter_map(|line| {
                        let line = line.trim();
                        if line.is_empty() {
                            return None;
                        }
                        let mut parts = line.splitn(3, ':');
                        let number: u64 = parts.next()?.parse().ok()?;
                        let head_sha = parts.next()?.to_string();
                        let status = parts.next()?.to_string();
                        Some((number, (head_sha, status)))
                    })
                    .collect();
                tracing::debug!(
                    channel = %self.channel_name,
                    count = map.len(),
                    "Loaded CI status tracking"
                );
                map
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to load CI status, starting fresh"
                );
                HashMap::new()
            }
        }
    }

    /// Track CI status for a PR (append to file).
    /// Key format: `{pr_number}:{head_sha}:{overall_status}`
    async fn track_ci_status(
        &self,
        pr_number: u64,
        head_sha: &str,
        status: &str,
        tracked: &mut HashMap<u64, (String, String)>,
    ) {
        let changed = tracked
            .get(&pr_number)
            .map(|(sha, s)| sha != head_sha || s != status)
            .unwrap_or(true);

        tracked.insert(pr_number, (head_sha.to_string(), status.to_string()));

        if changed {
            self.compact_ci_status(tracked).await;
        }
    }

    /// Rewrite CI status file from in-memory map.
    async fn compact_ci_status(&self, tracked: &mut HashMap<u64, (String, String)>) {
        let file = self.state_dir.join("ci-status.txt");
        let content: String = tracked
            .iter()
            .map(|(number, (sha, status))| format!("{}:{}:{}\n", number, sha, status))
            .collect();
        if let Err(e) = tokio::fs::write(&file, content).await {
            tracing::warn!(error = %e, "Failed to compact CI status file");
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
        let state_file = self.state_dir.join("processed-comments.txt");
        let is_fresh_start = !state_file.exists();
        tokio::fs::create_dir_all(&self.state_dir).await
            .with_context(|| format!("failed to create state directory: {}", self.state_dir.display()))?;
        let mut processed_comments: HashSet<String> = self.load_processed_comments().await;

        // Track processed event IDs for non-comment deduplication (close events)
        let mut processed_events: HashSet<String> = HashSet::new();

        // Load seen issues for deduplication (prevent re-triggering after restart)
        let mut seen_issues: HashSet<String> = self.load_seen_issues().await;

        // Cache issue info for comment routing (number → title, type, labels, assignees)
        let mut issue_cache: HashMap<u64, (String, String, Vec<String>, Vec<String>)> = HashMap::new();

        // Load CI status tracking for check-run polling
        let mut ci_status: HashMap<u64, (String, String)> = self.load_ci_status().await;

        // Determine poll start time:
        // - Fresh start (no processed-comments.txt): start from "now" to avoid
        //   replaying old comments that already have @j:<role> mentions.
        // - Restart (file exists): go back 5 minutes to catch events missed
        //   during downtime. Deduplication via processed-comments.txt prevents
        //   re-processing.
        let mut last_poll = if is_fresh_start {
            tracing::info!(
                channel = %self.channel_name,
                "Fresh start detected — polling from now (no backfill)"
            );
            chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        } else {
            tracing::info!(
                channel = %self.channel_name,
                processed_count = processed_comments.len(),
                "Restart detected — polling from 5 minutes ago"
            );
            (chrono::Utc::now() - chrono::Duration::minutes(5))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        };

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
                        &mut seen_issues,
                        &mut issue_cache,
                        &mut last_poll,
                        &mut ci_status,
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
        processed_comments: &mut HashSet<String>,
        processed_events: &mut HashSet<String>,
        seen_issues: &mut HashSet<String>,
        issue_cache: &mut HashMap<u64, (String, String, Vec<String>, Vec<String>)>, // number → (title, type, labels, assignees)
        last_poll: &mut String,
        ci_status: &mut HashMap<u64, (String, String)>, // pr_number → (head_sha, overall_status)
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
                (issue.title.clone(), github_type.to_string(), labels.clone(), assignees.clone()),
            );

            // Track seen issues for dedup (prevent re-triggering after restart).
            // Key = number:labels — triggers on first sight and label changes.
            // Does NOT include updated_at: comments (including agent's own replies)
            // update that timestamp, which would cause infinite re-triggering.
            let mut labels_sorted: Vec<String> = issue.labels.iter()
                .map(|l| l.name.clone())
                .collect();
            labels_sorted.sort();
            let seen_key = format!("{}:{}", issue.number, labels_sorted.join(","));
            let is_new = !seen_issues.contains(&seen_key);
            self.track_seen_issue(&seen_key, seen_issues).await;

            // For new/changed issues, create a trigger message so Pattern-mode
            // patterns can match on issue metadata (type, labels, assignees)
            // without requiring a comment.
            if is_new {
                let event_uid = format!("{}-{}-opened", github_type, issue.number);

                let message = self.build_trigger_message(
                    "issues",
                    issue.number,
                    &issue.title,
                    github_type,
                    "opened",
                    &issue.user.login,
                    &labels,
                    &assignees,
                    &event_uid,
                );

                tracing::info!(
                    channel = %self.channel_name,
                    event = "issue_trigger",
                    number = issue.number,
                    github_type = github_type,
                    labels = ?labels,
                    "New/changed issue detected → routing for Pattern mode"
                );

                if let Err(e) = (options.on_message)(message) {
                    tracing::error!(error = %e, number = issue.number, "Failed to route issue event");
                }
            }
        }

        // Build set of current open issue numbers — needed in step 2 (comment
        // filtering) and step 3 (close detection).
        let current_open_numbers: HashSet<u64> = issues.iter().map(|i| i.number).collect();

        // 2. Fetch and process comments.
        // The issue cache is now populated, so lookups work correctly.
        let comments = client.list_comments_since(&poll_start).await?;
        tracing::trace!(
            channel = %self.channel_name,
            count = comments.len(),
            "Fetched comments"
        );

        let mention_re = Regex::new(r"(?i)@j:(\w+)").unwrap();

        for comment in &comments {
            // Build dedup key: id:updated_at — re-processes edited comments
            let comment_key = format!("{}:{}", comment.id, comment.updated_at);

            // Skip already-processed comments (persistent dedup).
            // Also check for old format (plain ID) for backward compatibility
            // with processed-comments.txt files created before the id:updated_at change.
            let id_only = comment.id.to_string();
            if processed_comments.contains(&comment_key) || processed_comments.contains(&id_only) {
                continue;
            }

            let body_trimmed = comment.body.trim();

            // Extract @j:<role> mention
            let handover_role = mention_re
                .captures(body_trimmed)
                .and_then(|caps| caps.get(1))
                .map(|m| m.as_str().to_lowercase());

            // Extract [Role] prefix for self-loop prevention
            let comment_role = extract_comment_role(body_trimmed);

            let issue_number = comment.issue_number().unwrap_or(0);

            // Skip comments on closed issues/PRs — prevents triggering agents
            // for PRs/issues that were closed between poll cycles.
            if !should_process_comment(comment, &current_open_numbers) {
                tracing::debug!(
                    channel = %self.channel_name,
                    comment_id = comment.id,
                    issue_number = issue_number,
                    "Skipping comment on closed issue/PR"
                );
                // Still track as processed to avoid re-processing on next cycle
                self.track_comment(&comment_key, processed_comments).await;
                continue;
            }

            // Look up issue info from cache
            let (title, github_type, labels, assignees) = issue_cache
                .get(&issue_number)
                .cloned()
                .unwrap_or_else(|| (format!("#{}", issue_number), "issue".to_string(), vec![], vec![]));

            let event_uid = format!("comment-{}", comment.id);

            // Build trigger message
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

            // Include the triggering comment body so the agent knows what was asked
            message.metadata.insert(
                "comment_body".to_string(),
                serde_json::Value::String(comment.body.clone()),
            );

            // Append the comment body to the message content
            let comment_section = format!(
                "\n\n---\nTriggering comment by {}:\n\n{}",
                comment.user.login, comment.body
            );
            match &mut message.content.text {
                Some(text) => text.push_str(&comment_section),
                None => message.content.text = Some(comment_section),
            }

            // Add handover_role only if @j:<role> mention exists
            // Pattern mode patterns can match without handover_role
            if let Some(ref role) = handover_role {
                message.metadata.insert(
                    "handover_role".to_string(),
                    serde_json::Value::String(role.clone()),
                );

                tracing::info!(
                    channel = %self.channel_name,
                    event = "mention",
                    comment_id = comment.id,
                    issue_number = issue_number,
                    target_role = %role,
                    user = %comment.user.login,
                    body_preview = %truncate_str(&comment.body, 80),
                    "Comment with @j:{} detected → routing", role,
                );
            } else {
                tracing::debug!(
                    channel = %self.channel_name,
                    comment_id = comment.id,
                    issue_number = issue_number,
                    user = %comment.user.login,
                    "Comment without @j:<role> mention → routing for Pattern mode"
                );
            }

            if let Some(ref role) = comment_role {
                message.metadata.insert(
                    "comment_role".to_string(),
                    serde_json::Value::String(role.clone()),
                );
            }

            if let Err(e) = (options.on_message)(message) {
                tracing::error!(error = %e, number = issue_number, "Failed to route comment event");
            }

            self.track_comment(&comment_key, processed_comments).await;
        }

        // 2b. Fetch and process PR reviews and review comments.
        // These are per-PR endpoints, so we iterate over open PRs from issue_cache.
        let open_pr_numbers: Vec<u64> = issue_cache
            .iter()
            .filter(|(_, (_, github_type, _, _))| github_type == "pull_request")
            .map(|(number, _)| *number)
            .collect();

        for pr_number in &open_pr_numbers {
            // Process reviews
            match client.list_reviews(*pr_number).await {
                Ok(reviews) => {
                    tracing::trace!(
                        channel = %self.channel_name,
                        pr_number = pr_number,
                        count = reviews.len(),
                        "Fetched reviews for PR"
                    );

                    for review in &reviews {
                        if review.state == "PENDING" {
                            continue;
                        }

                        let submitted_at = review.submitted_at.as_deref().unwrap_or("");
                        let review_key = format!("review-{}:{}", review.id, submitted_at);

                        if processed_comments.contains(&review_key) {
                            continue;
                        }

                        let body_trimmed = review.body.trim();

                        let comment_role = extract_comment_role(body_trimmed);

                        let (title, github_type, labels, assignees) = issue_cache
                            .get(pr_number)
                            .cloned()
                            .unwrap_or_else(|| (format!("#{}", pr_number), "pull_request".to_string(), vec![], vec![]));

                        let event_uid = format!("review-{}", review.id);

                        let mut message = self.build_trigger_message(
                            "pull_request_review",
                            *pr_number,
                            &title,
                            &github_type,
                            "review_submitted",
                            &review.user.login,
                            &labels,
                            &assignees,
                            &event_uid,
                        );

                        message.metadata.insert(
                            "review_state".to_string(),
                            serde_json::Value::String(review.state.clone()),
                        );

                        message.metadata.insert(
                            "comment_body".to_string(),
                            serde_json::Value::String(review.body.clone()),
                        );

                        let review_section = format!(
                            "\n\n---\nReview by {} ({}):\n\n{}",
                            review.user.login, review.state, review.body
                        );
                        match &mut message.content.text {
                            Some(text) => text.push_str(&review_section),
                            None => message.content.text = Some(review_section),
                        }

                        if let Some(ref role) = comment_role {
                            message.metadata.insert(
                                "comment_role".to_string(),
                                serde_json::Value::String(role.clone()),
                            );
                        }

                        tracing::info!(
                            channel = %self.channel_name,
                            event = "review_submitted",
                            review_id = review.id,
                            pr_number = pr_number,
                            review_state = %review.state,
                            user = %review.user.login,
                            "PR review detected → routing"
                        );

                        if let Err(e) = (options.on_message)(message) {
                            tracing::error!(error = %e, pr_number = pr_number, "Failed to route review event");
                        }

                        self.track_comment(&review_key, processed_comments).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        channel = %self.channel_name,
                        pr_number = pr_number,
                        error = %e,
                        "Failed to fetch reviews for PR"
                    );
                }
            }

            // Process review comments
            match client.list_review_comments(*pr_number).await {
                Ok(review_comments) => {
                    tracing::trace!(
                        channel = %self.channel_name,
                        pr_number = pr_number,
                        count = review_comments.len(),
                        "Fetched review comments for PR"
                    );

                    for rc in &review_comments {
                        let rc_key = format!("review-comment-{}:{}", rc.id, rc.updated_at);

                        if processed_comments.contains(&rc_key) {
                            continue;
                        }

                        let body_trimmed = rc.body.trim();

                        let comment_role = extract_comment_role(body_trimmed);

                        let (title, github_type, labels, assignees) = issue_cache
                            .get(pr_number)
                            .cloned()
                            .unwrap_or_else(|| (format!("#{}", pr_number), "pull_request".to_string(), vec![], vec![]));

                        let event_uid = format!("review-comment-{}", rc.id);

                        let mut message = self.build_trigger_message(
                            "pull_request_review_comment",
                            *pr_number,
                            &title,
                            &github_type,
                            "created",
                            &rc.user.login,
                            &labels,
                            &assignees,
                            &event_uid,
                        );

                        message.metadata.insert(
                            "comment_body".to_string(),
                            serde_json::Value::String(rc.body.clone()),
                        );

                        let mut context_parts = Vec::new();
                        if let Some(ref path) = rc.path {
                            if let Some(line) = rc.line {
                                context_parts.push(format!("{}:{}", path, line));
                            } else {
                                context_parts.push(path.clone());
                            }
                        }

                        let review_comment_section = if context_parts.is_empty() {
                            format!(
                                "\n\n---\nReview comment by {}:\n\n{}",
                                rc.user.login, rc.body
                            )
                        } else {
                            format!(
                                "\n\n---\nReview comment by {} on {}:\n\n{}",
                                rc.user.login, context_parts.join(", "), rc.body
                            )
                        };
                        match &mut message.content.text {
                            Some(text) => text.push_str(&review_comment_section),
                            None => message.content.text = Some(review_comment_section),
                        }

                        if let Some(ref role) = comment_role {
                            message.metadata.insert(
                                "comment_role".to_string(),
                                serde_json::Value::String(role.clone()),
                            );
                        }

                        tracing::info!(
                            channel = %self.channel_name,
                            event = "review_comment",
                            comment_id = rc.id,
                            pr_number = pr_number,
                            path = ?rc.path,
                            line = ?rc.line,
                            user = %rc.user.login,
                            "PR review comment detected → routing"
                        );

                        if let Err(e) = (options.on_message)(message) {
                            tracing::error!(error = %e, pr_number = pr_number, "Failed to route review comment event");
                        }

                        self.track_comment(&rc_key, processed_comments).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        channel = %self.channel_name,
                        pr_number = pr_number,
                        error = %e,
                        "Failed to fetch review comments for PR"
                    );
                }
            }
        }

        // 2c. Poll CI check-run status for open PRs (if enabled).
        if self.config.poll_ci_status {
            for pr_number in &open_pr_numbers {
                let head_sha = match client.get_pr_head_sha(*pr_number).await {
                    Ok(sha) => sha,
                    Err(e) => {
                        tracing::warn!(
                            channel = %self.channel_name,
                            pr_number = pr_number,
                            error = %e,
                            "Failed to get PR head SHA for CI polling"
                        );
                        continue;
                    }
                };

                let check_runs = match client.list_check_runs(&head_sha).await {
                    Ok(runs) => runs,
                    Err(e) => {
                        tracing::warn!(
                            channel = %self.channel_name,
                            pr_number = pr_number,
                            error = %e,
                            "Failed to list check runs for CI polling"
                        );
                        continue;
                    }
                };

                let has_failure = check_runs
                    .iter()
                    .any(|cr| cr.conclusion.as_deref() == Some("failure") || cr.conclusion.as_deref() == Some("timed_out"));
                let all_completed = check_runs
                    .iter()
                    .all(|cr| cr.status == "completed");

                let overall_status = if has_failure {
                    "failure"
                } else if !all_completed {
                    "pending"
                } else {
                    "success"
                };

                let tracked_status = ci_status
                    .get(pr_number)
                    .map(|(sha, status)| (sha.clone(), status.clone()));

                // Reset tracking if head_sha changed (new commit pushed)
                let should_reset = tracked_status
                    .as_ref()
                    .map(|(tracked_sha, _)| tracked_sha != &head_sha)
                    .unwrap_or(true);

                let previous_status = if should_reset {
                    None
                } else {
                    tracked_status.as_ref().map(|(_, s)| s.clone())
                };

                // Only trigger on transition TO "failure"
                if overall_status == "failure" && previous_status.as_deref() != Some("failure") {
                    let failed_checks: Vec<&super::client::GithubCheckRun> = check_runs
                        .iter()
                        .filter(|cr| cr.conclusion.as_deref() == Some("failure") || cr.conclusion.as_deref() == Some("timed_out"))
                        .collect();

                    let (title, github_type, labels, assignees) = issue_cache
                        .get(pr_number)
                        .cloned()
                        .unwrap_or_else(|| {
                            (
                                format!("#{}", pr_number),
                                "pull_request".to_string(),
                                vec![],
                                vec![],
                            )
                        });

                    let event_uid = format!("ci-{}-{}-failure", pr_number, head_sha.get(..12).unwrap_or(&head_sha));

                    let mut message = self.build_trigger_message(
                        "check_run",
                        *pr_number,
                        &title,
                        &github_type,
                        "completed",
                        "github-actions",
                        &labels,
                        &assignees,
                        &event_uid,
                    );

                    message.metadata.insert(
                        "ci_head_sha".to_string(),
                        serde_json::Value::String(head_sha.clone()),
                    );

                    let failed_checks_json: Vec<serde_json::Value> = failed_checks
                        .iter()
                        .map(|cr| {
                            serde_json::json!({
                                "name": cr.name,
                                "conclusion": cr.conclusion.clone().unwrap_or_default(),
                            })
                        })
                        .collect();
                    message.metadata.insert(
                        "ci_failed_checks".to_string(),
                        serde_json::Value::Array(failed_checks_json),
                    );

                    let failed_names: Vec<String> = failed_checks
                        .iter()
                        .map(|cr| {
                            format!(
                                "- {} ({})",
                                cr.name,
                                cr.conclusion.as_deref().unwrap_or("unknown")
                            )
                        })
                        .collect();

                    let ci_section = format!(
                        "\n\n---\nCI check-run failure detected on commit {}:\n\n{}\n\nDiagnose:\n  gh pr checks {}",
                        head_sha.get(..8).unwrap_or(&head_sha),
                        failed_names.join("\n"),
                        pr_number
                    );
                    match &mut message.content.text {
                        Some(text) => text.push_str(&ci_section),
                        None => message.content.text = Some(ci_section),
                    }

                    tracing::info!(
                        channel = %self.channel_name,
                        event = "ci_failure",
                        pr_number = pr_number,
                        head_sha = %head_sha.get(..8).unwrap_or(&head_sha),
                        failed_count = failed_checks.len(),
                        "CI failure detected → routing to developer agent"
                    );

                    if let Err(e) = (options.on_message)(message) {
                        tracing::error!(error = %e, pr_number = pr_number, "Failed to route CI failure event");
                    }
                }

                self.track_ci_status(*pr_number, &head_sha, overall_status, ci_status)
                    .await;
            }
        }

        // 3. Detect closed issues/PRs by comparing cache with full open set.
        // Since we fetched ALL open issues (not just recently-updated ones),
        // the comparison is reliable: if an issue was in the cache but is not
        // in the current open set, it was genuinely closed.

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
///   "[High-Level Planner] planning" → Some("High-Level Planner")
///   "normal comment" → None
///   "[Unknown] something" → None
///
/// Only recognizes known agent roles to avoid false positives.
fn extract_comment_role(text: &str) -> Option<String> {
    if text.starts_with('[') {
        if let Some(end) = text.find(']') {
            let role = &text[1..end];
            match role {
                "Planner" | "Developer" | "Reviewer" | "High-Level Planner" => return Some(role.to_string()),
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

/// Check whether a comment should be processed based on whether its
/// parent issue/PR is still open.
///
/// Returns `true` if the comment's issue is in the open set and should
/// be routed to agents. Returns `false` if the issue is closed (not in
/// the open set) or if the issue number could not be parsed from the URL.
fn should_process_comment(comment: &GithubComment, open_numbers: &HashSet<u64>) -> bool {
    let issue_number = comment.issue_number().unwrap_or(0);
    open_numbers.contains(&issue_number)
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

    // --- Rule filtering (github_type, labels, assignees) ---

    /// Helper: create a message with labels and assignees metadata
    fn make_message_with_rules(
        github_type: &str,
        number: u64,
        labels: &[&str],
        assignees: &[&str],
    ) -> InboundMessage {
        let mut msg = make_message(github_type, number);
        msg.metadata.insert(
            "github_labels".to_string(),
            serde_json::json!(labels),
        );
        msg.metadata.insert(
            "github_assignees".to_string(),
            serde_json::json!(assignees),
        );
        msg
    }

    #[test]
    fn test_github_type_rule_blocks_wrong_type() {
        let msg = make_message("issue", 42);
        let patterns = vec![ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none(), "developer pattern should not match issue type");
    }

    #[test]
    fn test_github_type_rule_allows_correct_type() {
        let msg = make_message("issue", 42);
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "planner");
    }

    #[test]
    fn test_assignees_rule_blocks_wrong_assignee() {
        // Pattern requires assignee "alice", but issue is assigned to "bob"
        let mut msg = make_message_with_rules("issue", 42, &[], &["bob"]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![ChannelPattern {
            name: "planner".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                assignees: Some(vec!["alice".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none(), "should not match when assignee doesn't match");
    }

    #[test]
    fn test_assignees_rule_allows_matching_assignee() {
        // Pattern requires assignee "alice", issue is assigned to "alice"
        let mut msg = make_message_with_rules("issue", 42, &[], &["alice"]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![ChannelPattern {
            name: "planner".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                assignees: Some(vec!["alice".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "planner");
    }

    #[test]
    fn test_assignees_rule_or_logic() {
        // Pattern allows "alice" or "bob", issue assigned to "bob"
        let mut msg = make_message_with_rules("issue", 42, &[], &["bob"]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![ChannelPattern {
            name: "planner".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                assignees: Some(vec!["alice".to_string(), "bob".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some(), "should match when any assignee in the list matches");
    }

    #[test]
    fn test_assignees_rule_case_insensitive() {
        // Pattern has "Alice", issue has "alice"
        let mut msg = make_message_with_rules("issue", 42, &[], &["alice"]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![ChannelPattern {
            name: "planner".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                assignees: Some(vec!["Alice".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some(), "assignee matching should be case-insensitive");
    }

    #[test]
    fn test_labels_rule_blocks_wrong_label() {
        // Pattern requires label "bug", but issue has "enhancement"
        let mut msg = make_message_with_rules("pull_request", 43, &["enhancement"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        let patterns = vec![ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                labels: Some(LabelRule::Flat(vec!["bug".to_string()])),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none(), "should not match when label doesn't match");
    }

    #[test]
    fn test_labels_rule_allows_matching_label() {
        // Pattern requires label "bug", issue has "bug"
        let mut msg = make_message_with_rules("pull_request", 43, &["bug", "priority-high"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        let patterns = vec![ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                labels: Some(LabelRule::Flat(vec!["bug".to_string()])),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
    }

    #[test]
    fn test_labels_rule_case_insensitive() {
        // Pattern has "Bug", issue has "bug"
        let mut msg = make_message_with_rules("pull_request", 43, &["bug"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        let patterns = vec![ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                labels: Some(LabelRule::Flat(vec!["Bug".to_string()])),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some(), "label matching should be case-insensitive");
    }

    #[test]
    fn test_all_rules_and_logic() {
        // Pattern requires: pull_request AND label "ready-for-review" AND assignee "alice"
        // Message has all three — should match
        let mut msg = make_message_with_rules("pull_request", 43, &["ready-for-review"], &["alice"]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("reviewer"));
        let patterns = vec![ChannelPattern {
            name: "reviewer".to_string(),
            enabled: true,
            role: Some("Reviewer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                labels: Some(LabelRule::Flat(vec!["ready-for-review".to_string()])),
                assignees: Some(vec!["alice".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some(), "should match when all rules pass");
    }

    #[test]
    fn test_and_logic_partial_fail() {
        // Pattern requires: pull_request AND label "ready-for-review" AND assignee "alice"
        // Message has correct type and label but wrong assignee — should NOT match
        let mut msg = make_message_with_rules("pull_request", 43, &["ready-for-review"], &["bob"]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("reviewer"));
        let patterns = vec![ChannelPattern {
            name: "reviewer".to_string(),
            enabled: true,
            role: Some("Reviewer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                labels: Some(LabelRule::Flat(vec!["ready-for-review".to_string()])),
                assignees: Some(vec!["alice".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none(), "should not match when any AND rule fails");
    }

    #[test]
    fn test_no_rules_always_matches() {
        // Pattern with no rules (all None) — should match purely on role
        let mut msg = make_message_with_rules("issue", 42, &["any-label"], &["anyone"]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![ChannelPattern {
            name: "planner".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: crate::channels::types::PatternRules::default(),
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some(), "no rules means match on role alone");
    }

    #[test]
    fn test_no_assignees_on_issue_fails_assignee_rule() {
        // Pattern requires assignee "alice", but issue has no assignees
        let mut msg = make_message_with_rules("issue", 42, &[], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![ChannelPattern {
            name: "planner".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                assignees: Some(vec!["alice".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none(), "no assignees on issue should fail assignee rule");
    }

    #[test]
    fn test_fallback_to_second_pattern_when_first_rules_fail() {
        // Two patterns with same role but different rules.
        // First requires assignee "alice", second has no assignee rule.
        // Message has assignee "bob" — should skip first, match second.
        let mut msg = make_message_with_rules("issue", 42, &[], &["bob"]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![
            ChannelPattern {
                name: "planner-alice".to_string(),
                enabled: true,
                role: Some("Planner".to_string()),
                rules: crate::channels::types::PatternRules {
                    github_type: Some(vec!["issue".to_string()]),
                    assignees: Some(vec!["alice".to_string()]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ChannelPattern {
                name: "planner-default".to_string(),
                enabled: true,
                role: Some("Planner".to_string()),
                rules: crate::channels::types::PatternRules {
                    github_type: Some(vec!["issue".to_string()]),
                    ..Default::default()
                },
                ..Default::default()
            },
        ];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "planner-default",
            "should fall through to second pattern when first pattern's rules don't match");
    }

    // --- Helper function tests ---

    #[test]
    fn test_extract_comment_role() {
        assert_eq!(extract_comment_role("[Developer] some text"), Some("Developer".to_string()));
        assert_eq!(extract_comment_role("[Reviewer] code looks good"), Some("Reviewer".to_string()));
        assert_eq!(extract_comment_role("[Planner] questions"), Some("Planner".to_string()));
        assert_eq!(extract_comment_role("[High-Level Planner] planning"), Some("High-Level Planner".to_string()));
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
            poll_ci_status: true,
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
            poll_ci_status: true,
        };
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut processed = HashSet::new();

        // Track comments with id:updated_at keys
        adapter.track_comment("100:2024-01-01T00:00:00Z", &mut processed).await;
        adapter.track_comment("200:2024-01-02T00:00:00Z", &mut processed).await;
        adapter.track_comment("300:2024-01-03T00:00:00Z", &mut processed).await;

        assert_eq!(processed.len(), 3);
        assert!(processed.contains("100:2024-01-01T00:00:00Z"));
        assert!(processed.contains("200:2024-01-02T00:00:00Z"));
        assert!(processed.contains("300:2024-01-03T00:00:00Z"));

        // Reload from disk — should get same set
        let reloaded = adapter.load_processed_comments().await;
        assert_eq!(reloaded.len(), 3);
        assert!(reloaded.contains("100:2024-01-01T00:00:00Z"));
    }

    #[tokio::test]
    async fn test_edited_comment_reprocessed() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = GithubConfig {
            owner: "test".to_string(),
            repo: "test".to_string(),
            token: "test".to_string(),
            api_url: "https://api.github.com".to_string(),
            poll_interval_secs: 60,
            poll_ci_status: true,
        };
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut processed = HashSet::new();

        // Track comment with original updated_at
        adapter.track_comment("100:2024-01-01T00:00:00Z", &mut processed).await;
        assert!(processed.contains("100:2024-01-01T00:00:00Z"));

        // Same comment ID but different updated_at (edited) — should NOT be in set
        assert!(!processed.contains("100:2024-01-01T12:00:00Z"));

        // Track the edited version
        adapter.track_comment("100:2024-01-01T12:00:00Z", &mut processed).await;

        // Now both versions are tracked
        assert_eq!(processed.len(), 2);
        assert!(processed.contains("100:2024-01-01T00:00:00Z"));
        assert!(processed.contains("100:2024-01-01T12:00:00Z"));
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
            poll_ci_status: true,
        };
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        // Create a set with 3000 entries (key format: "id:timestamp")
        let mut processed: HashSet<String> = (1u64..=3000)
            .map(|id| format!("{id}:2024-01-01T00:00:00Z"))
            .collect();

        // Compact should keep only the 2000 highest IDs
        adapter.compact_processed_comments(&mut processed).await;

        assert_eq!(processed.len(), 2000);
        // Lowest kept should be 1001
        assert!(!processed.contains("1:2024-01-01T00:00:00Z"));
        assert!(!processed.contains("1000:2024-01-01T00:00:00Z"));
        assert!(processed.contains("1001:2024-01-01T00:00:00Z"));
        assert!(processed.contains("3000:2024-01-01T00:00:00Z"));

        // Verify file was rewritten correctly
        let reloaded = adapter.load_processed_comments().await;
        assert_eq!(reloaded.len(), 2000);
        assert!(reloaded.contains("1001:2024-01-01T00:00:00Z"));
        assert!(reloaded.contains("3000:2024-01-01T00:00:00Z"));
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
            poll_ci_status: true,
        };
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        // Set with fewer than 2000 entries — compact should be a no-op
        let mut processed: HashSet<String> = (1u64..=100)
            .map(|id| format!("{id}:2024-01-01T00:00:00Z"))
            .collect();
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
            poll_ci_status: true,
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
            poll_ci_status: true,
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

    // --- Trigger mode tests ---

    #[test]
    fn test_pattern_issue_matches() {
        let msg = make_message("issue", 42);
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "planner");
    }

    #[test]
    fn test_pattern_pr_matches() {
        let msg = make_message("pull_request", 43);
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "developer");
    }

    #[test]
    fn test_pattern_self_loop_prevention() {
        let mut msg = make_message("pull_request", 43);
        msg.metadata.insert("comment_role".to_string(), serde_json::json!("Developer"));
        let patterns = vec![ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    #[test]
    fn test_pattern_blocks_wrong_type() {
        let msg = make_message("issue", 42);
        let patterns = vec![ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    // --- Nested AND/OR label logic tests ---

    #[test]
    fn test_labels_nested_and_or() {
        // Nested: [["bug", "enhancement"], ["test"]] → (bug OR enhancement) AND test
        // Message has ["bug", "test"] → should match
        let mut msg = make_message_with_rules("pull_request", 43, &["bug", "test"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        let patterns = vec![ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                labels: Some(LabelRule::Nested(vec![
                    vec!["bug".to_string(), "enhancement".to_string()],
                    vec!["test".to_string()],
                ])),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some(), "should match when both AND groups are satisfied");

        // Message has ["bug", "other"] → should NOT match (missing "test" group)
        let mut msg2 = make_message_with_rules("pull_request", 44, &["bug", "other"], &[]);
        msg2.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        let result2 = GithubMatcher.match_message(&msg2, &patterns);
        assert!(result2.is_none(), "should not match when second AND group is not satisfied");
    }

    #[test]
    fn test_labels_nested_single_group() {
        // Nested with single group: [["bug"]] behaves same as flat ["bug"]
        let mut msg = make_message_with_rules("pull_request", 43, &["bug"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        let patterns = vec![ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                labels: Some(LabelRule::Nested(vec![
                    vec!["bug".to_string()],
                ])),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some(), "single nested group should behave like flat");
    }

    #[test]
    fn test_labels_nested_all_and() {
        // Nested: [["bug"], ["test"], ["v2"]] → requires all three labels
        let mut msg = make_message_with_rules("pull_request", 43, &["bug", "test", "v2"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        let patterns = vec![ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                labels: Some(LabelRule::Nested(vec![
                    vec!["bug".to_string()],
                    vec!["test".to_string()],
                    vec!["v2".to_string()],
                ])),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some(), "should match when all three labels are present");

        // Missing one label → should NOT match
        let mut msg2 = make_message_with_rules("pull_request", 44, &["bug", "test"], &[]);
        msg2.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        let result2 = GithubMatcher.match_message(&msg2, &patterns);
        assert!(result2.is_none(), "should not match when one required label is missing");
    }

    #[test]
    fn test_labels_nested_empty_group() {
        // Edge case: empty inner group [[]] should not block matching
        let mut msg = make_message_with_rules("pull_request", 43, &["bug"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        let patterns = vec![ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                labels: Some(LabelRule::Nested(vec![
                    vec!["bug".to_string()],
                    vec![],  // empty group — should be treated as always-match
                ])),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some(), "empty inner group should not block matching");
    }

    #[test]
    fn test_labels_flat_backward_compat() {
        // Verify Flat(vec!["bug", "enhancement"]) still uses OR logic
        let mut msg = make_message_with_rules("pull_request", 43, &["enhancement"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        let patterns = vec![ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                labels: Some(LabelRule::Flat(vec!["bug".to_string(), "enhancement".to_string()])),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some(), "flat labels should use OR logic — enhancement matches");

        // Neither label present → should NOT match
        let mut msg2 = make_message_with_rules("pull_request", 44, &["other"], &[]);
        msg2.metadata.insert("handover_role".to_string(), serde_json::json!("developer"));
        let result2 = GithubMatcher.match_message(&msg2, &patterns);
        assert!(result2.is_none(), "flat labels OR logic — no matching label");
    }

    // --- TOML deserialization tests for LabelRule ---

    #[test]
    fn test_labels_toml_flat_deserialize() {
        let pattern: ChannelPattern = toml::from_str(r#"
            name = "test"
            [rules]
            labels = ["bug", "enhancement"]
        "#).unwrap();
        assert!(
            matches!(pattern.rules.labels, Some(LabelRule::Flat(_))),
            "flat TOML array should deserialize as LabelRule::Flat"
        );
        if let Some(LabelRule::Flat(labels)) = &pattern.rules.labels {
            assert_eq!(labels, &["bug", "enhancement"]);
        }
    }

    #[test]
    fn test_labels_toml_nested_deserialize() {
        let pattern: ChannelPattern = toml::from_str(r#"
            name = "test"
            [rules]
            labels = [["bug", "enhancement"], ["test"]]
        "#).unwrap();
        assert!(
            matches!(pattern.rules.labels, Some(LabelRule::Nested(_))),
            "nested TOML array should deserialize as LabelRule::Nested"
        );
        if let Some(LabelRule::Nested(groups)) = &pattern.rules.labels {
            assert_eq!(groups.len(), 2);
            assert_eq!(groups[0], vec!["bug", "enhancement"]);
            assert_eq!(groups[1], vec!["test"]);
        }
    }

    // --- Closed issue/PR comment filtering (issue #89) ---

    /// Tests `should_process_comment` — the helper that decides whether a
    /// comment should be routed to agents based on whether its parent
    /// issue/PR is still open.
    #[test]
    fn test_should_process_comment_open_issue() {
        use super::super::client::{GithubComment, GithubUser};

        let open_numbers: HashSet<u64> = [10, 20].into_iter().collect();

        // Comment on open issue #10 → should be processed
        let comment = GithubComment {
            id: 1,
            user: GithubUser { login: "user1".to_string() },
            body: "test".to_string(),
            issue_url: "https://api.github.com/repos/owner/repo/issues/10".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        assert!(
            should_process_comment(&comment, &open_numbers),
            "comment on open issue #10 should be processed"
        );
    }

    #[test]
    fn test_should_process_comment_closed_issue() {
        use super::super::client::{GithubComment, GithubUser};

        let open_numbers: HashSet<u64> = [10, 20].into_iter().collect();

        // Comment on closed issue #30 → should be skipped
        let comment = GithubComment {
            id: 2,
            user: GithubUser { login: "user1".to_string() },
            body: "test".to_string(),
            issue_url: "https://api.github.com/repos/owner/repo/issues/30".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        assert!(
            !should_process_comment(&comment, &open_numbers),
            "comment on closed issue #30 should be skipped"
        );
    }

    #[test]
    fn test_should_process_comment_malformed_url() {
        use super::super::client::{GithubComment, GithubUser};

        let open_numbers: HashSet<u64> = [10, 20].into_iter().collect();

        // Comment with malformed issue_url → issue_number() returns None,
        // unwrap_or(0) yields 0, which is not in open set → should be skipped
        let comment = GithubComment {
            id: 3,
            user: GithubUser { login: "user1".to_string() },
            body: "test".to_string(),
            issue_url: "not-a-valid-url".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        assert!(
            !should_process_comment(&comment, &open_numbers),
            "comment with malformed URL should be skipped (issue_number falls back to 0)"
        );
    }

    // --- exclude_labels tests ---

    #[test]
    fn test_exclude_labels_blocks_matching_label() {
        // Message has "feature-plan", pattern has exclude_labels = ["feature-plan"] → should NOT match
        let mut msg = make_message_with_rules("issue", 42, &["feature-plan"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![ChannelPattern {
            name: "detail-planner".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                exclude_labels: Some(vec!["feature-plan".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none(), "exclude_labels should block matching when label is present");
    }

    #[test]
    fn test_exclude_labels_allows_non_matching_label() {
        // Message has "bug", pattern has exclude_labels = ["feature-plan"] → should match
        let mut msg = make_message_with_rules("issue", 42, &["bug"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![ChannelPattern {
            name: "detail-planner".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                exclude_labels: Some(vec!["feature-plan".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some(), "exclude_labels should not block when excluded label is absent");
    }

    #[test]
    fn test_exclude_labels_multiple() {
        // Message has "feature-plan", pattern has exclude_labels = ["feature-plan", "wip"] → should NOT match
        let mut msg = make_message_with_rules("issue", 42, &["feature-plan"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![ChannelPattern {
            name: "detail-planner".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                exclude_labels: Some(vec!["feature-plan".to_string(), "wip".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none(), "exclude_labels should block when any exclude label matches");
    }

    #[test]
    fn test_exclude_labels_case_insensitive() {
        // Message has "Feature-Plan", pattern has exclude_labels = ["feature-plan"] → should NOT match
        let mut msg = make_message_with_rules("issue", 42, &["Feature-Plan"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![ChannelPattern {
            name: "detail-planner".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                exclude_labels: Some(vec!["feature-plan".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none(), "exclude_labels matching should be case-insensitive");
    }

    #[test]
    fn test_exclude_labels_toml_deserialize() {
        let pattern: ChannelPattern = toml::from_str(r#"
            name = "test"
            [rules]
            github_type = ["issue"]
            exclude_labels = ["feature-plan", "wip"]
        "#).unwrap();
        assert!(pattern.rules.exclude_labels.is_some(), "exclude_labels should deserialize");
        let excl = pattern.rules.exclude_labels.unwrap();
        assert_eq!(excl.len(), 2);
        assert!(excl.contains(&"feature-plan".to_string()));
        assert!(excl.contains(&"wip".to_string()));
    }

    #[test]
    fn test_high_level_planner_with_feature_plan_label() {
        // High-level planner pattern: labels = ["feature-plan"]
        let mut msg = make_message_with_rules("issue", 42, &["feature-plan"], &[]);
        msg.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![ChannelPattern {
            name: "high-level-planner".to_string(),
            enabled: true,
            role: Some("High-Level Planner".to_string()),
            template: Some("github-high-level-planner".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                labels: Some(LabelRule::Flat(vec!["feature-plan".to_string()])),
                ..Default::default()
            },
            ..Default::default()
        }];
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some(), "high-level planner should match issue with feature-plan label");
        assert_eq!(result.unwrap().pattern_name, "high-level-planner");
    }

    #[test]
    fn test_two_level_routing() {
        // Issue with feature-plan → high-level planner
        let mut msg1 = make_message_with_rules("issue", 42, &["feature-plan"], &[]);
        msg1.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let patterns = vec![
            ChannelPattern {
                name: "high-level-planner".to_string(),
                enabled: true,
                role: Some("High-Level Planner".to_string()),
                template: Some("github-high-level-planner".to_string()),
                rules: crate::channels::types::PatternRules {
                    github_type: Some(vec!["issue".to_string()]),
                    labels: Some(LabelRule::Flat(vec!["feature-plan".to_string()])),
                    ..Default::default()
                },
                ..Default::default()
            },
            ChannelPattern {
                name: "detail-planner".to_string(),
                enabled: true,
                role: Some("Planner".to_string()),
                template: Some("github-planner".to_string()),
                rules: crate::channels::types::PatternRules {
                    github_type: Some(vec!["issue".to_string()]),
                    exclude_labels: Some(vec!["feature-plan".to_string()]),
                    ..Default::default()
                },
                ..Default::default()
            },
        ];
        let result1 = GithubMatcher.match_message(&msg1, &patterns);
        assert!(result1.is_some());
        assert_eq!(result1.unwrap().pattern_name, "high-level-planner");

        // Issue with bug → detail planner (no feature-plan)
        let mut msg2 = make_message_with_rules("issue", 43, &["bug"], &[]);
        msg2.metadata.insert("handover_role".to_string(), serde_json::json!("planner"));
        let result2 = GithubMatcher.match_message(&msg2, &patterns);
        assert!(result2.is_some());
        assert_eq!(result2.unwrap().pattern_name, "detail-planner");
    }

    #[test]
    fn test_review_dedup_key_format() {
        let mut processed: HashSet<String> = HashSet::new();
        processed.insert("review-123:2026-04-15T10:00:00Z".to_string());
        processed.insert("review-comment-456:2026-04-15T10:00:00Z".to_string());

        assert!(processed.contains("review-123:2026-04-15T10:00:00Z"));
        assert!(processed.contains("review-comment-456:2026-04-15T10:00:00Z"));
        assert!(!processed.contains("review-123:2026-04-16T10:00:00Z"));
    }

    #[test]
    fn test_review_comment_role_extraction() {
        assert_eq!(extract_comment_role("[Developer] Fixed the issue"), Some("Developer".to_string()));
        assert_eq!(extract_comment_role("[Reviewer] Looks good"), Some("Reviewer".to_string()));
        assert_eq!(extract_comment_role("[Planner] Planning phase"), Some("Planner".to_string()));
        assert_eq!(extract_comment_role("Normal review comment"), None);
    }

    #[test]
    fn test_review_dedup_key_uniqueness() {
        let key1 = format!("review-{}:{}", 123, "2026-04-15T10:00:00Z");
        let key2 = format!("review-{}:{}", 123, "2026-04-16T10:00:00Z");
        let key3 = format!("review-comment-{}:{}", 456, "2026-04-15T10:00:00Z");

        assert_ne!(key1, key2, "Same review ID with different submitted_at should be different keys");
        assert_ne!(key1, key3, "Review and review comment keys should be different");
    }

    // --- CI status tracking tests ---

    fn make_ci_test_config() -> GithubConfig {
        GithubConfig {
            owner: "test".to_string(),
            repo: "test".to_string(),
            token: "test".to_string(),
            api_url: "https://api.github.com".to_string(),
            poll_interval_secs: 60,
            poll_ci_status: true,
        }
    }

    #[tokio::test]
    async fn test_ci_status_load_and_track() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut ci_status: HashMap<u64, (String, String)> = HashMap::new();

        adapter.track_ci_status(42, "abc123", "pending", &mut ci_status).await;
        adapter.track_ci_status(43, "def456", "failure", &mut ci_status).await;

        assert_eq!(ci_status.len(), 2);
        assert_eq!(ci_status.get(&42), Some(&("abc123".to_string(), "pending".to_string())));
        assert_eq!(ci_status.get(&43), Some(&("def456".to_string(), "failure".to_string())));

        let reloaded = adapter.load_ci_status().await;
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.get(&42), Some(&("abc123".to_string(), "pending".to_string())));
        assert_eq!(reloaded.get(&43), Some(&("def456".to_string(), "failure".to_string())));
    }

    #[tokio::test]
    async fn test_ci_status_transition_triggers_message() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut ci_status: HashMap<u64, (String, String)> = HashMap::new();

        // PR 42 was previously tracked as "pending"
        adapter.track_ci_status(42, "abc123", "pending", &mut ci_status).await;

        // Simulate transition: same head_sha, status changes to "failure"
        let (tracked_sha, previous_status) = ci_status.get(&42).cloned().unwrap();
        assert_eq!(tracked_sha, "abc123");
        assert_eq!(previous_status, "pending");

        // The polling logic would detect: overall_status="failure" && previous_status != "failure"
        // → trigger message. We test the state management part here.
        let should_trigger = previous_status != "failure";
        assert!(should_trigger, "Transition from pending to failure should trigger message");

        // Update to failure
        adapter.track_ci_status(42, "abc123", "failure", &mut ci_status).await;
        assert_eq!(ci_status.get(&42), Some(&("abc123".to_string(), "failure".to_string())));
    }

    #[tokio::test]
    async fn test_ci_status_no_retrigger_on_same_failure() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut ci_status: HashMap<u64, (String, String)> = HashMap::new();

        // PR 42 is already tracked as "failure"
        adapter.track_ci_status(42, "abc123", "failure", &mut ci_status).await;

        // On next poll, same failure status — should NOT re-trigger
        let (_, previous_status) = ci_status.get(&42).cloned().unwrap();
        let should_trigger = previous_status != "failure";
        assert!(!should_trigger, "Same failure status should not re-trigger");
    }

    #[tokio::test]
    async fn test_ci_status_resets_on_new_commit() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut ci_status: HashMap<u64, (String, String)> = HashMap::new();

        // PR 42 tracked with old head_sha in "failure" status
        adapter.track_ci_status(42, "abc123", "failure", &mut ci_status).await;

        // Developer pushes a fix — new head_sha
        let new_head_sha = "def456";
        let (tracked_sha, _) = ci_status.get(&42).cloned().unwrap();

        // head_sha changed → tracking should reset
        let should_reset = tracked_sha != new_head_sha;
        assert!(should_reset, "New commit should reset CI status tracking");

        // After reset, previous_status would be None → transition to any status triggers
        // Simulate: new commit's CI is still pending
        adapter.track_ci_status(42, new_head_sha, "pending", &mut ci_status).await;
        assert_eq!(
            ci_status.get(&42),
            Some(&("def456".to_string(), "pending".to_string()))
        );

        // Now CI fails on new commit → should trigger (because we reset)
        let (_, previous_status) = ci_status.get(&42).cloned().unwrap();
        let should_trigger = previous_status != "failure";
        assert!(should_trigger, "Failure on new commit should trigger after reset");
    }

    #[tokio::test]
    async fn test_ci_status_empty_load() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let ci_status = adapter.load_ci_status().await;
        assert!(ci_status.is_empty());
    }

    #[tokio::test]
    async fn test_ci_status_compact() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut ci_status: HashMap<u64, (String, String)> = HashMap::new();

        // Add entries
        for i in 1..=100u64 {
            adapter
                .track_ci_status(i, &format!("sha{}", i), "success", &mut ci_status)
                .await;
        }

        assert_eq!(ci_status.len(), 100);

        // Compact should rewrite the file (no size reduction since under threshold)
        adapter.compact_ci_status(&mut ci_status).await;
        assert_eq!(ci_status.len(), 100);

        // Verify persistence
        let reloaded = adapter.load_ci_status().await;
        assert_eq!(reloaded.len(), 100);
    }

    #[test]
    fn test_ci_failure_message_routing() {
        // CI failure messages with github_type=pull_request should route to pr-{N}
        let mut msg = make_message("pull_request", 43);
        msg.metadata.insert(
            "github_event".to_string(),
            serde_json::Value::String("check_run".to_string()),
        );
        msg.metadata.insert(
            "github_action".to_string(),
            serde_json::Value::String("completed".to_string()),
        );

        let name = GithubMatcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "pr-43");

        // CI failure with developer pattern match should also route to pr-{N}
        let pm = PatternMatch {
            pattern_name: "developer".to_string(),
            channel: "github".to_string(),
            matches: HashMap::new(),
        };
        let name_with_pattern = GithubMatcher.derive_thread_name(&msg, &[], Some(&pm));
        assert_eq!(name_with_pattern, "pr-43");
    }

    #[tokio::test]
    async fn test_ci_status_no_file_growth_on_unchanged() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path());
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut ci_status: HashMap<u64, (String, String)> = HashMap::new();

        adapter.track_ci_status(42, "abc123", "failure", &mut ci_status).await;
        let file = adapter.state_dir.join("ci-status.txt");
        let lines_after_first = tokio::fs::read_to_string(&file).await.unwrap().lines().count();

        // Track same entry again — file should be rewritten (same content), not grow
        adapter.track_ci_status(42, "abc123", "failure", &mut ci_status).await;
        let lines_after_second = tokio::fs::read_to_string(&file).await.unwrap().lines().count();

        assert_eq!(lines_after_first, lines_after_second, "File should not grow when status is unchanged");
    }

    #[test]
    fn test_ci_timed_out_triggers_failure() {
        let check_runs = vec![
            super::super::client::GithubCheckRun {
                id: 1,
                name: "CI".to_string(),
                status: "completed".to_string(),
                conclusion: Some("timed_out".to_string()),
                head_sha: "abc123def456".to_string(),
                started_at: None,
                completed_at: None,
            },
        ];

        let has_failure = check_runs
            .iter()
            .any(|cr| cr.conclusion.as_deref() == Some("failure") || cr.conclusion.as_deref() == Some("timed_out"));

        assert!(has_failure, "timed_out should be treated as failure");

        let failed_checks: Vec<_> = check_runs
            .iter()
            .filter(|cr| cr.conclusion.as_deref() == Some("failure") || cr.conclusion.as_deref() == Some("timed_out"))
            .collect();

        assert_eq!(failed_checks.len(), 1);
        assert_eq!(failed_checks[0].name, "CI");
    }

    #[test]
    fn test_safe_head_sha_truncation() {
        let short_sha = "abc".to_string();
        let result = short_sha.get(..8).unwrap_or(&short_sha);
        assert_eq!(result, "abc");

        let normal_sha = "abc123def456".to_string();
        let result2 = normal_sha.get(..8).unwrap_or(&normal_sha);
        assert_eq!(result2, "abc123de");

        let empty_sha = "".to_string();
        let result3 = empty_sha.get(..8).unwrap_or(&empty_sha);
        assert_eq!(result3, "");
    }
}
