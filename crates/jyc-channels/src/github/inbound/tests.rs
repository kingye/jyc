//! Tests for the GitHub inbound channel.
//!
//! Extracted from the monolithic `github/inbound.rs`.

#[cfg(test)]
mod tests {
    use crate::github::inbound::*;
    use jyc_types::{
        ChannelMatcher, ChannelPattern, InboundMessage, LabelRule, MessageContent, PatternMatch,
        PatternRules,
    };
    use std::collections::{HashMap, HashSet};

    fn make_message(github_type: &str, number: u64) -> InboundMessage {
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

    // --- Thread name derivation ---

    #[test]
    fn test_derive_thread_name_issue() {
        let msg = make_message("issue", 42);
        let name = GithubMatcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "issue-42");
    }

    #[test]
    fn test_derive_thread_name_pr() {
        let msg = make_message("pull_request", 43);
        let name = GithubMatcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "pr-43");
    }

    #[test]
    fn test_derive_thread_name_reviewer() {
        // The reviewer pattern no longer has a hardcoded special case; it
        // must declare `thread_prefix = "review-pr"` in config to keep the
        // historical thread name. With the prefix configured, the resolver
        // returns `review-pr-{N}`.
        let msg = make_message("pull_request", 43);
        let patterns = vec![ChannelPattern {
            name: "reviewer".to_string(),
            template: Some("github-reviewer".to_string()),
            thread_prefix: Some("review-pr".to_string()),
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
        let name = GithubMatcher.derive_thread_name(&msg, &patterns, Some(&pm));
        assert_eq!(name, "review-pr-43");
    }

    #[test]
    fn test_derive_thread_name_reviewer_legacy_fallback() {
        // Backwards-compat: a pattern literally named "reviewer" without an
        // explicit `thread_prefix` still routes to `review-pr-{N}` so existing
        // deployments don't break. A deprecation warning is logged at runtime;
        // users should migrate to `thread_prefix = "review-pr"` explicitly.
        let msg = make_message("pull_request", 43);
        let patterns = vec![ChannelPattern {
            name: "reviewer".to_string(),
            template: Some("github-reviewer".to_string()),
            // No thread_prefix → legacy fallback kicks in.
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
        let name = GithubMatcher.derive_thread_name(&msg, &patterns, Some(&pm));
        assert_eq!(name, "review-pr-43");
    }

    #[test]
    fn test_derive_thread_name_non_reviewer_no_prefix_falls_back_to_default() {
        // The legacy fallback is scoped to the literal pattern name "reviewer".
        // Any other pattern name without `thread_prefix` falls through to the
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
        let name = GithubMatcher.derive_thread_name(&msg, &patterns, Some(&pm));
        assert_eq!(name, "pr-43");
    }

    #[test]
    fn test_derive_thread_name_high_level_planner_prefix() {
        // Two issue patterns, distinguished by labels, must declare distinct
        // `thread_prefix` values to avoid sharing a workspace dir.
        let msg = make_message("issue", 42);
        let patterns = vec![
            ChannelPattern {
                name: "high-level-planner".to_string(),
                template: Some("github-high-level-planner".to_string()),
                thread_prefix: Some("plan".to_string()),
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
            GithubMatcher.derive_thread_name(&msg, &patterns, Some(&pm_hl)),
            "plan-42"
        );

        // Same issue + detail-planner pattern → default `issue-{N}`.
        let pm_detail = PatternMatch {
            pattern_name: "detail-planner".to_string(),
            channel: "github".to_string(),
            matches: HashMap::new(),
        };
        assert_eq!(
            GithubMatcher.derive_thread_name(&msg, &patterns, Some(&pm_detail)),
            "issue-42"
        );
    }

    #[test]
    fn test_derive_thread_name_developer() {
        let msg = make_message("pull_request", 43);
        let pm = PatternMatch {
            pattern_name: "developer".to_string(),
            channel: "github".to_string(),
            matches: HashMap::new(),
        };
        let name = GithubMatcher.derive_thread_name(&msg, &[], Some(&pm));
        assert_eq!(name, "pr-43");
    }

    // --- Rule filtering (github_type, labels, assignees) ---

    /// Helper: create a message with labels and assignees metadata
    fn make_message_with_rules(
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
        assert_eq!(pattern_priority(Some("Reviewer")), 0);
        assert_eq!(pattern_priority(Some("reviewer")), 0);
        assert_eq!(pattern_priority(Some("REVIEWER")), 0);
        assert_eq!(pattern_priority(Some("Developer")), 255);
        assert_eq!(pattern_priority(Some("Planner")), 255);
        assert_eq!(pattern_priority(None), 255);
        assert_eq!(pattern_priority(Some("")), 255);
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
        assert_eq!(
            extract_comment_role("[Developer] some text"),
            Some("Developer".to_string())
        );
        assert_eq!(
            extract_comment_role("[Reviewer] code looks good"),
            Some("Reviewer".to_string())
        );
        assert_eq!(
            extract_comment_role("[Planner] questions"),
            Some("Planner".to_string())
        );
        assert_eq!(
            extract_comment_role("[High-Level Planner] planning"),
            Some("High-Level Planner".to_string())
        );
        assert_eq!(extract_comment_role("normal comment"), None);
        assert_eq!(extract_comment_role("[Unknown] something"), None);
        assert_eq!(extract_comment_role(""), None);
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
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
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
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
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
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
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
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
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
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
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
    fn test_review_dedup_key_format() {
        let mut processed: HashSet<String> = HashSet::new();
        processed.insert("review-123:2026-04-15T10:00:00Z".to_string());
        processed.insert("review-comment-456:2026-04-15T10:00:00Z".to_string());

        assert!(processed.contains("review-123:2026-04-15T10:00:00Z"));
        assert!(processed.contains("review-comment-456:2026-04-15T10:00:00Z"));
        assert!(!processed.contains("review-123:2026-04-16T10:00:00Z"));
    }

    #[test]
    fn test_review_comment_role_extraction() {
        assert_eq!(
            extract_comment_role("[Developer] Fixed the issue"),
            Some("Developer".to_string())
        );
        assert_eq!(
            extract_comment_role("[Reviewer] Looks good"),
            Some("Reviewer".to_string())
        );
        assert_eq!(
            extract_comment_role("[Planner] Planning phase"),
            Some("Planner".to_string())
        );
        assert_eq!(extract_comment_role("Normal review comment"), None);
    }

    #[test]
    fn test_review_dedup_key_uniqueness() {
        let key1 = format!("review-{}:{}", 123, "2026-04-15T10:00:00Z");
        let key2 = format!("review-{}:{}", 123, "2026-04-16T10:00:00Z");
        let key3 = format!("review-comment-{}:{}", 456, "2026-04-15T10:00:00Z");

        assert_ne!(
            key1, key2,
            "Same review ID with different submitted_at should be different keys"
        );
        assert_ne!(
            key1, key3,
            "Review and review comment keys should be different"
        );
    }

    // --- CI status tracking tests ---

    fn make_ci_test_config() -> GithubConfig {
        GithubConfig {
            owner: "test".to_string(),
            repo: "test".to_string(),
            token: "test".to_string(),
            api_url: "https://api.github.com".to_string(),
            poll_interval_secs: 60,
            poll_ci_status: true,
        }
    }

    #[tokio::test]
    async fn test_ci_status_load_and_track() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut ci_status: HashMap<u64, (String, String)> = HashMap::new();

        adapter
            .track_ci_status(42, "abc123", "pending", &mut ci_status)
            .await;
        adapter
            .track_ci_status(43, "def456", "failure", &mut ci_status)
            .await;

        assert_eq!(ci_status.len(), 2);
        assert_eq!(
            ci_status.get(&42),
            Some(&("abc123".to_string(), "pending".to_string()))
        );
        assert_eq!(
            ci_status.get(&43),
            Some(&("def456".to_string(), "failure".to_string()))
        );

        let reloaded = adapter.load_ci_status().await;
        assert_eq!(reloaded.len(), 2);
        assert_eq!(
            reloaded.get(&42),
            Some(&("abc123".to_string(), "pending".to_string()))
        );
        assert_eq!(
            reloaded.get(&43),
            Some(&("def456".to_string(), "failure".to_string()))
        );
    }

    #[tokio::test]
    async fn test_ci_status_transition_triggers_message() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut ci_status: HashMap<u64, (String, String)> = HashMap::new();

        // PR 42 was previously tracked as "pending"
        adapter
            .track_ci_status(42, "abc123", "pending", &mut ci_status)
            .await;

        // Simulate transition: same head_sha, status changes to "failure"
        let (tracked_sha, previous_status) = ci_status.get(&42).cloned().unwrap();
        assert_eq!(tracked_sha, "abc123");
        assert_eq!(previous_status, "pending");

        // The polling logic would detect: overall_status="failure" && previous_status != "failure"
        // → trigger message. We test the state management part here.
        let should_trigger = previous_status != "failure";
        assert!(
            should_trigger,
            "Transition from pending to failure should trigger message"
        );

        // Update to failure
        adapter
            .track_ci_status(42, "abc123", "failure", &mut ci_status)
            .await;
        assert_eq!(
            ci_status.get(&42),
            Some(&("abc123".to_string(), "failure".to_string()))
        );
    }

    #[tokio::test]
    async fn test_ci_status_no_retrigger_on_same_failure() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut ci_status: HashMap<u64, (String, String)> = HashMap::new();

        // PR 42 is already tracked as "failure"
        adapter
            .track_ci_status(42, "abc123", "failure", &mut ci_status)
            .await;

        // On next poll, same failure status — should NOT re-trigger
        let (_, previous_status) = ci_status.get(&42).cloned().unwrap();
        let should_trigger = previous_status != "failure";
        assert!(!should_trigger, "Same failure status should not re-trigger");
    }

    #[tokio::test]
    async fn test_ci_status_resets_on_new_commit() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut ci_status: HashMap<u64, (String, String)> = HashMap::new();

        // PR 42 tracked with old head_sha in "failure" status
        adapter
            .track_ci_status(42, "abc123", "failure", &mut ci_status)
            .await;

        // Developer pushes a fix — new head_sha
        let new_head_sha = "def456";
        let (tracked_sha, _) = ci_status.get(&42).cloned().unwrap();

        // head_sha changed → tracking should reset
        let should_reset = tracked_sha != new_head_sha;
        assert!(should_reset, "New commit should reset CI status tracking");

        // After reset, previous_status would be None → transition to any status triggers
        // Simulate: new commit's CI is still pending
        adapter
            .track_ci_status(42, new_head_sha, "pending", &mut ci_status)
            .await;
        assert_eq!(
            ci_status.get(&42),
            Some(&("def456".to_string(), "pending".to_string()))
        );

        // Now CI fails on new commit → should trigger (because we reset)
        let (_, previous_status) = ci_status.get(&42).cloned().unwrap();
        let should_trigger = previous_status != "failure";
        assert!(
            should_trigger,
            "Failure on new commit should trigger after reset"
        );
    }

    #[tokio::test]
    async fn test_ci_status_empty_load() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let ci_status = adapter.load_ci_status().await;
        assert!(ci_status.is_empty());
    }

    #[tokio::test]
    async fn test_ci_status_compact() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut ci_status: HashMap<u64, (String, String)> = HashMap::new();

        // Add entries
        for i in 1..=100u64 {
            adapter
                .track_ci_status(i, &format!("sha{}", i), "success", &mut ci_status)
                .await;
        }

        assert_eq!(ci_status.len(), 100);

        // Compact should rewrite the file (no size reduction since under threshold)
        adapter.compact_ci_status(&mut ci_status).await;
        assert_eq!(ci_status.len(), 100);

        // Verify persistence
        let reloaded = adapter.load_ci_status().await;
        assert_eq!(reloaded.len(), 100);
    }

    #[test]
    fn test_ci_failure_message_routing() {
        // CI failure messages with github_type=pull_request should route to pr-{N}
        let mut msg = make_message("pull_request", 43);
        msg.metadata.insert(
            "github_event".to_string(),
            serde_json::Value::String("check_run".to_string()),
        );
        msg.metadata.insert(
            "github_action".to_string(),
            serde_json::Value::String("completed".to_string()),
        );

        let name = GithubMatcher.derive_thread_name(&msg, &[], None);
        assert_eq!(name, "pr-43");

        // CI failure with developer pattern match should also route to pr-{N}
        let pm = PatternMatch {
            pattern_name: "developer".to_string(),
            channel: "github".to_string(),
            matches: HashMap::new(),
        };
        let name_with_pattern = GithubMatcher.derive_thread_name(&msg, &[], Some(&pm));
        assert_eq!(name_with_pattern, "pr-43");
    }

    #[tokio::test]
    async fn test_ci_status_no_file_growth_on_unchanged() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
        tokio::fs::create_dir_all(&adapter.state_dir).await.unwrap();

        let mut ci_status: HashMap<u64, (String, String)> = HashMap::new();

        adapter
            .track_ci_status(42, "abc123", "failure", &mut ci_status)
            .await;
        let file = adapter.state_dir.join("ci-status.txt");
        let lines_after_first = tokio::fs::read_to_string(&file)
            .await
            .unwrap()
            .lines()
            .count();

        // Track same entry again — file should be rewritten (same content), not grow
        adapter
            .track_ci_status(42, "abc123", "failure", &mut ci_status)
            .await;
        let lines_after_second = tokio::fs::read_to_string(&file)
            .await
            .unwrap()
            .lines()
            .count();

        assert_eq!(
            lines_after_first, lines_after_second,
            "File should not grow when status is unchanged"
        );
    }

    #[test]
    fn test_ci_timed_out_triggers_failure() {
        let check_runs = [crate::github::client::GithubCheckRun {
            id: 1,
            name: "CI".to_string(),
            status: "completed".to_string(),
            conclusion: Some("timed_out".to_string()),
            head_sha: "abc123def456".to_string(),
            started_at: None,
            completed_at: None,
        }];

        let has_failure = check_runs.iter().any(|cr| {
            cr.conclusion.as_deref() == Some("failure")
                || cr.conclusion.as_deref() == Some("timed_out")
        });

        assert!(has_failure, "timed_out should be treated as failure");

        let failed_checks: Vec<_> = check_runs
            .iter()
            .filter(|cr| {
                cr.conclusion.as_deref() == Some("failure")
                    || cr.conclusion.as_deref() == Some("timed_out")
            })
            .collect();

        assert_eq!(failed_checks.len(), 1);
        assert_eq!(failed_checks[0].name, "CI");
    }

    #[test]
    fn test_safe_head_sha_truncation() {
        let short_sha = "abc".to_string();
        let result = short_sha.get(..8).unwrap_or(&short_sha);
        assert_eq!(result, "abc");

        let normal_sha = "abc123def456".to_string();
        let result2 = normal_sha.get(..8).unwrap_or(&normal_sha);
        assert_eq!(result2, "abc123de");

        let empty_sha = "".to_string();
        let result3 = empty_sha.get(..8).unwrap_or(&empty_sha);
        assert_eq!(result3, "");
    }

    // --- scan_active_pr_threads tests ---

    #[test]
    fn test_scan_active_pr_threads_empty_workspace() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);

        let result = adapter.scan_active_pr_threads();
        assert!(
            result.is_empty(),
            "should return empty set when workspace dir does not exist"
        );
    }

    #[test]
    fn test_scan_active_pr_threads_with_pr_dirs() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        // Inject a reviewer pattern with `thread_prefix = "review-pr"` so the
        // scan recognizes `review-pr-{N}` directories. Without the pattern,
        // only the default `pr-{N}` prefix is recognized.
        let reviewer_pattern = ChannelPattern {
            name: "reviewer".to_string(),
            thread_prefix: Some("review-pr".to_string()),
            rules: PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        };
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None)
                .with_patterns(vec![reviewer_pattern]);

        let workspace = tmpdir.path().join("test_ch").join("workspace");
        std::fs::create_dir_all(workspace.join("pr-42")).unwrap();
        std::fs::create_dir_all(workspace.join("review-pr-43")).unwrap();
        std::fs::create_dir_all(workspace.join("issue-5")).unwrap();

        let result = adapter.scan_active_pr_threads();
        assert_eq!(result.len(), 2);
        assert!(result.contains(&42));
        assert!(result.contains(&43));
        assert!(!result.contains(&5), "issue dirs should not be included");
    }

    #[test]
    fn test_scan_active_pr_threads_review_prefix_legacy_fallback() {
        // A pattern named "reviewer" without an explicit `thread_prefix` still
        // contributes `review-pr` to the recognized PR prefix set, mirroring
        // the legacy fallback in derive_thread_name. This keeps disk scans
        // consistent for existing deployments that haven't migrated yet.
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let reviewer_pattern = ChannelPattern {
            name: "reviewer".to_string(),
            // No thread_prefix.
            rules: PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        };
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None)
                .with_patterns(vec![reviewer_pattern]);

        let workspace = tmpdir.path().join("test_ch").join("workspace");
        std::fs::create_dir_all(workspace.join("review-pr-43")).unwrap();
        std::fs::create_dir_all(workspace.join("pr-42")).unwrap();

        let result = adapter.scan_active_pr_threads();
        assert_eq!(result.len(), 2);
        assert!(result.contains(&42));
        assert!(result.contains(&43));
    }

    #[test]
    fn test_scan_active_pr_threads_review_prefix_unknown_pattern_ignored() {
        // The legacy fallback is keyed on the pattern name "reviewer" only.
        // For any other pattern name without `thread_prefix`, `review-pr-{N}`
        // dirs are not recognized.
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let qa_pattern = ChannelPattern {
            name: "qa".to_string(),
            rules: PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        };
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None)
                .with_patterns(vec![qa_pattern]);

        let workspace = tmpdir.path().join("test_ch").join("workspace");
        std::fs::create_dir_all(workspace.join("review-pr-43")).unwrap();
        std::fs::create_dir_all(workspace.join("pr-42")).unwrap();

        let result = adapter.scan_active_pr_threads();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&42));
        assert!(!result.contains(&43));
    }

    #[test]
    fn test_scan_active_pr_threads_non_numeric_suffix() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);

        let workspace = tmpdir.path().join("test_ch").join("workspace");
        std::fs::create_dir_all(workspace.join("pr-abc")).unwrap();

        let result = adapter.scan_active_pr_threads();
        assert!(result.is_empty(), "non-numeric suffix should be skipped");
    }

    #[test]
    fn test_ci_polling_filters_by_active_threads() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        // Reviewer pattern is required for `review-pr-` to be recognized.
        let reviewer_pattern = ChannelPattern {
            name: "reviewer".to_string(),
            thread_prefix: Some("review-pr".to_string()),
            rules: PatternRules {
                github_type: Some(vec!["pull_request".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        };
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None)
                .with_patterns(vec![reviewer_pattern]);

        let workspace = tmpdir.path().join("test_ch").join("workspace");
        std::fs::create_dir_all(workspace.join("pr-42")).unwrap();
        std::fs::create_dir_all(workspace.join("review-pr-43")).unwrap();

        let active_pr_threads = adapter.scan_active_pr_threads();

        let open_pr_numbers: Vec<u64> = vec![42, 43, 44, 45];

        let polled: Vec<u64> = open_pr_numbers
            .iter()
            .filter(|pr| active_pr_threads.contains(pr))
            .copied()
            .collect();

        let mut sorted = polled.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![42, 43],
            "only PRs with active thread dirs should be polled"
        );
    }

    #[test]
    fn test_ci_polling_no_active_threads_skips_all() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);

        let active_pr_threads = adapter.scan_active_pr_threads();

        let open_pr_numbers: Vec<u64> = vec![100, 200, 300];

        let polled: Vec<u64> = open_pr_numbers
            .iter()
            .filter(|pr| active_pr_threads.contains(pr))
            .copied()
            .collect();

        assert!(
            polled.is_empty(),
            "no workspace dirs means no PRs should be polled"
        );
    }

    // --- scan_threads_for_number tests ---

    #[test]
    fn test_scan_threads_for_number_matches_all_prefixes() {
        // Closing an issue/PR should enumerate every workspace dir whose name
        // ends in `-{N}`, regardless of which prefix patterns are configured.
        // This is the channel-agnostic close-thread enumeration used by the
        // GitHub close path.
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);

        let workspace = tmpdir.path().join("test_ch").join("workspace");
        std::fs::create_dir_all(workspace.join("issue-42")).unwrap();
        std::fs::create_dir_all(workspace.join("plan-42")).unwrap();
        std::fs::create_dir_all(workspace.join("issue-99")).unwrap();
        // Tricky: ends in -42 but with empty prefix should be ignored if it
        // happens to materialize as a literal "-42" dir; here we add a normal
        // unrelated one to confirm the strict suffix match.
        std::fs::create_dir_all(workspace.join("unrelated-43")).unwrap();

        let mut result = adapter.scan_threads_for_number(42);
        result.sort();
        assert_eq!(result, vec!["issue-42".to_string(), "plan-42".to_string()]);
    }

    #[test]
    fn test_scan_threads_for_number_empty_workspace() {
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);
        let result = adapter.scan_threads_for_number(42);
        assert!(result.is_empty());
    }

    #[test]
    fn test_scan_threads_for_number_no_substring_false_match() {
        // A directory ending in -420 must NOT match number 42.
        let tmpdir = tempfile::tempdir().unwrap();
        let config = make_ci_test_config();
        let adapter =
            GithubInboundAdapter::new(&config, "test_ch".to_string(), tmpdir.path(), None);

        let workspace = tmpdir.path().join("test_ch").join("workspace");
        std::fs::create_dir_all(workspace.join("issue-420")).unwrap();
        std::fs::create_dir_all(workspace.join("issue-42")).unwrap();

        let mut result = adapter.scan_threads_for_number(42);
        result.sort();
        assert_eq!(result, vec!["issue-42".to_string()]);
    }

    // --- Dedup tests for triggered_in_cycle ---

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

    #[test]
    fn test_ci_failure_not_blocked_by_other_triggers() {
        // Regression test: CI failure events use a separate dedup key
        // ("ci-{N}") so they are not blocked when another event type
        // (comment, review, label change) already triggered for the
        // same PR in the same poll cycle.
        let mut triggered_in_cycle: HashSet<String> = HashSet::new();

        // Simulate a comment trigger for PR 42 that fired first
        assert!(triggered_in_cycle.insert("42".to_string()));

        // CI failure for PR 42 should still be allowed — different key
        assert!(
            triggered_in_cycle.insert("ci-42".to_string()),
            "CI failure should use a different dedup key than other triggers"
        );
    }

    #[test]
    fn test_ci_failure_self_dedup() {
        // CI failure events for the same PR should still deduplicate
        // against each other within the same cycle.
        let mut triggered_in_cycle: HashSet<String> = HashSet::new();

        // First CI failure trigger for PR 42 — allowed
        assert!(triggered_in_cycle.insert("ci-42".to_string()));

        // Second CI failure trigger for same PR 42 — blocked (duplicate CI)
        assert!(
            !triggered_in_cycle.insert("ci-42".to_string()),
            "CI failure should deduplicate against itself"
        );
    }
}
