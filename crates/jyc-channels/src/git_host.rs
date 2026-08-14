//! Shared git-host logic (GitHub / Gitee).
//!
//! Both hosts are GitHub-compatible APIs and share these channel-agnostic
//! helpers: pattern priority, rule matching, `[Role]` prefix extraction, and
//! persistent keyed-set tracking. Host-specific differences (metadata key
//! prefixes, allowed roles) are parameterized.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use jyc_types::{InboundMessage, PatternRules};

/// Persistent `HashSet<String>` backed by an append-only text file.
///
/// Used for processed-comment / seen-issue tracking in both GitHub and
/// Gitee: `load()` reads the file, `insert()` appends new keys, `compact()`
/// truncates to the `max` most recent keys (numeric-ID prefix heuristic).
pub(crate) struct PersistentKeySet {
    path: PathBuf,
}

impl PersistentKeySet {
    pub(crate) fn new(state_dir: &Path, filename: &str) -> Self {
        Self {
            path: state_dir.join(filename),
        }
    }

    /// Load the set from disk (empty when missing or unreadable).
    pub(crate) async fn load(&self) -> HashSet<String> {
        if !self.path.exists() {
            return HashSet::new();
        }
        match tokio::fs::read_to_string(&self.path).await {
            Ok(content) => content
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.path.display(),
                    "Failed to load persistent key set"
                );
                HashSet::new()
            }
        }
    }

    /// Insert a key and append it to the file (only when newly added).
    pub(crate) async fn insert(&self, key: &str, set: &mut HashSet<String>) {
        if set.insert(key.to_string()) {
            use tokio::io::AsyncWriteExt;
            if let Ok(mut f) = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .await
            {
                let _ = f.write_all(format!("{key}\n").as_bytes()).await;
                let _ = f.flush().await;
                let _ = f.sync_all().await;
            }
        }
    }

    /// Truncate to the `max` most recent keys (by numeric ID prefix), then
    /// rewrite the file.
    pub(crate) async fn compact(&self, set: &mut HashSet<String>, max: usize) {
        if set.len() <= max {
            return;
        }

        // Keys are `<numeric-id>:<rest>`; sort by the numeric id to approximate recency.
        let mut entries: Vec<(u64, String)> = set
            .iter()
            .map(|key| {
                let id = key
                    .split(':')
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                (id, key.clone())
            })
            .collect();
        entries.sort_unstable_by_key(|(id, _)| *id);
        let keep_from = entries.len() - max;
        let keep: HashSet<String> = entries[keep_from..]
            .iter()
            .map(|(_, key)| key.clone())
            .collect();

        let before = set.len();
        *set = keep;

        let content: String = set.iter().map(|key| format!("{key}\n")).collect();
        if let Err(e) = tokio::fs::write(&self.path, content).await {
            tracing::warn!(
                error = %e,
                path = %self.path.display(),
                "Failed to compact persistent key set"
            );
        } else {
            tracing::info!(
                before = before,
                after = set.len(),
                path = %self.path.display(),
                "Compacted persistent key set"
            );
        }
    }
}

/// Reviewer patterns get priority 0; everything else 255.
pub(crate) fn pattern_priority(role: Option<&str>) -> u8 {
    match role {
        Some(r) if r.eq_ignore_ascii_case("Reviewer") => 0,
        _ => 255,
    }
}

/// Check whether the host-specific rules (type, labels, assignees) all match.
///
/// `key_prefix` is the metadata key prefix used by the host (`"github_"` or
/// `"gitee_"`). All present rules use AND logic; within each rule, OR logic
/// applies (any value in the list suffices). Rules that are `None` are
/// considered matched (no constraint).
pub(crate) fn rules_match(
    rules: &PatternRules,
    message: &InboundMessage,
    key_prefix: &str,
) -> bool {
    let type_key = format!("{key_prefix}type");
    let labels_key = format!("{key_prefix}labels");
    let assignees_key = format!("{key_prefix}assignees");

    // Check type rule
    if let Some(ref allowed_types) = rules.github_type {
        let msg_type = message
            .metadata
            .get(&type_key)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !allowed_types
            .iter()
            .any(|t| t.eq_ignore_ascii_case(msg_type))
        {
            return false;
        }
    }

    // Labels, used for both the labels rule and exclude_labels
    let msg_labels: Vec<String> = message
        .metadata
        .get(&labels_key)
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

    // Assignees rule (OR logic: any assignee on the issue/PR in the rule list)
    if let Some(ref allowed_assignees) = rules.assignees {
        let msg_assignees: Vec<String> = message
            .metadata
            .get(&assignees_key)
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

/// Roles recognized by the GitHub channel's self-loop prevention.
pub(crate) const GITHUB_COMMENT_ROLES: &[&str] =
    &["Planner", "Developer", "Reviewer", "High-Level Planner"];

/// Build the comment body for a git-host reply: strip trailing separators,
/// append the footer, and prefix `[Role]` (skipping the prefix when the AI
/// already added it or when no role is set).
pub(crate) fn build_comment_body(reply_text: &str, role: &str, footer: &str) -> String {
    let clean_reply = jyc_core::email_parser::strip_trailing_separators(reply_text);
    let with_footer = if footer.is_empty() {
        clean_reply
    } else {
        format!("{clean_reply}\n\n{footer}")
    };
    if role.is_empty() || with_footer.trim_start().starts_with(&format!("[{role}]")) {
        with_footer
    } else {
        format!("[{role}] {with_footer}")
    }
}

/// Extract the `[Role]` prefix from a comment body for self-loop prevention.
///
/// Loose `[Role]` match (no trailing-space requirement). When `allowed` is
/// `Some`, only the listed roles are returned; when `None`, any role text is
/// accepted (Gitee behavior).
pub(crate) fn extract_comment_role(text: &str, allowed: Option<&[&str]>) -> Option<String> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('[')
        && let Some(end) = trimmed.find(']')
    {
        let role = &trimmed[1..end];
        if let Some(allow) = allowed {
            if allow.contains(&role) {
                return Some(role.to_string());
            }
        } else {
            return Some(role.to_string());
        }
    }
    None
}
