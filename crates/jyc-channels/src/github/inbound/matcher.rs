//! GitHub matcher — stateless pattern matching for GitHub events.
//!
//! Extracted from the monolithic `github/inbound.rs`.

use std::collections::HashMap;

use jyc_types::{ChannelMatcher, ChannelPattern, InboundMessage, PatternMatch, PatternRules};

use super::GithubMatcher;

impl ChannelMatcher for GithubMatcher {
    fn channel_type(&self) -> &str {
        "github"
    }

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
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

        // If the matched pattern declares an explicit thread_prefix, use it.
        // This is the configurable mechanism that lets two patterns matching
        // the same GitHub identity (e.g. issue + label combinations) live in
        // separate workspace directories so each can carry its own template
        // and AGENTS.md without collision.
        if let Some(pm) = pattern_match {
            if let Some(pattern) = patterns.iter().find(|p| p.name == pm.pattern_name)
                && let Some(prefix) = pattern.thread_prefix.as_deref()
            {
                return format!("{}-{}", prefix, number);
            }

            // Backwards-compatible fallback: a pattern named "reviewer"
            // without an explicit `thread_prefix` keeps the historical
            // `review-pr-{N}` thread name. Emit a deprecation warning so
            // users migrate to declaring `thread_prefix = "review-pr"`
            // explicitly. New patterns should not rely on this.
            if pm.pattern_name == "reviewer" {
                tracing::warn!(
                    pattern = %pm.pattern_name,
                    "Pattern 'reviewer' has no `thread_prefix` configured; falling back to legacy 'review-pr-{{N}}'. \
                     Add `thread_prefix = \"review-pr\"` to the pattern config to silence this warning. \
                     The implicit fallback will be removed in a future release."
                );
                return format!("review-pr-{}", number);
            }
        }

        // Default derivation by event type.
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
        // Pattern evaluation order is normally TOML-declaration order with
        // first-match-wins semantics. We deviate from strict order in ONE
        // specific way: any pattern whose `role` is "Reviewer" is tried
        // before non-reviewer patterns.
        //
        // Rationale: in the typical PR workflow a developer pattern handles
        // the early phases (e.g. label `ready-for-dev`) and a reviewer
        // pattern handles the review phase (label `ready-for-review`). When
        // the developer hands off to the reviewer they ADD `ready-for-review`,
        // and the existing dev label often remains on the PR until cleanup.
        // With strict TOML order, a developer pattern listed before a
        // reviewer pattern wins on every poll because it still matches
        // `ready-for-dev`, and the reviewer is never tried.
        //
        // Promoting reviewer patterns to be evaluated first means a PR
        // labeled `ready-for-review` is routed to the reviewer regardless
        // of any leftover developer-phase labels — which is the semantically
        // correct outcome (the PR is ready for review; the review supersedes
        // any further developer work). Operators no longer need to reorder
        // their TOML or scrub stale labels for the reviewer to fire.
        //
        // Within the reviewer group and within the non-reviewer group,
        // relative TOML order is preserved (stable sort).
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
                tracing::debug!(
                    pattern = %pattern.name,
                    "Rules did not match, skipping"
                );
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

/// Sort key for `ChannelPattern` evaluation order in `GithubMatcher::match_message`.
///
/// Lower numbers are tried first. Currently:
/// - `0` — patterns whose `role` equals "Reviewer" (case-insensitive).
/// - `255` — every other pattern.
///
/// `Vec::sort_by_key` is stable, so patterns within the same priority bucket
/// retain their TOML-declaration order.
///
/// The bucket spread (0 vs 255) leaves room to introduce intermediate roles
/// later (e.g. CI bots) without renumbering everything.
pub(crate) fn pattern_priority(role: Option<&str>) -> u8 {
    match role {
        Some(r) if r.eq_ignore_ascii_case("Reviewer") => 0,
        _ => 255,
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
            if !allowed_types
                .iter()
                .any(|t| t.eq_ignore_ascii_case(msg_type))
            {
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
        if let Some(ref label_rule) = rules.labels
            && !label_rule.matches(&msg_labels)
        {
            return false;
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
