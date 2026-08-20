use crate::github::inbound::*;
use jyc_types::{
    ChannelMatcher, ChannelPattern, InboundMessage, LabelRule, MessageContent, PatternMatch,
    PatternRules,
};
use std::collections::{HashMap, HashSet};

pub(crate) fn make_message(github_type: &str, number: u64) -> InboundMessage {
    let mut metadata = HashMap::new();
    metadata.insert(
        "github_type".to_string(),
        serde_json::Value::String(github_type.to_string()),
    );
    metadata.insert("github_number".to_string(), serde_json::json!(number));

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
        references: None,
        reply_to_id: None,
        external_id: None,
        attachments: vec![],
        metadata,
        matched_pattern: None,
    }
}

pub(crate) fn make_patterns() -> Vec<ChannelPattern> {
    vec![
        ChannelPattern {
            name: "planner".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: jyc_types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        },
        ChannelPattern {
            name: "developer".to_string(),
            enabled: true,
            role: Some("Developer".to_string()),
            rules: jyc_types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        },
        ChannelPattern {
            name: "reviewer".to_string(),
            enabled: true,
            role: Some("Reviewer".to_string()),
            rules: jyc_types::PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        },
    ]
}

// --- Topic name derivation ---

#[test]
fn test_derive_topic_name_issue() {
    let msg = make_message("issue", 42);
    let name = GithubMatcher.derive_topic_name(&msg, &[], None);
    assert_eq!(name, "issue-42");
}

#[test]
fn test_derive_topic_name_pr() {
    let msg = make_message("pull_request", 43);
    let name = GithubMatcher.derive_topic_name(&msg, &[], None);
    assert_eq!(name, "pr-43");
}

#[test]
fn test_derive_topic_name_reviewer() {
    // The reviewer pattern no longer has a hardcoded special case; it
    // must declare `topic_prefix = "review-pr"` in config to keep the
    // historical topic name. With the prefix configured, the resolver
    // returns `review-pr-{N}`.
    let msg = make_message("pull_request", 43);
    let patterns = vec![ChannelPattern {
        name: "reviewer".to_string(),
        template: Some("github-reviewer".to_string()),
        topic_prefix: Some("review-pr".to_string()),
        rules: PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    }];
    let pm = PatternMatch {
        pattern_name: "reviewer".to_string(),
        channel: "github".to_string(),
        matches: HashMap::new(),
    };
    let name = GithubMatcher.derive_topic_name(&msg, &patterns, Some(&pm));
    assert_eq!(name, "review-pr-43");
}

#[test]
fn test_derive_topic_name_reviewer_legacy_fallback() {
    // Backwards-compat: a pattern literally named "reviewer" without an
    // explicit `topic_prefix` still routes to `review-pr-{N}` so existing
    // deployments don't break. A deprecation warning is logged at runtime;
    // users should migrate to `topic_prefix = "review-pr"` explicitly.
    let msg = make_message("pull_request", 43);
    let patterns = vec![ChannelPattern {
        name: "reviewer".to_string(),
        template: Some("github-reviewer".to_string()),
        // No topic_prefix → legacy fallback kicks in.
        rules: PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    }];
    let pm = PatternMatch {
        pattern_name: "reviewer".to_string(),
        channel: "github".to_string(),
        matches: HashMap::new(),
    };
    let name = GithubMatcher.derive_topic_name(&msg, &patterns, Some(&pm));
    assert_eq!(name, "review-pr-43");
}

#[test]
fn test_derive_topic_name_non_reviewer_no_prefix_falls_back_to_default() {
    // The legacy fallback is scoped to the literal pattern name "reviewer".
    // Any other pattern name without `topic_prefix` falls through to the
    // event-type default.
    let msg = make_message("pull_request", 43);
    let patterns = vec![ChannelPattern {
        name: "qa".to_string(),
        template: Some("github-qa".to_string()),
        rules: PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    }];
    let pm = PatternMatch {
        pattern_name: "qa".to_string(),
        channel: "github".to_string(),
        matches: HashMap::new(),
    };
    let name = GithubMatcher.derive_topic_name(&msg, &patterns, Some(&pm));
    assert_eq!(name, "pr-43");
}

#[test]
fn test_derive_topic_name_high_level_planner_prefix() {
    // Two issue patterns, distinguished by labels, must declare distinct
    // `topic_prefix` values to avoid sharing a workspace dir.
    let msg = make_message("issue", 42);
    let patterns = vec![
        ChannelPattern {
            name: "high-level-planner".to_string(),
            template: Some("github-high-level-planner".to_string()),
            topic_prefix: Some("plan".to_string()),
            rules: PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                labels: Some(LabelRule::Flat(vec!["feature-plan".to_string()])),
                ..Default::default()
            },
            ..Default::default()
        },
        ChannelPattern {
            name: "detail-planner".to_string(),
            template: Some("github-planner".to_string()),
            rules: PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                exclude_labels: Some(vec!["feature-plan".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        },
    ];
    let pm_hl = PatternMatch {
        pattern_name: "high-level-planner".to_string(),
        channel: "github".to_string(),
        matches: HashMap::new(),
    };
    assert_eq!(
        GithubMatcher.derive_topic_name(&msg, &patterns, Some(&pm_hl)),
        "plan-42"
    );

    // Same issue + detail-planner pattern → default `issue-{N}`.
    let pm_detail = PatternMatch {
        pattern_name: "detail-planner".to_string(),
        channel: "github".to_string(),
        matches: HashMap::new(),
    };
    assert_eq!(
        GithubMatcher.derive_topic_name(&msg, &patterns, Some(&pm_detail)),
        "issue-42"
    );
}

#[test]
fn test_derive_topic_name_developer() {
    let msg = make_message("pull_request", 43);
    let pm = PatternMatch {
        pattern_name: "developer".to_string(),
        channel: "github".to_string(),
        matches: HashMap::new(),
    };
    let name = GithubMatcher.derive_topic_name(&msg, &[], Some(&pm));
    assert_eq!(name, "pr-43");
}

// --- Rule filtering (github_type, labels, assignees) ---

/// Helper: create a message with labels and assignees metadata
pub(crate) fn make_message_with_rules(
    github_type: &str,
    number: u64,
    labels: &[&str],
    assignees: &[&str],
) -> InboundMessage {
    let mut msg = make_message(github_type, number);
    msg.metadata
        .insert("github_labels".to_string(), serde_json::json!(labels));
    msg.metadata
        .insert("github_assignees".to_string(), serde_json::json!(assignees));
    msg
}

#[test]
fn test_github_type_rule_blocks_wrong_type() {
    let msg = make_message("issue", 42);
    let patterns = vec![ChannelPattern {
        name: "developer".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_none(),
        "developer pattern should not match issue type"
    );
}

#[test]
fn test_github_type_rule_allows_correct_type() {
    let msg = make_message("issue", 42);
    let patterns = make_patterns();
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(result.is_some());
    assert_eq!(result.unwrap().pattern_name, "planner");
}

/// Regression for the May 26 PR #204 bug: with `developer` listed before
/// `reviewer` in the TOML config, a PR carrying BOTH `ready-for-dev` and
/// `ready-for-review` (the developer hadn't stripped the dev label on
/// handoff) routed to the developer pattern on every poll because of
/// strict TOML-order first-match-wins. The reviewer pattern was never
/// tried.
///
/// Fix: `pattern_priority` promotes any pattern whose role is
/// "Reviewer" ahead of non-reviewer patterns. With this in place a PR
/// labeled `ready-for-review` is routed to the reviewer regardless of
/// any leftover developer-phase labels.
#[test]
fn test_reviewer_pattern_wins_when_both_dev_and_review_labels_present() {
    // Reproduce jyc_repo's exact pattern config: developer FIRST, reviewer SECOND.
    let developer = ChannelPattern {
        name: "developer".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            labels: Some(jyc_types::LabelRule::Nested(vec![vec![
                "bug".to_string(),
                "enhancement".to_string(),
                "documentation".to_string(),
            ]])),
            assignees: Some(vec!["kingye".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    };
    let reviewer = ChannelPattern {
        name: "reviewer".to_string(),
        enabled: true,
        role: Some("Reviewer".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            labels: Some(jyc_types::LabelRule::Flat(vec![
                "ready-for-review".to_string(),
            ])),
            assignees: Some(vec!["kingye".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    };
    let patterns = vec![developer, reviewer];

    // PR #204's actual label set on poll after the developer added
    // `ready-for-review` without removing `ready-for-dev`.
    let msg = make_message_with_rules(
        "pull_request",
        204,
        &["enhancement", "ready-for-dev", "ready-for-review"],
        &["kingye"],
    );

    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(result.is_some(), "expected a pattern match");
    assert_eq!(
        result.unwrap().pattern_name,
        "reviewer",
        "reviewer must win over developer when both could match"
    );
}

/// Negative case for the priority change: a PR with only the dev label
/// (no `ready-for-review`) must still route to the developer. The
/// reviewer-priority bump must not poach developer-phase PRs.
#[test]
fn test_developer_pattern_still_wins_when_no_review_label() {
    let developer = ChannelPattern {
        name: "developer".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            labels: Some(jyc_types::LabelRule::Nested(vec![vec![
                "bug".to_string(),
                "enhancement".to_string(),
            ]])),
            assignees: Some(vec!["kingye".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    };
    let reviewer = ChannelPattern {
        name: "reviewer".to_string(),
        enabled: true,
        role: Some("Reviewer".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            labels: Some(jyc_types::LabelRule::Flat(vec![
                "ready-for-review".to_string(),
            ])),
            assignees: Some(vec!["kingye".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    };
    let patterns = vec![developer, reviewer];

    let msg = make_message_with_rules(
        "pull_request",
        205,
        &["enhancement", "ready-for-dev"], // no ready-for-review
        &["kingye"],
    );

    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(result.is_some());
    assert_eq!(
        result.unwrap().pattern_name,
        "developer",
        "developer must still match when no review label is present"
    );
}

/// Within the non-reviewer bucket, TOML-declaration order is preserved
/// (stable sort). If two non-reviewer patterns both match, the one
/// listed first in the TOML wins.
#[test]
fn test_non_reviewer_patterns_preserve_toml_order() {
    let first = ChannelPattern {
        name: "first".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["issue".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    };
    let second = ChannelPattern {
        name: "second".to_string(),
        enabled: true,
        role: Some("Planner".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["issue".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    };
    let patterns = vec![first, second];
    let msg = make_message("issue", 1);
    let result = GithubMatcher.match_message(&msg, &patterns).unwrap();
    assert_eq!(
        result.pattern_name, "first",
        "stable sort: first non-reviewer pattern in TOML order should win"
    );
}

#[test]
fn test_pattern_priority_helper() {
    // Reviewer (any case) → 0, anything else → 255.
    assert_eq!(crate::git_host::pattern_priority(Some("Reviewer")), 0);
    assert_eq!(crate::git_host::pattern_priority(Some("reviewer")), 0);
    assert_eq!(crate::git_host::pattern_priority(Some("REVIEWER")), 0);
    assert_eq!(crate::git_host::pattern_priority(Some("Developer")), 255);
    assert_eq!(crate::git_host::pattern_priority(Some("Planner")), 255);
    assert_eq!(crate::git_host::pattern_priority(None), 255);
    assert_eq!(crate::git_host::pattern_priority(Some("")), 255);
}

#[test]
fn test_assignees_rule_blocks_wrong_assignee() {
    // Pattern requires assignee "alice", but issue is assigned to "bob"
    let msg = make_message_with_rules("issue", 42, &[], &["bob"]);

    let patterns = vec![ChannelPattern {
        name: "planner".to_string(),
        enabled: true,
        role: Some("Planner".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["issue".to_string()]),
            assignees: Some(vec!["alice".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_none(),
        "should not match when assignee doesn't match"
    );
}

#[test]
fn test_assignees_rule_allows_matching_assignee() {
    // Pattern requires assignee "alice", issue is assigned to "alice"
    let msg = make_message_with_rules("issue", 42, &[], &["alice"]);

    let patterns = vec![ChannelPattern {
        name: "planner".to_string(),
        enabled: true,
        role: Some("Planner".to_string()),
        rules: jyc_types::PatternRules {
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
    let msg = make_message_with_rules("issue", 42, &[], &["bob"]);

    let patterns = vec![ChannelPattern {
        name: "planner".to_string(),
        enabled: true,
        role: Some("Planner".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["issue".to_string()]),
            assignees: Some(vec!["alice".to_string(), "bob".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_some(),
        "should match when any assignee in the list matches"
    );
}

#[test]
fn test_assignees_rule_case_insensitive() {
    // Pattern has "Alice", issue has "alice"
    let msg = make_message_with_rules("issue", 42, &[], &["alice"]);

    let patterns = vec![ChannelPattern {
        name: "planner".to_string(),
        enabled: true,
        role: Some("Planner".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["issue".to_string()]),
            assignees: Some(vec!["Alice".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_some(),
        "assignee matching should be case-insensitive"
    );
}

#[test]
fn test_labels_rule_blocks_wrong_label() {
    // Pattern requires label "bug", but issue has "enhancement"
    let msg = make_message_with_rules("pull_request", 43, &["enhancement"], &[]);

    let patterns = vec![ChannelPattern {
        name: "developer".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            labels: Some(LabelRule::Flat(vec!["bug".to_string()])),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_none(),
        "should not match when label doesn't match"
    );
}

#[test]
fn test_labels_rule_allows_matching_label() {
    // Pattern requires label "bug", issue has "bug"
    let msg = make_message_with_rules("pull_request", 43, &["bug", "priority-high"], &[]);

    let patterns = vec![ChannelPattern {
        name: "developer".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
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
    let msg = make_message_with_rules("pull_request", 43, &["bug"], &[]);

    let patterns = vec![ChannelPattern {
        name: "developer".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            labels: Some(LabelRule::Flat(vec!["Bug".to_string()])),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_some(),
        "label matching should be case-insensitive"
    );
}

#[test]
fn test_all_rules_and_logic() {
    // Pattern requires: pull_request AND label "ready-for-review" AND assignee "alice"
    // Message has all three — should match
    let msg = make_message_with_rules("pull_request", 43, &["ready-for-review"], &["alice"]);

    let patterns = vec![ChannelPattern {
        name: "reviewer".to_string(),
        enabled: true,
        role: Some("Reviewer".to_string()),
        rules: jyc_types::PatternRules {
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
    let msg = make_message_with_rules("pull_request", 43, &["ready-for-review"], &["bob"]);

    let patterns = vec![ChannelPattern {
        name: "reviewer".to_string(),
        enabled: true,
        role: Some("Reviewer".to_string()),
        rules: jyc_types::PatternRules {
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
    let msg = make_message_with_rules("issue", 42, &["any-label"], &["anyone"]);

    let patterns = vec![ChannelPattern {
        name: "planner".to_string(),
        enabled: true,
        role: Some("Planner".to_string()),
        rules: jyc_types::PatternRules::default(),
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(result.is_some(), "no rules means match on role alone");
}

#[test]
fn test_no_assignees_on_issue_fails_assignee_rule() {
    // Pattern requires assignee "alice", but issue has no assignees
    let msg = make_message_with_rules("issue", 42, &[], &[]);

    let patterns = vec![ChannelPattern {
        name: "planner".to_string(),
        enabled: true,
        role: Some("Planner".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["issue".to_string()]),
            assignees: Some(vec!["alice".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_none(),
        "no assignees on issue should fail assignee rule"
    );
}

#[test]
fn test_fallback_to_second_pattern_when_first_rules_fail() {
    // Two patterns with same role but different rules.
    // First requires assignee "alice", second has no assignee rule.
    // Message has assignee "bob" — should skip first, match second.
    let msg = make_message_with_rules("issue", 42, &[], &["bob"]);

    let patterns = vec![
        ChannelPattern {
            name: "planner-alice".to_string(),
            enabled: true,
            role: Some("Planner".to_string()),
            rules: jyc_types::PatternRules {
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
            rules: jyc_types::PatternRules {
                github_type: Some(vec!["issue".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        },
    ];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(result.is_some());
    assert_eq!(
        result.unwrap().pattern_name,
        "planner-default",
        "should fall through to second pattern when first pattern's rules don't match"
    );
}

// --- Helper function tests ---

#[test]
fn test_extract_comment_role() {
    let allowed = Some(crate::git_host::GITHUB_COMMENT_ROLES);
    assert_eq!(
        crate::git_host::extract_comment_role("[Developer] some text", allowed),
        Some("Developer".to_string())
    );
    assert_eq!(
        crate::git_host::extract_comment_role("[Reviewer] code looks good", allowed),
        Some("Reviewer".to_string())
    );
    assert_eq!(
        crate::git_host::extract_comment_role("[Planner] questions", allowed),
        Some("Planner".to_string())
    );
    assert_eq!(
        crate::git_host::extract_comment_role("[High-Level Planner] planning", allowed),
        Some("High-Level Planner".to_string())
    );
    assert_eq!(
        crate::git_host::extract_comment_role("normal comment", allowed),
        None
    );
    assert_eq!(
        crate::git_host::extract_comment_role("[Unknown] something", allowed),
        None
    );
    assert_eq!(crate::git_host::extract_comment_role("", allowed), None);
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
    let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
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
    let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
    tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

    let mut processed = HashSet::new();

    // Track comments with id:updated_at keys
    adapter
        .track_comment("100:2024-01-01T00:00:00Z", &mut processed)
        .await;
    adapter
        .track_comment("200:2024-01-02T00:00:00Z", &mut processed)
        .await;
    adapter
        .track_comment("300:2024-01-03T00:00:00Z", &mut processed)
        .await;

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
    let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
    tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

    let mut processed = HashSet::new();

    // Track comment with original updated_at
    adapter
        .track_comment("100:2024-01-01T00:00:00Z", &mut processed)
        .await;
    assert!(processed.contains("100:2024-01-01T00:00:00Z"));

    // Same comment ID but different updated_at (edited) — should NOT be in set
    assert!(!processed.contains("100:2024-01-01T12:00:00Z"));

    // Track the edited version
    adapter
        .track_comment("100:2024-01-01T12:00:00Z", &mut processed)
        .await;

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
    let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
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
    let adapter = GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
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
    let adapter =
        GithubInboundAdapter::new(&config, "test_github".to_string(), tmpdir.path(), None);

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
    let adapter =
        GithubInboundAdapter::new(&config, "test_github".to_string(), tmpdir.path(), None);

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

#[test]
fn test_build_trigger_message_number_aliases_are_type_gated() {
    // Pipe topic templates use `${msg.pr_number}` / `${msg.issue_number}`.
    // Only the key matching the event type is set, so a planner pattern
    // configured with `plan-${msg.issue_number}` that accidentally matches a
    // PR event fails placeholder resolution (message dropped with a warning)
    // instead of silently landing PR traffic in an issue topic.
    let config = GithubConfig {
        owner: "kingye".to_string(),
        repo: "jyc".to_string(),
        token: "test".to_string(),
        api_url: "https://api.github.com".to_string(),
        poll_interval_secs: 60,
        poll_ci_status: true,
    };
    let tmpdir = tempfile::tempdir().unwrap();
    let adapter =
        GithubInboundAdapter::new(&config, "test_github".to_string(), tmpdir.path(), None);

    let issue = adapter.build_trigger_message(
        "issue_comment",
        42,
        "Add dark mode",
        "issue",
        "created",
        "user1",
        &[],
        &[],
        "comment-1",
    );
    assert_eq!(issue.metadata.get("issue_number").unwrap(), 42);
    assert!(!issue.metadata.contains_key("pr_number"));

    let pr = adapter.build_trigger_message(
        "pull_request",
        43,
        "Fix it",
        "pull_request",
        "opened",
        "user1",
        &[],
        &[],
        "pr-43-opened",
    );
    assert_eq!(pr.metadata.get("pr_number").unwrap(), 43);
    assert!(!pr.metadata.contains_key("issue_number"));

    // `repo` disambiguates topics when several github channels pipe into the
    // same agent (`review-${msg.repo}-${msg.pr_number}`).
    assert_eq!(pr.metadata.get("repo").unwrap(), "jyc");
}

#[test]
fn test_build_trigger_message_enterprise_gh_host() {
    let config = GithubConfig {
        owner: "Climate-21".to_string(),
        repo: "c21-networkcalculation-srv".to_string(),
        token: "test".to_string(),
        api_url: "https://github.tools.sap/api/v3".to_string(),
        poll_interval_secs: 60,
        poll_ci_status: true,
    };
    let tmpdir = tempfile::tempdir().unwrap();
    let adapter =
        GithubInboundAdapter::new(&config, "networkcalc".to_string(), tmpdir.path(), None);

    let msg = adapter.build_trigger_message(
        "pull_request",
        7880,
        "feat: Add mapping",
        "pull_request",
        "opened",
        "d032459",
        &[],
        &[],
        "pr-7880-opened",
    );

    let text = msg.content.text.unwrap();
    // Enterprise repos should include GH_HOST prefix
    assert!(
        text.contains("GH_HOST=github.tools.sap gh repo clone"),
        "expected GH_HOST prefix in clone cmd, got: {text}"
    );
    assert!(
        text.contains("GH_HOST=github.tools.sap gh pr view 7880"),
        "expected GH_HOST prefix in pr view cmd"
    );
    assert!(
        text.contains("GH_HOST=github.tools.sap gh pr diff 7880"),
        "expected GH_HOST prefix in pr diff cmd"
    );
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
    // Reviewer is promoted ahead of non-reviewer patterns by
    // `pattern_priority`. With both `developer` and `reviewer` matching
    // any pull_request (the test fixture has no label constraints),
    // `reviewer` wins regardless of TOML declaration order.
    // See `test_reviewer_pattern_wins_when_both_dev_and_review_labels_present`
    // for the production-shaped variant of this regression.
    assert_eq!(result.unwrap().pattern_name, "reviewer");
}

#[test]
fn test_pattern_self_loop_prevention() {
    let mut msg = make_message("pull_request", 43);
    msg.metadata
        .insert("comment_role".to_string(), serde_json::json!("Developer"));
    let patterns = vec![ChannelPattern {
        name: "developer".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
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
        rules: jyc_types::PatternRules {
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
    let msg = make_message_with_rules("pull_request", 43, &["bug", "test"], &[]);

    let patterns = vec![ChannelPattern {
        name: "developer".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
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
    assert!(
        result.is_some(),
        "should match when both AND groups are satisfied"
    );

    // Message has ["bug", "other"] → should NOT match (missing "test" group)
    let msg2 = make_message_with_rules("pull_request", 44, &["bug", "other"], &[]);

    let result2 = GithubMatcher.match_message(&msg2, &patterns);
    assert!(
        result2.is_none(),
        "should not match when second AND group is not satisfied"
    );
}

#[test]
fn test_labels_nested_single_group() {
    // Nested with single group: [["bug"]] behaves same as flat ["bug"]
    let msg = make_message_with_rules("pull_request", 43, &["bug"], &[]);

    let patterns = vec![ChannelPattern {
        name: "developer".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            labels: Some(LabelRule::Nested(vec![vec!["bug".to_string()]])),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_some(),
        "single nested group should behave like flat"
    );
}
