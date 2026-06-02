use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

use super::client::GiteeClient;
use jyc_types::GiteeConfig;

/// Type alias for issue cache: number → (title, type, labels, assignees)
type IssueCache = HashMap<u64, (String, String, Vec<String>, Vec<String>)>;
use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundAdapter, InboundAdapterOptions, InboundMessage,
    MessageContent, PatternMatch, PatternRules,
};

/// Gitee channel matcher — stateless pattern matching for Gitee events.
pub struct GiteeMatcher;

impl ChannelMatcher for GiteeMatcher {
    fn channel_type(&self) -> &str {
        "gitee"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        let gitee_type = message
            .metadata
            .get("gitee_type")
            .and_then(|v| v.as_str())
            .unwrap_or("issue");
        let number = message
            .metadata
            .get("gitee_number")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        if let Some(pm) = pattern_match
            && let Some(pattern) = patterns.iter().find(|p| p.name == pm.pattern_name)
            && let Some(prefix) = pattern.thread_prefix.as_deref()
        {
            return format!("{}-{}", prefix, number);
        }

        match gitee_type {
            "pull_request" => format!("pr-{}", number),
            _ => format!("issue-{}", number),
        }
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        let mut ordered: Vec<&ChannelPattern> = patterns.iter().collect();
        ordered.sort_by_key(|p| pattern_priority(p.role.as_deref()));

        for pattern in ordered {
            if !pattern.enabled {
                continue;
            }

            let Some(ref pattern_role) = pattern.role else {
                continue;
            };

            if !self.rules_match(&pattern.rules, message) {
                continue;
            }

            if let Some(comment_role) = message
                .metadata
                .get("comment_role")
                .and_then(|v| v.as_str())
                && pattern_role.eq_ignore_ascii_case(comment_role)
            {
                continue;
            }

            return Some(PatternMatch {
                pattern_name: pattern.name.clone(),
                channel: "gitee".to_string(),
                matches: HashMap::new(),
            });
        }

        None
    }

    fn store_unmatched_messages(&self) -> bool {
        false
    }
}

fn pattern_priority(role: Option<&str>) -> u8 {
    match role {
        Some(r) if r.eq_ignore_ascii_case("Reviewer") => 0,
        _ => 255,
    }
}

impl GiteeMatcher {
    fn rules_match(&self, rules: &PatternRules, message: &InboundMessage) -> bool {
        if let Some(ref allowed_types) = rules.github_type {
            let msg_type = message
                .metadata
                .get("gitee_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !allowed_types
                .iter()
                .any(|t| t.eq_ignore_ascii_case(msg_type))
            {
                return false;
            }
        }

        let msg_labels: Vec<String> = message
            .metadata
            .get("gitee_labels")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();

        if let Some(ref label_rule) = rules.labels
            && !label_rule.matches(&msg_labels)
        {
            return false;
        }

        if let Some(ref exclude_labels) = rules.exclude_labels {
            let has_excluded = exclude_labels
                .iter()
                .any(|l| msg_labels.contains(&l.to_lowercase()));
            if has_excluded {
                return false;
            }
        }

        if let Some(ref allowed_assignees) = rules.assignees {
            let msg_assignees: Vec<String> = message
                .metadata
                .get("gitee_assignees")
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

/// Extract [Role] prefix from comment body for self-loop prevention.
fn extract_comment_role(body: &str) -> Option<String> {
    let trimmed = body.trim_start();
    if trimmed.starts_with('[')
        && let Some(end) = trimmed.find("] ")
    {
        return Some(trimmed[1..end].to_string());
    }
    None
}

/// Gitee inbound adapter — polls Gitee API for events.
pub struct GiteeInboundAdapter {
    config: GiteeConfig,
    channel_name: String,
    state_dir: PathBuf,
    workdir: PathBuf,
    patterns: Vec<ChannelPattern>,
}

impl GiteeInboundAdapter {
    pub fn new(config: &GiteeConfig, channel_name: String, workdir: &Path) -> Self {
        let state_dir = workdir.join(&channel_name).join(".gitee");
        Self {
            config: config.clone(),
            channel_name,
            state_dir,
            workdir: workdir.to_path_buf(),
            patterns: Vec::new(),
        }
    }

    pub fn with_patterns(mut self, patterns: Vec<ChannelPattern>) -> Self {
        self.patterns = patterns;
        self
    }

    async fn load_processed_comments(&self) -> HashSet<String> {
        let file = self.state_dir.join("processed-comments.txt");
        if !file.exists() {
            return HashSet::new();
        }
        match tokio::fs::read_to_string(&file).await {
            Ok(content) => content
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load processed comments");
                HashSet::new()
            }
        }
    }

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
            let _ = f.write_all(format!("{}\n", key).as_bytes()).await;
        }
    }

    async fn load_seen_issues(&self) -> HashSet<String> {
        let file = self.state_dir.join("seen-issues.txt");
        if !file.exists() {
            return HashSet::new();
        }
        match tokio::fs::read_to_string(&file).await {
            Ok(content) => content
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load seen issues");
                HashSet::new()
            }
        }
    }

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
                let _ = f.write_all(format!("{}\n", key).as_bytes()).await;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_trigger_message(
        &self,
        event_type: &str,
        number: u64,
        title: &str,
        gitee_type: &str,
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

        let repo_cmd = match gitee_type {
            "pull_request" => format!(
                "Repository: {}/{}\n\nRead PR:\n  curl -s \"{}/repos/{}/{}/pulls/{}\"\n",
                self.config.owner,
                self.config.repo,
                self.config.api_url,
                self.config.owner,
                self.config.repo,
                number
            ),
            _ => format!(
                "Repository: {}/{}\n\nRead Issue:\n  curl -s \"{}/repos/{}/{}/issues/{}\"\n",
                self.config.owner,
                self.config.repo,
                self.config.api_url,
                self.config.owner,
                self.config.repo,
                number
            ),
        };

        let body = format!(
            "gitee event: {}\nrepository: {}/{}\nnumber: {}\ntype: {}\naction: {}\nactor: {}\n{}{}{}",
            event_type,
            self.config.owner,
            self.config.repo,
            number,
            gitee_type,
            action,
            actor,
            label_str,
            assignee_str,
            repo_cmd
        );

        let mut metadata = HashMap::new();
        metadata.insert("gitee_event".to_string(), serde_json::json!(event_type));
        metadata.insert("gitee_number".to_string(), serde_json::json!(number));
        metadata.insert("gitee_type".to_string(), serde_json::json!(gitee_type));
        metadata.insert("gitee_action".to_string(), serde_json::json!(action));
        metadata.insert("gitee_labels".to_string(), serde_json::json!(labels));
        metadata.insert("gitee_assignees".to_string(), serde_json::json!(assignees));

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
impl ChannelMatcher for GiteeInboundAdapter {
    fn channel_type(&self) -> &str {
        "gitee"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String {
        GiteeMatcher.derive_thread_name(message, patterns, pattern_match)
    }

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch> {
        GiteeMatcher.match_message(message, patterns)
    }
}

#[async_trait]
impl InboundAdapter for GiteeInboundAdapter {
    async fn start(&self, options: InboundAdapterOptions, cancel: CancellationToken) -> Result<()> {
        let client = GiteeClient::new(&self.config).context("Failed to create Gitee client")?;

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
            "Gitee inbound adapter started"
        );

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
        let mut processed_events: HashSet<String> = HashSet::new();
        let mut seen_issues: HashSet<String> = self.load_seen_issues().await;
        let mut issue_cache: HashMap<u64, (String, String, Vec<String>, Vec<String>)> =
            HashMap::new();

        let mut last_poll = if is_fresh_start {
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
        } else {
            (chrono::Utc::now() - chrono::Duration::minutes(5))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        };

        let poll_interval = tokio::time::Duration::from_secs(self.config.poll_interval_secs);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(channel = %self.channel_name, "Gitee polling cancelled");
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
                    ).await {
                        tracing::error!(channel = %self.channel_name, error = %e, "Gitee poll cycle failed");
                        (options.on_error)(e);
                    }
                }
            }
        }

        tracing::info!(channel = %self.channel_name, "Gitee inbound adapter stopped");
        Ok(())
    }
}

impl GiteeInboundAdapter {
    #[allow(clippy::too_many_arguments)]
    async fn poll_once(
        &self,
        client: &GiteeClient,
        options: &InboundAdapterOptions,
        processed_comments: &mut HashSet<String>,
        processed_events: &mut HashSet<String>,
        seen_issues: &mut HashSet<String>,
        issue_cache: &mut IssueCache,
        last_poll: &mut String,
    ) -> Result<()> {
        let poll_start = last_poll.clone();
        let mut triggered_in_cycle: HashSet<String> = HashSet::new();

        // 1. Fetch ALL open issues/PRs
        let issues = client.list_all_open_issues().await?;

        for issue in &issues {
            let gitee_type = if issue.is_pull_request() {
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
                    gitee_type.to_string(),
                    labels.clone(),
                    assignees.clone(),
                ),
            );

            let mut labels_sorted: Vec<String> = labels.clone();
            labels_sorted.sort();
            let seen_key = format!("{}:{}", issue.number, labels_sorted.join(","));
            let is_new = !seen_issues.contains(&seen_key);
            self.track_seen_issue(&seen_key, seen_issues).await;

            if is_new {
                if !triggered_in_cycle.insert(issue.number.to_string()) {
                    continue;
                }

                let event_uid = format!("{}-{}-opened", gitee_type, issue.number);
                let message = self.build_trigger_message(
                    "issues",
                    issue.number,
                    &issue.title,
                    gitee_type,
                    "opened",
                    &issue.user.login,
                    &labels,
                    &assignees,
                    &event_uid,
                );

                if let Err(e) = (options.on_message)(message) {
                    tracing::error!(error = %e, number = issue.number, "Failed to route issue event");
                }
            }
        }

        let current_open_numbers: HashSet<u64> = issues.iter().map(|i| i.number).collect();

        // 2. Fetch and process comments
        let comments = client.list_comments_since(&poll_start).await?;

        for comment in &comments {
            let comment_key = format!("{}:{}", comment.id, comment.updated_at);

            let id_only = comment.id.to_string();
            if processed_comments.contains(&comment_key) || processed_comments.contains(&id_only) {
                continue;
            }

            let body_trimmed = comment.body.trim();
            let comment_role = extract_comment_role(body_trimmed);
            let issue_number = comment.issue_number().unwrap_or(0);

            if !current_open_numbers.contains(&issue_number) {
                self.track_comment(&comment_key, processed_comments).await;
                continue;
            }

            let (title, gitee_type, labels, assignees) =
                issue_cache.get(&issue_number).cloned().unwrap_or_else(|| {
                    (
                        format!("#{}", issue_number),
                        "issue".to_string(),
                        vec![],
                        vec![],
                    )
                });

            let event_uid = format!("comment-{}", comment.id);
            let mut message = self.build_trigger_message(
                "issue_comment",
                issue_number,
                &title,
                &gitee_type,
                "mentioned",
                &comment.user.login,
                &labels,
                &assignees,
                &event_uid,
            );

            message.metadata.insert(
                "comment_body".to_string(),
                serde_json::Value::String(comment.body.clone()),
            );

            let comment_section = format!(
                "\n\n---\nTriggering comment by {}:\n\n{}",
                comment.user.login, comment.body
            );
            match &mut message.content.text {
                Some(text) => text.push_str(&comment_section),
                None => message.content.text = Some(comment_section),
            }

            if let Some(ref role) = comment_role {
                message.metadata.insert(
                    "comment_role".to_string(),
                    serde_json::Value::String(role.clone()),
                );
            }

            if !triggered_in_cycle.insert(issue_number.to_string()) {
                self.track_comment(&comment_key, processed_comments).await;
                continue;
            }

            if let Err(e) = (options.on_message)(message) {
                tracing::error!(error = %e, number = issue_number, "Failed to route comment event");
            }

            self.track_comment(&comment_key, processed_comments).await;
        }

        // 3. Detect closed issues/PRs
        let cached_numbers: Vec<u64> = issue_cache.keys().cloned().collect();
        for cached_number in cached_numbers {
            if !current_open_numbers.contains(&cached_number) {
                if let Some((_title, gitee_type, _labels, _assignees)) =
                    issue_cache.get(&cached_number)
                {
                    let event_uid = format!("{}-{}-closed", gitee_type, cached_number);

                    if !processed_events.contains(&event_uid) {
                        tracing::info!(
                            channel = %self.channel_name,
                            event = "closed",
                            number = cached_number,
                            gitee_type = gitee_type,
                            "Gitee close event detected"
                        );

                        if let Some(ref on_close) = options.on_thread_close
                            && let Ok(entries) =
                                std::fs::read_dir(jyc_core::thread_path::resolve_workspace(
                                    &self.workdir,
                                    &self.channel_name,
                                ))
                        {
                            let suffix = format!("-{}", cached_number);
                            for entry in entries.flatten() {
                                let name = entry.file_name().to_string_lossy().to_string();
                                if name.ends_with(&suffix) && name.len() > suffix.len() {
                                    let _ = (on_close)(name);
                                }
                            }
                        }

                        processed_events.insert(event_uid);
                    }
                }
                issue_cache.remove(&cached_number);
            }
        }

        // Update last_poll for next cycle
        *last_poll = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        Ok(())
    }
}
