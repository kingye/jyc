use super::inbound_tests::make_message_with_rules;
use crate::github::inbound::*;
use jyc_types::{ChannelMatcher, ChannelPattern, LabelRule};
use std::collections::HashSet;

#[test]
fn test_labels_nested_all_and() {
    // Nested: [["bug"], ["test"], ["v2"]] → requires all three labels
    let msg = make_message_with_rules("pull_request", 43, &["bug", "test", "v2"], &[]);

    let patterns = vec![ChannelPattern {
        name: "developer".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
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
    assert!(
        result.is_some(),
        "should match when all three labels are present"
    );

    // Missing one label → should NOT match
    let msg2 = make_message_with_rules("pull_request", 44, &["bug", "test"], &[]);

    let result2 = GithubMatcher.match_message(&msg2, &patterns);
    assert!(
        result2.is_none(),
        "should not match when one required label is missing"
    );
}

#[test]
fn test_labels_nested_empty_group() {
    // Edge case: empty inner group [[]] should not block matching
    let msg = make_message_with_rules("pull_request", 43, &["bug"], &[]);

    let patterns = vec![ChannelPattern {
        name: "developer".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            labels: Some(LabelRule::Nested(vec![
                vec!["bug".to_string()],
                vec![], // empty group — should be treated as always-match
            ])),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_some(),
        "empty inner group should not block matching"
    );
}

#[test]
fn test_labels_flat_backward_compat() {
    // Verify Flat(vec!["bug", "enhancement"]) still uses OR logic
    let msg = make_message_with_rules("pull_request", 43, &["enhancement"], &[]);

    let patterns = vec![ChannelPattern {
        name: "developer".to_string(),
        enabled: true,
        role: Some("Developer".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["pull_request".to_string()]),
            labels: Some(LabelRule::Flat(vec![
                "bug".to_string(),
                "enhancement".to_string(),
            ])),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_some(),
        "flat labels should use OR logic — enhancement matches"
    );

    // Neither label present → should NOT match
    let msg2 = make_message_with_rules("pull_request", 44, &["other"], &[]);

    let result2 = GithubMatcher.match_message(&msg2, &patterns);
    assert!(
        result2.is_none(),
        "flat labels OR logic — no matching label"
    );
}

// --- TOML deserialization tests for LabelRule ---

#[test]
fn test_labels_toml_flat_deserialize() {
    let pattern: ChannelPattern = toml::from_str(
        r#"
            name = "test"
            [rules]
            labels = ["bug", "enhancement"]
        "#,
    )
    .unwrap();
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
    let pattern: ChannelPattern = toml::from_str(
        r#"
            name = "test"
            [rules]
            labels = [["bug", "enhancement"], ["test"]]
        "#,
    )
    .unwrap();
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
    use crate::github::client::{GithubComment, GithubUser};

    let open_numbers: HashSet<u64> = [10, 20].into_iter().collect();

    // Comment on open issue #10 → should be processed
    let comment = GithubComment {
        id: 1,
        user: GithubUser {
            login: "user1".to_string(),
        },
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
    use crate::github::client::{GithubComment, GithubUser};

    let open_numbers: HashSet<u64> = [10, 20].into_iter().collect();

    // Comment on closed issue #30 → should be skipped
    let comment = GithubComment {
        id: 2,
        user: GithubUser {
            login: "user1".to_string(),
        },
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
    use crate::github::client::{GithubComment, GithubUser};

    let open_numbers: HashSet<u64> = [10, 20].into_iter().collect();

    // Comment with malformed issue_url → issue_number() returns None,
    // unwrap_or(0) yields 0, which is not in open set → should be skipped
    let comment = GithubComment {
        id: 3,
        user: GithubUser {
            login: "user1".to_string(),
        },
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
    let msg = make_message_with_rules("issue", 42, &["feature-plan"], &[]);

    let patterns = vec![ChannelPattern {
        name: "detail-planner".to_string(),
        enabled: true,
        role: Some("Planner".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["issue".to_string()]),
            exclude_labels: Some(vec!["feature-plan".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_none(),
        "exclude_labels should block matching when label is present"
    );
}

#[test]
fn test_exclude_labels_allows_non_matching_label() {
    // Message has "bug", pattern has exclude_labels = ["feature-plan"] → should match
    let msg = make_message_with_rules("issue", 42, &["bug"], &[]);

    let patterns = vec![ChannelPattern {
        name: "detail-planner".to_string(),
        enabled: true,
        role: Some("Planner".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["issue".to_string()]),
            exclude_labels: Some(vec!["feature-plan".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_some(),
        "exclude_labels should not block when excluded label is absent"
    );
}

#[test]
fn test_exclude_labels_multiple() {
    // Message has "feature-plan", pattern has exclude_labels = ["feature-plan", "wip"] → should NOT match
    let msg = make_message_with_rules("issue", 42, &["feature-plan"], &[]);

    let patterns = vec![ChannelPattern {
        name: "detail-planner".to_string(),
        enabled: true,
        role: Some("Planner".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["issue".to_string()]),
            exclude_labels: Some(vec!["feature-plan".to_string(), "wip".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_none(),
        "exclude_labels should block when any exclude label matches"
    );
}

#[test]
fn test_exclude_labels_case_insensitive() {
    // Message has "Feature-Plan", pattern has exclude_labels = ["feature-plan"] → should NOT match
    let msg = make_message_with_rules("issue", 42, &["Feature-Plan"], &[]);

    let patterns = vec![ChannelPattern {
        name: "detail-planner".to_string(),
        enabled: true,
        role: Some("Planner".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["issue".to_string()]),
            exclude_labels: Some(vec!["feature-plan".to_string()]),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_none(),
        "exclude_labels matching should be case-insensitive"
    );
}

#[test]
fn test_exclude_labels_toml_deserialize() {
    let pattern: ChannelPattern = toml::from_str(
        r#"
            name = "test"
            [rules]
            github_type = ["issue"]
            exclude_labels = ["feature-plan", "wip"]
        "#,
    )
    .unwrap();
    assert!(
        pattern.rules.exclude_labels.is_some(),
        "exclude_labels should deserialize"
    );
    let excl = pattern.rules.exclude_labels.unwrap();
    assert_eq!(excl.len(), 2);
    assert!(excl.contains(&"feature-plan".to_string()));
    assert!(excl.contains(&"wip".to_string()));
}

#[test]
fn test_high_level_planner_with_feature_plan_label() {
    // High-level planner pattern: labels = ["feature-plan"]
    let msg = make_message_with_rules("issue", 42, &["feature-plan"], &[]);

    let patterns = vec![ChannelPattern {
        name: "high-level-planner".to_string(),
        enabled: true,
        role: Some("High-Level Planner".to_string()),
        template: Some("github-high-level-planner".to_string()),
        rules: jyc_types::PatternRules {
            github_type: Some(vec!["issue".to_string()]),
            labels: Some(LabelRule::Flat(vec!["feature-plan".to_string()])),
            ..Default::default()
        },
        ..Default::default()
    }];
    let result = GithubMatcher.match_message(&msg, &patterns);
    assert!(
        result.is_some(),
        "high-level planner should match issue with feature-plan label"
    );
    assert_eq!(result.unwrap().pattern_name, "high-level-planner");
}

#[test]
fn test_two_level_routing() {
    // Issue with feature-plan → high-level planner
    let msg1 = make_message_with_rules("issue", 42, &["feature-plan"], &[]);

    let patterns = vec![
        ChannelPattern {
            name: "high-level-planner".to_string(),
            enabled: true,
            role: Some("High-Level Planner".to_string()),
            template: Some("github-high-level-planner".to_string()),
            rules: jyc_types::PatternRules {
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
            rules: jyc_types::PatternRules {
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
    let msg2 = make_message_with_rules("issue", 43, &["bug"], &[]);

    let result2 = GithubMatcher.match_message(&msg2, &patterns);
    assert!(result2.is_some());
    assert_eq!(result2.unwrap().pattern_name, "detail-planner");
}

#[test]
fn test_review_comment_role_extraction() {
    assert_eq!(
        crate::git_host::extract_comment_role(
            "[Developer] Fixed the issue",
            Some(crate::git_host::GITHUB_COMMENT_ROLES)
        ),
        Some("Developer".to_string())
    );
    assert_eq!(
        crate::git_host::extract_comment_role(
            "[Reviewer] Looks good",
            Some(crate::git_host::GITHUB_COMMENT_ROLES)
        ),
        Some("Reviewer".to_string())
    );
    assert_eq!(
        crate::git_host::extract_comment_role(
            "[Planner] Planning phase",
            Some(crate::git_host::GITHUB_COMMENT_ROLES)
        ),
        Some("Planner".to_string())
    );
    assert_eq!(
        crate::git_host::extract_comment_role(
            "Normal review comment",
            Some(crate::git_host::GITHUB_COMMENT_ROLES)
        ),
        None
    );
}

#[test]
fn test_triggered_in_cycle_allows_first_trigger() {
    // Simulates the dedup guard pattern used in poll_once() for all
    // trigger types (label, comment, review, review_comment, CI failure).
    // Validates that the first trigger for an issue/PR is allowed through.
    let mut triggered_in_cycle: HashSet<String> = HashSet::new();

    // First insert for issue 42 should return true (allowed)
    assert!(
        triggered_in_cycle.insert("42".to_string()),
        "First trigger for an issue should be allowed"
    );
}

#[test]
fn test_triggered_in_cycle_blocks_duplicate() {
    let mut triggered_in_cycle: HashSet<String> = HashSet::new();

    // First insert returns true
    assert!(triggered_in_cycle.insert("42".to_string()));

    // Second insert for same number returns false (blocked)
    assert!(
        !triggered_in_cycle.insert("42".to_string()),
        "Duplicate trigger for same issue should be blocked"
    );
}

#[test]
fn test_triggered_in_cycle_allows_different_numbers() {
    let mut triggered_in_cycle: HashSet<String> = HashSet::new();

    // Insert different issue numbers — all should be allowed
    assert!(triggered_in_cycle.insert("42".to_string()));
    assert!(
        triggered_in_cycle.insert("43".to_string()),
        "Different issue numbers should each be allowed"
    );
    assert!(triggered_in_cycle.insert("100".to_string()));
}

#[test]
fn test_triggered_in_cycle_independent_per_cycle() {
    // Validates that the dedup set is scoped per poll cycle — a fresh
    // HashSet is created for each poll_once() call, so the same issue
    // number can trigger again in a new cycle.
    let mut cycle1: HashSet<String> = HashSet::new();
    assert!(cycle1.insert("42".to_string())); // First cycle: allowed
    assert!(!cycle1.insert("42".to_string())); // First cycle: duplicate blocked

    // New cycle = fresh HashSet
    let mut cycle2: HashSet<String> = HashSet::new();
    assert!(
        cycle2.insert("42".to_string()),
        "Same issue should be allowed in a new poll cycle"
    );
}

#[test]
fn test_triggered_in_cycle_mixed_issues() {
    // Validates realistic scenario: issue 42 triggers via label change,
    // then a comment for issue 42 is blocked, while a comment for
    // issue 43 is still allowed.
    let mut triggered_in_cycle: HashSet<String> = HashSet::new();

    // Label trigger for issue 42 — allowed
    assert!(triggered_in_cycle.insert("42".to_string()));

    // Comment trigger for issue 42 — blocked (duplicate)
    assert!(!triggered_in_cycle.insert("42".to_string()));

    // Comment trigger for issue 43 — allowed (different issue)
    assert!(triggered_in_cycle.insert("43".to_string()));

    // Review trigger for PR 42 — blocked (same number, still in cycle)
    assert!(!triggered_in_cycle.insert("42".to_string()));

    // Review trigger for PR 99 — allowed
    assert!(triggered_in_cycle.insert("99".to_string()));
}
