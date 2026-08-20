//! GitHub inbound polling — `start` / `poll_once` and helpers.
//!
//! Extracted from the monolithic `github/inbound.rs`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use tokio_util::sync::CancellationToken;

use crate::github::client::{GithubClient, GithubComment};
use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage,
    PatternMatch,
};

use super::{GithubInboundAdapter, GithubMatcher};

impl ChannelMatcher for GithubInboundAdapter {
    fn channel_type(&self) -> &str {
        "github"
    }

    fn derive_topic_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        GithubMatcher.derive_topic_name(message, patterns, pattern_match)
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
    async fn start(&self, options: InboundAdapterOptions, cancel: CancellationToken) -> Result<()> {
        // Create GitHub API client
        let client = GithubClient::new(&self.config).context("Failed to create GitHub client")?;

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
        tokio::fs::create_dir_all(&self.state_dir)
            .await
            .with_context(|| {
                format!(
                    "failed to create state directory: {}",
                    self.state_dir.display()
                )
            })?;
        let mut processed_comments: HashSet<String> = self.load_processed_comments().await;

        // Track processed event IDs for non-comment deduplication (close events)
        let mut processed_events: HashSet<String> = HashSet::new();

        // Load seen issues for deduplication (prevent re-triggering after restart)
        let mut seen_issues: HashSet<String> = self.load_seen_issues().await;

        // Cache issue info for comment routing (number → title, type, labels, assignees)
        let mut issue_cache: HashMap<u64, (String, String, Vec<String>, Vec<String>)> =
            HashMap::new();

        // Load CI status tracking for check-run polling
        let mut ci_status: HashMap<u64, (String, String)> = self.load_ci_status().await;

        // Determine poll start time:
        // - Fresh start (no processed-comments.txt): start from "now" to avoid
        //   replaying old comments.
        // - Restart (file exists): go back 5 minutes to catch events missed
        //   during downtime. Deduplication via processed-comments.txt prevents
        //   re-processing.
        let mut last_poll = if is_fresh_start {
            tracing::info!(
                channel = %self.channel_name,
                "Fresh start detected — polling from now (no backfill)"
            );
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
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
    /// Execute one poll cycle: fetch comments and route via pattern matching.
    /// Routes events to topics via on_message callback.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
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
        let mut triggered_in_cycle: HashSet<String> = HashSet::new();

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
            let github_type = if issue.is_pull_request() {
                "pull_request"
            } else {
                "issue"
            };
            let labels: Vec<String> = issue.labels.iter().map(|l| l.name.clone()).collect();
            let assignees: Vec<String> = issue.assignees.iter().map(|a| a.login.clone()).collect();

            issue_cache.insert(
                issue.number,
                (
                    issue.title.clone(),
                    github_type.to_string(),
                    labels.clone(),
                    assignees.clone(),
                ),
            );

            // Track seen issues for dedup (prevent re-triggering after restart).
            // Key = number:labels — triggers on first sight and label changes.
            // Does NOT include updated_at: comments (including agent's own replies)
            // update that timestamp, which would cause infinite re-triggering.
            let mut labels_sorted: Vec<String> =
                issue.labels.iter().map(|l| l.name.clone()).collect();
            labels_sorted.sort();
            let seen_key = format!("{}:{}", issue.number, labels_sorted.join(","));
            let is_new = !seen_issues.contains(&seen_key);
            self.track_seen_issue(&seen_key, seen_issues).await;

            // For new/changed issues, create a trigger message so Pattern-mode
            // patterns can match on issue metadata (type, labels, assignees)
            // without requiring a comment.
            if is_new {
                // Dedup: skip if this issue already triggered in this poll cycle
                if !triggered_in_cycle.insert(issue.number.to_string()) {
                    continue;
                }

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

            // Extract [Role] prefix for self-loop prevention
            let comment_role = crate::git_host::extract_comment_role(
                body_trimmed,
                Some(crate::git_host::GITHUB_COMMENT_ROLES),
            );

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
            let (title, github_type, labels, assignees) =
                issue_cache.get(&issue_number).cloned().unwrap_or_else(|| {
                    (
                        format!("#{}", issue_number),
                        "issue".to_string(),
                        vec![],
                        vec![],
                    )
                });

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

            // Add comment_role for self-loop prevention in pattern matching
            if let Some(ref role) = comment_role {
                message.metadata.insert(
                    "comment_role".to_string(),
                    serde_json::Value::String(role.clone()),
                );
            }

            tracing::debug!(
                channel = %self.channel_name,
                comment_id = comment.id,
                issue_number = issue_number,
                user = %comment.user.login,
                "Comment detected → routing for Pattern mode"
            );

            // Dedup: skip if this issue already triggered in this poll cycle
            if !triggered_in_cycle.insert(issue_number.to_string()) {
                tracing::debug!(
                    channel = %self.channel_name,
                    issue_number = issue_number,
                    "Skipping duplicate comment trigger for issue already triggered in this cycle"
                );
                self.track_comment(&comment_key, processed_comments).await;
                continue;
            }

            if let Err(e) = (options.on_message)(message) {
                tracing::error!(error = %e, number = issue_number, "Failed to route comment event");
            }

            self.track_comment(&comment_key, processed_comments).await;
        }

        // 2b. Fetch and process PR reviews and review comments.
        // Only fetch for PRs with active topics (pr-N or review-pr-N directories exist).
        // This avoids 2 API calls per open PR for PRs that don't match any pattern,
        // which would otherwise make the poll cycle take 15+ minutes with many open PRs.
        let open_pr_numbers: Vec<u64> = issue_cache
            .iter()
            .filter(|(_, (_, github_type, _, _))| github_type == "pull_request")
            .map(|(number, _)| *number)
            .collect();

        let active_pr_topics = self.scan_active_pr_topics();
        tracing::debug!(
            channel = %self.channel_name,
            active = active_pr_topics.len(),
            total = open_pr_numbers.len(),
            "Review polling: active topics out of open PRs"
        );

        for pr_number in &open_pr_numbers {
            if !active_pr_topics.contains(pr_number) {
                continue;
            }

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

                        let comment_role = crate::git_host::extract_comment_role(
                            body_trimmed,
                            Some(crate::git_host::GITHUB_COMMENT_ROLES),
                        );

                        let (title, github_type, labels, assignees) =
                            issue_cache.get(pr_number).cloned().unwrap_or_else(|| {
                                (
                                    format!("#{}", pr_number),
                                    "pull_request".to_string(),
                                    vec![],
                                    vec![],
                                )
                            });

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

                        // Dedup: skip if this PR already triggered in this poll cycle
                        if !triggered_in_cycle.insert(pr_number.to_string()) {
                            tracing::debug!(
                                channel = %self.channel_name,
                                pr_number = pr_number,
                                "Skipping duplicate review trigger for PR already triggered in this cycle"
                            );
                            self.track_comment(&review_key, processed_comments).await;
                            continue;
                        }

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

                        let comment_role = crate::git_host::extract_comment_role(
                            body_trimmed,
                            Some(crate::git_host::GITHUB_COMMENT_ROLES),
                        );

                        let (title, github_type, labels, assignees) =
                            issue_cache.get(pr_number).cloned().unwrap_or_else(|| {
                                (
                                    format!("#{}", pr_number),
                                    "pull_request".to_string(),
                                    vec![],
                                    vec![],
                                )
                            });

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
                                rc.user.login,
                                context_parts.join(", "),
                                rc.body
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

                        // Dedup: skip if this PR already triggered in this poll cycle
                        if !triggered_in_cycle.insert(pr_number.to_string()) {
                            tracing::debug!(
                                channel = %self.channel_name,
                                pr_number = pr_number,
                                "Skipping duplicate review comment trigger for PR already triggered in this cycle"
                            );
                            self.track_comment(&rc_key, processed_comments).await;
                            continue;
                        }

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
            let active_pr_topics = self.scan_active_pr_topics();
            tracing::debug!(
                channel = %self.channel_name,
                active = active_pr_topics.len(),
                total = open_pr_numbers.len(),
                "CI polling: active topics out of open PRs"
            );

            for pr_number in &open_pr_numbers {
                if !active_pr_topics.contains(pr_number) {
                    continue;
                }
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

                let has_failure = check_runs.iter().any(|cr| {
                    cr.conclusion.as_deref() == Some("failure")
                        || cr.conclusion.as_deref() == Some("timed_out")
                });
                let all_completed = check_runs.iter().all(|cr| cr.status == "completed");

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
                    let failed_checks: Vec<&crate::github::client::GithubCheckRun> = check_runs
                        .iter()
                        .filter(|cr| {
                            cr.conclusion.as_deref() == Some("failure")
                                || cr.conclusion.as_deref() == Some("timed_out")
                        })
                        .collect();

                    let (title, github_type, labels, assignees) =
                        issue_cache.get(pr_number).cloned().unwrap_or_else(|| {
                            (
                                format!("#{}", pr_number),
                                "pull_request".to_string(),
                                vec![],
                                vec![],
                            )
                        });

                    let event_uid = format!(
                        "ci-{}-{}-failure",
                        pr_number,
                        head_sha.get(..12).unwrap_or(&head_sha)
                    );

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

                    // Dedup: skip if this PR already triggered in this poll cycle
                    if !triggered_in_cycle.insert(format!("ci-{}", pr_number)) {
                        tracing::debug!(
                            channel = %self.channel_name,
                            pr_number = pr_number,
                            "Skipping duplicate CI failure trigger for PR already triggered in this cycle"
                        );
                        self.track_ci_status(
                            *pr_number,
                            &head_sha,
                            previous_status.as_deref().unwrap_or("pending"),
                            ci_status,
                        )
                        .await;
                        continue;
                    }

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
                if let Some((_title, github_type, _labels, _assignees)) =
                    issue_cache.get(&cached_number)
                {
                    let event_uid = format!("{}-{}-closed", github_type, cached_number);

                    if !processed_events.contains(&event_uid) {
                        tracing::info!(
                            channel = %self.channel_name,
                            event = "closed",
                            number = cached_number,
                            github_type = github_type,
                            "GitHub close event detected (via cache comparison) → closing topics"
                        );

                        if let Some(ref on_close) = options.on_close_event {
                            (on_close)(cached_number);
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
            let github_type = if item.is_pull_request() {
                "pull_request"
            } else {
                "issue"
            };
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
                "GitHub close event detected → closing topics"
            );

            // Close every topic the pipe adapter routed for this number.
            if let Some(ref on_close) = options.on_close_event {
                let _ = github_type; // event-type no longer drives the prefix list
                let _ = is_merged; // merge state currently doesn't change cleanup
                (on_close)(item.number);
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
/// Check whether a comment should be processed based on whether its
/// parent issue/PR is still open.
///
/// Returns `true` if the comment's issue is in the open set and should
/// be routed to agents. Returns `false` if the issue is closed (not in
/// the open set) or if the issue number could not be parsed from the URL.
pub(crate) fn should_process_comment(comment: &GithubComment, open_numbers: &HashSet<u64>) -> bool {
    let issue_number = comment.issue_number().unwrap_or(0);
    open_numbers.contains(&issue_number)
}
