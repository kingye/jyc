//! `GithubInboundAdapter` state — persistent tracking and trigger building.
//!
//! Extracted from the monolithic `github/inbound.rs`.

use arc_swap::ArcSwap;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use jyc_types::GithubConfig;
use jyc_types::{ChannelPattern, InboundMessage, MessageContent};

use super::GithubInboundAdapter;

impl GithubInboundAdapter {
    pub fn new(
        config: &GithubConfig,
        channel_name: String,
        workdir: &Path,
        app_config: Option<Arc<ArcSwap<jyc_types::AppConfig>>>,
    ) -> Self {
        // Channel state lives under `channels/<channel>/` (migration PR-4);
        // lazily migrate the legacy `<channel>/.github` dir.
        let state_dir =
            jyc_core::topic_path::resolve_channel_state_dir(workdir, &channel_name).join(".github");
        jyc_core::topic_path::migrate_dir_if_needed(
            &workdir.join(&channel_name).join(".github"),
            &state_dir,
        );
        Self {
            config: config.clone(),
            channel_name,
            state_dir,
            workdir: workdir.to_path_buf(),
            app_config,
            test_patterns: None,
        }
    }

    /// Set test patterns (for unit tests only).
    #[cfg(test)]
    pub fn with_patterns(mut self, patterns: Vec<ChannelPattern>) -> Self {
        self.test_patterns = Some(patterns);
        self
    }

    /// Read the current patterns for this channel.
    fn patterns(&self) -> Vec<ChannelPattern> {
        if let Some(ref patterns) = self.test_patterns {
            return patterns.clone();
        }
        match &self.app_config {
            Some(cfg) => {
                let cfg = cfg.load();
                cfg.channels
                    .get(&self.channel_name)
                    .and_then(|c| c.patterns.clone())
                    .unwrap_or_default()
            }
            None => Vec::new(),
        }
    }

    /// Load processed comment keys from persistent storage.
    /// File format: one key per line (`{comment_id}:{updated_at}`).
    /// Using `id:updated_at` ensures edited comments are re-processed.
    pub(crate) async fn load_processed_comments(&self) -> HashSet<String> {
        crate::git_host::PersistentKeySet::new(&self.state_dir, "processed-comments.txt")
            .load()
            .await
    }

    /// Persist a comment key as processed (append to file).
    /// Key format: `{comment_id}:{updated_at}`
    pub(crate) async fn track_comment(&self, key: &str, processed: &mut HashSet<String>) {
        crate::git_host::PersistentKeySet::new(&self.state_dir, "processed-comments.txt")
            .insert(key, processed)
            .await;
        if processed.len() > 5000 {
            self.compact_processed_comments(processed).await;
        }
    }

    /// Compact processed comments file by keeping only the latest entries.
    pub(crate) async fn compact_processed_comments(&self, processed: &mut HashSet<String>) {
        crate::git_host::PersistentKeySet::new(&self.state_dir, "processed-comments.txt")
            .compact(processed, 2000)
            .await;
    }

    /// Load seen issues from persistent storage.
    /// File format: one line per issue (`{number}:{labels}:{updated_at}`).
    pub(crate) async fn load_seen_issues(&self) -> HashSet<String> {
        crate::git_host::PersistentKeySet::new(&self.state_dir, "seen-issues.txt")
            .load()
            .await
    }

    /// Track a seen issue (append to file).
    /// Key format: `{number}:{labels}:{updated_at}`
    pub(crate) async fn track_seen_issue(&self, key: &str, seen: &mut HashSet<String>) {
        crate::git_host::PersistentKeySet::new(&self.state_dir, "seen-issues.txt")
            .insert(key, seen)
            .await;
        if seen.len() > 5000 {
            self.compact_seen_issues(seen).await;
        }
    }

    /// Compact seen issues file by keeping only the latest entries.
    pub(crate) async fn compact_seen_issues(&self, seen: &mut HashSet<String>) {
        crate::git_host::PersistentKeySet::new(&self.state_dir, "seen-issues.txt")
            .compact(seen, 2000)
            .await;
    }

    /// Load CI status tracking from persistent storage.
    /// File format: `{pr_number}:{head_sha}:{overall_status}` per line.
    /// Returns map of pr_number → (head_sha, overall_status).
    pub(crate) async fn load_ci_status(&self) -> HashMap<u64, (String, String)> {
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
    pub(crate) async fn track_ci_status(
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
    pub(crate) async fn compact_ci_status(&self, tracked: &mut HashMap<u64, (String, String)>) {
        let file = self.state_dir.join("ci-status.txt");
        let content: String = tracked
            .iter()
            .map(|(number, (sha, status))| format!("{}:{}:{}\n", number, sha, status))
            .collect();
        if let Err(e) = tokio::fs::write(&file, content).await {
            tracing::warn!(error = %e, "Failed to compact CI status file");
        }
    }

    /// Compute the set of topic-name prefixes that correspond to PR-typed
    /// patterns in the configuration. Always includes the default `pr` prefix
    /// (used by patterns without an explicit `topic_prefix`). Patterns whose
    /// `rules.github_type` lists `pull_request` and that declare an explicit
    /// `topic_prefix` contribute that prefix.
    ///
    /// As a backwards-compatible fallback, if any PR-typed pattern is named
    /// `"reviewer"` and does NOT declare an explicit `topic_prefix`, the
    /// legacy `review-pr` prefix is added so disk scans continue to recognize
    /// topic directories produced by the implicit fallback in
    /// `derive_topic_name`. Mirror this with the matching `derive_topic_name`
    /// branch.
    ///
    /// Examples for a config with `developer` (default) and
    /// `reviewer` (`topic_prefix = "review-pr"`): `{"pr", "review-pr"}`.
    fn pr_topic_prefixes(&self) -> HashSet<String> {
        let mut prefixes: HashSet<String> = HashSet::new();
        prefixes.insert("pr".to_string());

        for pattern in self.patterns() {
            // Only consider patterns that can match pull_request events.
            let matches_pr = match pattern.rules.github_type.as_ref() {
                Some(types) => types.iter().any(|t| t == "pull_request"),
                None => false,
            };
            if !matches_pr {
                continue;
            }
            match pattern.topic_prefix.as_deref() {
                Some(prefix) if !prefix.is_empty() => {
                    prefixes.insert(prefix.to_string());
                }
                _ => {
                    // Legacy fallback for the unconfigured "reviewer" pattern.
                    if pattern.name == "reviewer" {
                        prefixes.insert("review-pr".to_string());
                    }
                }
            }
        }
        prefixes
    }

    /// Scan workspace directory for active PR topic directories.
    ///
    /// Returns a set of PR numbers that have an active topic directory.
    /// A directory is recognized as a PR topic when its name has the form
    /// `{prefix}-{N}` where `{prefix}` is one of the configured PR topic
    /// prefixes (see `pr_topic_prefixes`) and `{N}` is a valid `u64`.
    /// Returns an empty set if the workspace directory does not exist.
    pub(crate) fn scan_active_pr_topics(&self) -> HashSet<u64> {
        let workspace = jyc_core::topic_path::resolve_workspace(&self.workdir, &self.channel_name);

        let Ok(entries) = std::fs::read_dir(&workspace) else {
            return HashSet::new();
        };

        // Sort prefixes longest-first so that for a name like `review-pr-43`
        // we match `review-pr-` before the shorter `pr-`.
        let mut prefixes: Vec<String> = self.pr_topic_prefixes().into_iter().collect();
        prefixes.sort_by_key(|p| std::cmp::Reverse(p.len()));

        let mut pr_numbers = HashSet::new();
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            for prefix in &prefixes {
                let with_dash = format!("{}-", prefix);
                if let Some(suffix) = name.strip_prefix(with_dash.as_str()) {
                    if let Ok(num) = suffix.parse::<u64>() {
                        pr_numbers.insert(num);
                    }
                    break;
                }
            }
        }
        pr_numbers
    }

    /// Enumerate all topic directories in the workspace whose name has the
    /// strict form `{anything}-{N}` for the given GitHub number.
    ///
    /// Used on issue/PR close to close every topic that is associated with
    /// that GitHub identity, regardless of which `topic_prefix` patterns
    /// happen to be configured. The match is strict: the directory name must
    /// end with `-{N}` and the trailing `{N}` must parse cleanly to the same
    /// `u64`. This avoids false matches like `feature-plan-43-extra`.
    ///
    /// Returns an empty Vec if the workspace directory does not exist.
    pub(crate) fn scan_topics_for_number(&self, number: u64) -> Vec<String> {
        let workspace = jyc_core::topic_path::resolve_workspace(&self.workdir, &self.channel_name);

        let Ok(entries) = std::fs::read_dir(&workspace) else {
            return Vec::new();
        };

        let suffix = format!("-{}", number);
        let mut matches = Vec::new();
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy().to_string();
            // Require strict suffix `-{N}` AND a non-empty prefix before it.
            if let Some(prefix) = name.strip_suffix(&suffix)
                && !prefix.is_empty()
            {
                matches.push(name);
            }
        }
        matches
    }

    /// Build a minimal InboundMessage from a GitHub event.
    /// Contains only trigger metadata — agent uses `gh` CLI for actual content.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_trigger_message(
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

        // For enterprise GitHub (non-default api_url), prefix commands with GH_HOST
        // so `gh` targets the correct host (e.g., github.tools.sap instead of github.com).
        let gh_host_prefix = if self.config.api_url != "https://api.github.com" {
            self.config
                .api_url
                .strip_prefix("https://")
                .and_then(|s| s.split('/').next())
                .map(|host| format!("GH_HOST={} ", host))
                .unwrap_or_default()
        } else {
            String::new()
        };

        let gh_cmd = match github_type {
            "pull_request" => format!(
                "Repository: {}/{}\n\nSetup:\n  cd repo  # or: {gh}gh repo clone {}/{} repo && cd repo\n\nRead PR:\n  {gh}gh pr view {}\n  {gh}gh pr view {} --comments\n  {gh}gh pr diff {}",
                self.config.owner,
                self.config.repo,
                self.config.owner,
                self.config.repo,
                number,
                number,
                number,
                gh = gh_host_prefix
            ),
            _ => format!(
                "Repository: {}/{}\n\nSetup:\n  cd repo  # or: {gh}gh repo clone {}/{} repo && cd repo\n\nRead issue:\n  {gh}gh issue view {}\n  {gh}gh issue view {} --comments",
                self.config.owner,
                self.config.repo,
                self.config.owner,
                self.config.repo,
                number,
                number,
                gh = gh_host_prefix
            ),
        };

        let body = format!(
            "github event: {}\nrepository: {}/{}\nnumber: {}\ntype: {}\naction: {}\nactor: {}\n{}{}{}",
            event_type,
            self.config.owner,
            self.config.repo,
            number,
            github_type,
            action,
            actor,
            label_str,
            assignee_str,
            gh_cmd
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
            references: None,
            reply_to_id: None,
            external_id: Some(event_uid.to_string()),
            attachments: vec![],
            metadata,
            matched_pattern: None,
        }
    }
}
