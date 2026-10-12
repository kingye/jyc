//! `GithubInboundAdapter` state — persistent tracking and trigger building.
//!
//! Extracted from the monolithic `github/inbound.rs`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use jyc_types::GithubConfig;
use jyc_types::{InboundMessage, MessageContent};

use super::GithubInboundAdapter;

impl GithubInboundAdapter {
    /// Build the adapter for `channel_name`, with its state in
    /// `<data dir>/channels/<channel>/.github/`.
    pub fn new(config: &GithubConfig, channel_name: String, data_dir: &Path) -> Self {
        let state_dir = jyc_utils::paths::channels_dir(data_dir)
            .join(&channel_name)
            .join(".github");
        Self {
            config: config.clone(),
            channel_name,
            state_dir,
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
        // Repo name for pipe topic templates that need cross-repo disambiguation
        // (`${msg.repo}`), e.g. two channels piping into the same agent topic space.
        metadata.insert("repo".to_string(), serde_json::json!(self.config.repo));
        // Type-gated number aliases for pipe topic templates
        // (`${msg.pr_number}` / `${msg.issue_number}`): a PR event carries
        // only `pr_number`, an issue event only `issue_number`, so a
        // misrouted event fails placeholder resolution loudly instead of
        // landing in the wrong topic.
        match github_type {
            "pull_request" => {
                metadata.insert("pr_number".to_string(), serde_json::json!(number));
            }
            _ => {
                metadata.insert("issue_number".to_string(), serde_json::json!(number));
            }
        }
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
