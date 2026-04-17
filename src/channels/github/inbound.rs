use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use std::collections::{HashMap, HashSet};
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
        // Priority 1: Hand-over marker (@jyc:<role>) — match by role name directly
        if let Some(handover_role) = message.metadata.get("handover_role").and_then(|v| v.as_str()) {
            for pattern in patterns {
                if !pattern.enabled {
                    continue;
                }
                if let Some(ref role) = pattern.role {
                    if role.eq_ignore_ascii_case(handover_role) {
                        return Some(PatternMatch {
                            pattern_name: pattern.name.clone(),
                            channel: "github".to_string(),
                            matches: HashMap::new(),
                        });
                    }
                }
            }
            // No pattern found with that role — fall through to normal matching
        }

        // Priority 2: Normal matching by github_type + labels
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

            // Check labels rule (OR logic: match if ANY label matches).
            // Effective labels = explicit config labels + auto-label from role.
            // Auto-label is derived from pattern.role (e.g., "Developer" → "jyc:develop").
            //
            // Auto-label only applies to pull_request patterns, not issue patterns.
            // Issues are created by users (who may not add routing labels).
            // PRs are created by agents (who add labels during hand-off).
            let is_pr_pattern = pattern
                .rules
                .github_type
                .as_ref()
                .map_or(false, |types| types.iter().any(|t| t == "pull_request"));
            let auto_label = if is_pr_pattern {
                pattern.role.as_deref().and_then(role_to_routing_label)
            } else {
                None
            };
            let has_label_rules = pattern.rules.labels.is_some() || auto_label.is_some();

            if has_label_rules {
                let mut label_matched = false;

                // Check explicit label rules from config
                if let Some(ref label_rules) = pattern.rules.labels {
                    if label_rules.iter().any(|rule| labels.contains(&rule.to_lowercase())) {
                        label_matched = true;
                    }
                }

                // Check auto-label derived from role
                if let Some(auto) = auto_label {
                    if labels.contains(&auto.to_lowercase()) {
                        label_matched = true;
                    }
                }

                if !label_matched {
                    continue;
                }
            }

            // Check assignees rule (OR logic: match if ANY assignee matches)
            // Case-insensitive comparison
            if let Some(ref assignee_rules) = pattern.rules.assignees {
                let assignees: Vec<String> = message
                    .metadata
                    .get("github_assignees")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                            .collect()
                    })
                    .unwrap_or_default();

                let rules_lower: Vec<String> = assignee_rules
                    .iter()
                    .map(|r| r.to_lowercase())
                    .collect();

                if !rules_lower.iter().any(|rule| assignees.contains(rule))
                {
                    continue;
                }
            }

            // Self-loop prevention: skip if comment is from this pattern's own role.
            // A [Developer] comment should NOT re-trigger the developer pattern,
            // but SHOULD be visible to the reviewer pattern.
            if let Some(comment_role) = message.metadata.get("comment_role").and_then(|v| v.as_str()) {
                if let Some(ref pattern_role) = pattern.role {
                    if pattern_role.eq_ignore_ascii_case(comment_role) {
                        continue;
                    }
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

        // Track processed event IDs for deduplication
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
    /// Execute one poll cycle: fetch issues, comments, and closed items.
    /// Routes events to threads via on_message callback.
    async fn poll_once(
        &self,
        client: &GithubClient,
        options: &InboundAdapterOptions,
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

        // Track which issue numbers had comments routed in this cycle.
        // If a comment was routed for an issue, skip the issue_updated event
        // to avoid duplicate triggers in the same poll cycle.
        let mut commented_issues: HashSet<u64> = HashSet::new();

        // 1. Fetch open issues/PRs FIRST to populate the cache.
        // Comments (step 2) need the cache to look up labels and github_type.
        // Without this, comments after a restart would have empty labels and fail to match.
        let issues = client.list_issues_since(&poll_start).await?;
        tracing::trace!(
            channel = %self.channel_name,
            count = issues.len(),
            "Fetched open issues/PRs"
        );

        // Pre-compute issue routing data (label changes, new issues) before processing.
        // We store tuples of (issue_ref, github_type, labels, old_labels, is_new, new_labels_added).
        struct IssueRouteInfo {
            number: u64,
            title: String,
            github_type: String,
            labels: Vec<String>,
            assignees: Vec<String>,
            user_login: String,
            is_newly_created: bool,
            new_labels_added: Vec<String>,
            new_assignees_added: Vec<String>,
        }

        let mut issue_route_infos: Vec<IssueRouteInfo> = Vec::new();

        for issue in &issues {
            let github_type = if issue.is_pull_request() { "pull_request" } else { "issue" };
            let labels: Vec<String> = issue.labels.iter().map(|l| l.name.clone()).collect();
            let assignees: Vec<String> = issue.assignees.iter().map(|a| a.login.clone()).collect();

            // Detect label changes before updating cache
            let old_data = issue_cache
                .get(&issue.number)
                .cloned()
                .unwrap_or_else(|| (String::new(), String::new(), vec![], vec![]));
            let old_labels = old_data.2;
            let old_assignees = old_data.3;

            let new_labels_added: Vec<String> = labels
                .iter()
                .filter(|l| !old_labels.iter().any(|o| o.eq_ignore_ascii_case(l)))
                .cloned()
                .collect();

            let new_assignees_added: Vec<String> = assignees
                .iter()
                .filter(|a| !old_assignees.iter().any(|o| o.eq_ignore_ascii_case(a)))
                .cloned()
                .collect();

            let is_newly_created = issue.created_at > poll_start;

            // Update cache (used by comment processing in step 2)
            issue_cache.insert(
                issue.number,
                (issue.title.clone(), github_type.to_string(), labels.clone(), assignees.clone()),
            );

            issue_route_infos.push(IssueRouteInfo {
                number: issue.number,
                title: issue.title.clone(),
                github_type: github_type.to_string(),
                labels,
                assignees,
                user_login: issue.user.login.clone(),
                is_newly_created,
                new_labels_added,
                new_assignees_added,
            });
        }

        // 2. Fetch and process comments (they are more specific triggers than issue_updated).
        // The issue cache is now populated, so label lookups work correctly.
        let comments = client.list_comments_since(&poll_start).await?;
        tracing::trace!(
            channel = %self.channel_name,
            count = comments.len(),
            "Fetched comments"
        );

        for comment in &comments {
            let body_trimmed = comment.body.trim();

            // Extract agent role from [Role] prefix (e.g., "[Developer] ..." → "Developer").
            // This is stored in metadata for self-loop prevention in the matcher.
            // Unlike the previous design which globally filtered ALL agent comments,
            // we now let them through — each pattern only skips its OWN role's comments.
            let comment_role = extract_comment_role(body_trimmed);

            let event_uid = format!("comment-{}", comment.id);

            if processed_events.contains(&event_uid) {
                continue;
            }

            let issue_number = comment.issue_number().unwrap_or(0);

            // Look up issue info from cache
            let (title, github_type, labels, assignees) = issue_cache
                .get(&issue_number)
                .cloned()
                .unwrap_or_else(|| (format!("#{}", issue_number), "issue".to_string(), vec![], vec![]));

            // Detect hand-over marker: @jyc:<role> (e.g., @jyc:developer, @jyc:reviewer)
            let handover_role = extract_handover_role(body_trimmed);

            if let Some(ref role) = handover_role {
                tracing::info!(
                    channel = %self.channel_name,
                    event = "handover",
                    comment_id = comment.id,
                    issue_number = issue_number,
                    target_role = %role,
                    user = %comment.user.login,
                    "Hand-over marker detected → routing to role"
                );
            } else {
                tracing::info!(
                    channel = %self.channel_name,
                    event = "comment",
                    comment_id = comment.id,
                    issue_number = issue_number,
                    user = %comment.user.login,
                    body_preview = %truncate_str(&comment.body, 80),
                    "GitHub comment detected → routing to thread"
                );
            }

            // Route: build trigger message and send to on_message
            let mut message = self.build_trigger_message(
                if handover_role.is_some() { "handover" } else { "issue_comment" },
                issue_number,
                &title,
                &github_type,
                if handover_role.is_some() { "handover" } else { "commented" },
                &comment.user.login,
                &labels,
                &assignees,
                &event_uid,
            );

            // Set handover_role in metadata if detected
            if let Some(role) = handover_role {
                message.metadata.insert(
                    "handover_role".to_string(),
                    serde_json::Value::String(role),
                );
            }

            // Set comment_role in metadata for self-loop prevention in matcher
            if let Some(ref role) = comment_role {
                message.metadata.insert(
                    "comment_role".to_string(),
                    serde_json::Value::String(role.clone()),
                );
            }

            if let Err(e) = (options.on_message)(message) {
                tracing::error!(error = %e, number = issue_number, "Failed to route comment event");
            }

            commented_issues.insert(issue_number);
            processed_events.insert(event_uid);
        }

        // 3. Route new issues/PRs and label changes (using pre-computed data from step 1).
        for info in &issue_route_infos {
            // Skip if a comment was already routed for this issue in this cycle
            if commented_issues.contains(&info.number) {
                continue;
            }

            if info.is_newly_created {
                // Route new issues/PRs
                let event_uid = format!("{}-{}-opened", info.github_type, info.number);

                if processed_events.contains(&event_uid) {
                    continue;
                }

                tracing::info!(
                    channel = %self.channel_name,
                    event = "opened",
                    number = info.number,
                    title = %info.title,
                    github_type = %info.github_type,
                    user = %info.user_login,
                    labels = ?info.labels,
                    "New issue/PR detected → routing to thread"
                );

                let message = self.build_trigger_message(
                    &format!("{}_opened", info.github_type),
                    info.number,
                    &info.title,
                    &info.github_type,
                    "opened",
                    &info.user_login,
                    &info.labels,
                    &info.assignees,
                    &event_uid,
                );
                if let Err(e) = (options.on_message)(message) {
                    tracing::error!(error = %e, number = info.number, "Failed to route new issue event");
                }

                processed_events.insert(event_uid);
            } else if !info.new_labels_added.is_empty() {
                // Route label change on existing issues/PRs.
                // This handles the case where a user adds a label (e.g., "jyc:plan")
                // to an existing issue that was previously unmatched.
                let event_uid = format!(
                    "{}-{}-labeled-{}",
                    info.github_type,
                    info.number,
                    info.new_labels_added.join(",")
                );

                if processed_events.contains(&event_uid) {
                    continue;
                }

                tracing::info!(
                    channel = %self.channel_name,
                    event = "labeled",
                    number = info.number,
                    title = %info.title,
                    github_type = %info.github_type,
                    new_labels = ?info.new_labels_added,
                    all_labels = ?info.labels,
                    "Label change detected → routing to thread"
                );

                let message = self.build_trigger_message(
                    &format!("{}_labeled", info.github_type),
                    info.number,
                    &info.title,
                    &info.github_type,
                    "labeled",
                    &info.user_login,
                    &info.labels,
                    &info.assignees,
                    &event_uid,
                );
                if let Err(e) = (options.on_message)(message) {
                    tracing::error!(error = %e, number = info.number, "Failed to route label change event");
                }

                processed_events.insert(event_uid);
            } else if !info.new_assignees_added.is_empty() {
                // Route assignee change on existing issues/PRs.
                // This handles the case where a user assigns someone (e.g., "alice")
                // to an existing issue that was previously unassigned.
                let event_uid = format!(
                    "{}-{}-assigned-{}",
                    info.github_type,
                    info.number,
                    info.new_assignees_added.join(",")
                );

                if processed_events.contains(&event_uid) {
                    continue;
                }

                tracing::info!(
                    channel = %self.channel_name,
                    event = "assigned",
                    number = info.number,
                    title = %info.title,
                    github_type = %info.github_type,
                    new_assignees = ?info.new_assignees_added,
                    all_assignees = ?info.assignees,
                    "Assignee change detected → routing to thread"
                );

                let message = self.build_trigger_message(
                    &format!("{}_assigned", info.github_type),
                    info.number,
                    &info.title,
                    &info.github_type,
                    "assigned",
                    &info.user_login,
                    &info.labels,
                    &info.assignees,
                    &event_uid,
                );
                if let Err(e) = (options.on_message)(message) {
                    tracing::error!(error = %e, number = info.number, "Failed to route assignee change event");
                }

                processed_events.insert(event_uid);
            }
        }

        // 3. Detect closed issues/PRs by comparing with previous cache.
        // Strategy: if an issue was in the previous cache but not in current open list,
        // it was closed since the last poll.
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

/// Extract hand-over role from text containing `@jyc:<role>` marker.
///
/// Examples:
///   "@jyc:developer Please implement this" → Some("developer")
///   "@jyc:reviewer Ready for review" → Some("reviewer")
///   "Normal comment without marker" → None
///
/// The marker is case-insensitive. Only the first match is returned.
fn extract_handover_role(text: &str) -> Option<String> {
    let re = Regex::new(r"(?i)@jyc:(\w+)").ok()?;
    re.captures(text)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_lowercase())
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

/// Map agent role name to its routing label.
///
/// These labels are a fixed convention, hardcoded in agent templates (AGENTS.md).
/// Agents add these labels when creating PRs or handing off to other agents.
/// The matcher uses them for automatic label-based routing.
///
/// Examples:
///   "Developer" → Some("jyc:develop")
///   "Reviewer"  → Some("jyc:review")
///   "Planner"   → Some("jyc:plan")
///   "Unknown"   → None
fn role_to_routing_label(role: &str) -> Option<&'static str> {
    match role.to_lowercase().as_str() {
        "developer" => Some("jyc:develop"),
        "reviewer" => Some("jyc:review"),
        "planner" => Some("jyc:plan"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(github_type: &str, number: u64, labels: &[&str]) -> InboundMessage {
        make_message_with_assignees(github_type, number, labels, &[])
    }

    fn make_message_with_assignees(github_type: &str, number: u64, labels: &[&str], assignees: &[&str]) -> InboundMessage {
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
        metadata.insert(
            "github_assignees".to_string(),
            serde_json::json!(assignees),
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
    fn test_derive_thread_name_reviewer() {
        let msg = make_message("pull_request", 43, &[]);
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
        let msg = make_message("pull_request", 43, &[]);
        let pm = PatternMatch {
            pattern_name: "developer".to_string(),
            channel: "github".to_string(),
            matches: HashMap::new(),
        };
        let name = GithubMatcher.derive_thread_name(&msg, &[], Some(&pm));
        assert_eq!(name, "pr-43");
    }

    // --- Auto-label routing ---

    #[test]
    fn test_match_issue_with_plan_label() {
        // Planner has role="Planner" but github_type=issue → no auto-label.
        // Explicit labels would still work if configured.
        let patterns = vec![ChannelPattern {
            name: "planner".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                labels: Some(vec!["plan".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];

        let msg = make_message("issue", 42, &["plan"]);
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "planner");
    }

    #[test]
    fn test_match_issue_without_label_matches_planner() {
        // Planner has role="Planner" + github_type=["issue"] → no auto-label required.
        // Issues match based on github_type alone (no label check when no explicit labels).
        let msg = make_message("issue", 42, &[]);
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "planner");
    }

    #[test]
    fn test_match_pr_with_develop_label() {
        let msg = make_message("pull_request", 43, &["jyc:develop"]);
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "developer");
    }

    #[test]
    fn test_match_pr_with_review_label() {
        let msg = make_message("pull_request", 43, &["jyc:review"]);
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
    fn test_match_pr_no_labels_no_match() {
        // Developer has role="Developer" → auto-label "jyc:develop" is required
        let msg = make_message("pull_request", 43, &[]);
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    #[test]
    fn test_match_explicit_labels_plus_auto_label() {
        // Pattern with both explicit labels and role (auto-label)
        let patterns = vec![ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                labels: Some(vec!["custom-dev".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];

        // Matches via explicit label
        let msg1 = make_message("pull_request", 43, &["custom-dev"]);
        let result1 = GithubMatcher.match_message(&msg1, &patterns);
        assert!(result1.is_some());
        assert_eq!(result1.unwrap().pattern_name, "developer");

        // Also matches via auto-label
        let msg2 = make_message("pull_request", 43, &["jyc:develop"]);
        let result2 = GithubMatcher.match_message(&msg2, &patterns);
        assert!(result2.is_some());
        assert_eq!(result2.unwrap().pattern_name, "developer");

        // Neither label → no match
        let msg3 = make_message("pull_request", 43, &["unrelated"]);
        let result3 = GithubMatcher.match_message(&msg3, &patterns);
        assert!(result3.is_none());
    }

    #[test]
    fn test_match_pattern_without_role_no_auto_label() {
        // Pattern without role → no auto-label, only explicit labels checked
        let patterns = vec![ChannelPattern {
            name: "catch_all".to_string(),
            enabled: true,
            role: None,
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];

        // No role + no labels config → matches all issues (no label check)
        let msg = make_message("issue", 42, &[]);
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "catch_all");
    }

    #[test]
    fn test_match_disabled_pattern_skipped() {
        let msg = make_message("issue", 42, &["jyc:plan"]);
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

    // --- Hand-over routing ---

    #[test]
    fn test_match_handover_by_role() {
        let mut msg = make_message("pull_request", 43, &[]);
        msg.metadata.insert(
            "handover_role".to_string(),
            serde_json::json!("developer"),
        );
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "developer");
    }

    #[test]
    fn test_match_handover_case_insensitive() {
        let mut msg = make_message("issue", 42, &[]);
        msg.metadata.insert(
            "handover_role".to_string(),
            serde_json::json!("Reviewer"),
        );
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "reviewer");
    }

    #[test]
    fn test_match_handover_unknown_role_falls_through() {
        // Unknown handover role falls through to normal matching.
        // Issue without label → matches planner (no auto-label for issue patterns).
        let mut msg = make_message("issue", 42, &[]);
        msg.metadata.insert(
            "handover_role".to_string(),
            serde_json::json!("unknown_role"),
        );
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "planner");
    }

    #[test]
    fn test_match_handover_unknown_role_pr_no_label_no_match() {
        // Unknown handover role on a PR without label → no match.
        // PR patterns still require auto-label.
        let mut msg = make_message("pull_request", 43, &[]);
        msg.metadata.insert(
            "handover_role".to_string(),
            serde_json::json!("unknown_role"),
        );
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    #[test]
    fn test_match_handover_bypasses_labels() {
        // Hand-over should match even without the routing label on the PR
        let mut msg = make_message("pull_request", 43, &[]);
        msg.metadata.insert(
            "handover_role".to_string(),
            serde_json::json!("reviewer"),
        );
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "reviewer");
    }

    // --- Self-loop prevention ---

    #[test]
    fn test_self_loop_developer_comment_skips_developer() {
        // [Developer] comment on a PR with jyc:develop label
        // Should NOT match developer (self-loop), should have no match
        // since reviewer requires jyc:review label which is not present
        let mut msg = make_message("pull_request", 43, &["jyc:develop"]);
        msg.metadata.insert(
            "comment_role".to_string(),
            serde_json::json!("Developer"),
        );
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    #[test]
    fn test_self_loop_reviewer_comment_skips_reviewer() {
        // [Reviewer] comment on a PR with jyc:review label
        // Should NOT match reviewer (self-loop)
        let mut msg = make_message("pull_request", 43, &["jyc:review"]);
        msg.metadata.insert(
            "comment_role".to_string(),
            serde_json::json!("Reviewer"),
        );
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    #[test]
    fn test_cross_role_reviewer_comment_matches_developer() {
        // [Reviewer] comment on a PR with BOTH jyc:develop and jyc:review labels
        // Should skip reviewer (self-loop) but match developer
        let mut msg = make_message("pull_request", 43, &["jyc:develop", "jyc:review"]);
        msg.metadata.insert(
            "comment_role".to_string(),
            serde_json::json!("Reviewer"),
        );
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "developer");
    }

    #[test]
    fn test_cross_role_developer_comment_matches_reviewer() {
        // [Developer] comment on a PR with BOTH jyc:develop and jyc:review labels
        // Should skip developer (self-loop) but match reviewer
        let mut msg = make_message("pull_request", 43, &["jyc:develop", "jyc:review"]);
        msg.metadata.insert(
            "comment_role".to_string(),
            serde_json::json!("Developer"),
        );
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "reviewer");
    }

    #[test]
    fn test_human_comment_no_self_loop_check() {
        // Human comment (no comment_role) on PR with jyc:develop label
        // Should match developer normally
        let msg = make_message("pull_request", 43, &["jyc:develop"]);
        let patterns = make_patterns();
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "developer");
    }

    // --- Helper function tests ---

    #[test]
    fn test_extract_handover_role() {
        assert_eq!(
            extract_handover_role("@jyc:developer Please implement this"),
            Some("developer".to_string())
        );
        assert_eq!(
            extract_handover_role("@jyc:Reviewer Ready for review"),
            Some("reviewer".to_string())
        );
        assert_eq!(
            extract_handover_role("Normal comment without marker"),
            None
        );
        assert_eq!(
            extract_handover_role("Some text @jyc:planner more text"),
            Some("planner".to_string())
        );
        // Role prefix [Planner] is NOT a handover marker
        assert_eq!(
            extract_handover_role("[Planner] This is a reply"),
            None
        );
    }

    #[test]
    fn test_extract_comment_role() {
        assert_eq!(extract_comment_role("[Developer] some text"), Some("Developer".to_string()));
        assert_eq!(extract_comment_role("[Reviewer] code looks good"), Some("Reviewer".to_string()));
        assert_eq!(extract_comment_role("[Planner] questions"), Some("Planner".to_string()));
        assert_eq!(extract_comment_role("normal comment"), None);
        assert_eq!(extract_comment_role("[Unknown] something"), None);
        assert_eq!(extract_comment_role(""), None);
        assert_eq!(extract_comment_role("no bracket prefix"), None);
    }

    #[test]
    fn test_role_to_routing_label() {
        assert_eq!(role_to_routing_label("Developer"), Some("jyc:develop"));
        assert_eq!(role_to_routing_label("developer"), Some("jyc:develop"));
        assert_eq!(role_to_routing_label("Reviewer"), Some("jyc:review"));
        assert_eq!(role_to_routing_label("reviewer"), Some("jyc:review"));
        assert_eq!(role_to_routing_label("Planner"), Some("jyc:plan"));
        assert_eq!(role_to_routing_label("planner"), Some("jyc:plan"));
        assert_eq!(role_to_routing_label("Unknown"), None);
        assert_eq!(role_to_routing_label(""), None);
    }

    // --- Build trigger message ---

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
            &["alice".to_string(), "bob".to_string()],
            "pr-43-opened",
        );

        let text = msg.content.text.unwrap();
        assert!(text.contains("gh pr view 43"));
        assert!(text.contains("gh pr diff 43"));
        assert!(text.contains("assignees: alice, bob"));
    }

    #[test]
    fn test_build_trigger_message_no_assignees() {
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
            &[],
            "comment-12345",
        );

        let text = msg.content.text.unwrap();
        assert!(!text.contains("assignees:"));
    }

    // --- Assignee matching tests ---

    #[test]
    fn test_match_pr_with_assignee() {
        let patterns = vec![ChannelPattern {
            name: "assigned_to_alice".to_string(),
            enabled: true,
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                assignees: Some(vec!["alice".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];

        let msg = make_message_with_assignees("pull_request", 43, &[], &["alice", "bob"]);
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "assigned_to_alice");
    }

    #[test]
    fn test_match_pr_with_assignee_no_match() {
        let patterns = vec![ChannelPattern {
            name: "assigned_to_alice".to_string(),
            enabled: true,
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                assignees: Some(vec!["alice".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];

        let msg = make_message_with_assignees("pull_request", 43, &[], &["charlie", "david"]);
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }

    #[test]
    fn test_match_pr_assignee_and_label() {
        // Both assignee AND label must match (AND logic)
        let patterns = vec![ChannelPattern {
            name: "alice_develop".to_string(),
            enabled: true,
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                labels: Some(vec!["jyc:develop".to_string()]),
                assignees: Some(vec!["alice".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];

        // Both match → should match
        let msg1 = make_message_with_assignees("pull_request", 43, &["jyc:develop"], &["alice"]);
        let result1 = GithubMatcher.match_message(&msg1, &patterns);
        assert!(result1.is_some());
        assert_eq!(result1.unwrap().pattern_name, "alice_develop");

        // Only assignee matches → no match
        let msg2 = make_message_with_assignees("pull_request", 43, &["wip"], &["alice"]);
        let result2 = GithubMatcher.match_message(&msg2, &patterns);
        assert!(result2.is_none());

        // Only label matches → no match
        let msg3 = make_message_with_assignees("pull_request", 43, &["jyc:develop"], &["bob"]);
        let result3 = GithubMatcher.match_message(&msg3, &patterns);
        assert!(result3.is_none());
    }

    #[test]
    fn test_match_pr_assignee_case_insensitive() {
        let patterns = vec![ChannelPattern {
            name: "assigned_to_alice".to_string(),
            enabled: true,
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                assignees: Some(vec!["ALICE".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];

        // Config uses uppercase, message uses lowercase → should match
        let msg = make_message_with_assignees("pull_request", 43, &[], &["alice"]);
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "assigned_to_alice");
    }

    #[test]
    fn test_match_issue_with_assignee() {
        let patterns = vec![ChannelPattern {
            name: "assigned_to_bob".to_string(),
            enabled: true,
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                assignees: Some(vec!["bob".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];

        let msg = make_message_with_assignees("issue", 42, &[], &["bob"]);
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().pattern_name, "assigned_to_bob");
    }

    #[test]
    fn test_match_issue_no_assignees() {
        // Issue with no assignees should not match a pattern with assignee rules
        let patterns = vec![ChannelPattern {
            name: "assigned_to_bob".to_string(),
            enabled: true,
            rules: crate::channels::types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                assignees: Some(vec!["bob".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }];

        let msg = make_message_with_assignees("issue", 42, &[], &[]);
        let result = GithubMatcher.match_message(&msg, &patterns);
        assert!(result.is_none());
    }
}
