//! Shared git-host logic (GitHub / Gitee).
//!
//! Both hosts are GitHub-compatible APIs and share these channel-agnostic
//! helpers: pattern priority, rule matching, and `[Role]` prefix extraction.
//! Host-specific differences (metadata key prefixes, allowed roles) are
//! parameterized.

use jyc_types::{InboundMessage, PatternRules};

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
