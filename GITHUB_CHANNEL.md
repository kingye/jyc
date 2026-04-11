# GitHub Channel

GitHub Issue/PR channel for jyc — enables AI agents to work on GitHub issues, create PRs, and participate in code reviews.

## Architecture

```
GitHub API (polling)
    ↓
poll_events()
├── Fetch all open items (issues + PRs) from /issues?state=open
├── Classify into issues and PRs
│
├── process_comments()
│   ├── Fetch comments from /issues/comments?since=...
│   ├── Skip bot's own comments
│   ├── Skip comments on closed items
│   └── Route to thread: github-<number>
│
├── process_issues()
│   ├── Filter: only issues, open, not already processed
│   └── Route to thread: github-<issue_number>
│
└── process_pull_requests()
    ├── Filter: only PRs, open, not already processed
    ├── Detect linked issue (Fixes #N, Closes #N, Resolves #N)
    ├── If linked → route to issue thread (github-<issue_number>)
    └── If not linked → own thread (github-<pr_number>)
```

## Configuration

```toml
[channels.jyc_repo]
type = "github"

[channels.jyc_repo.github]
owner = "myorg"
repo = "myrepo"
token = "${GITHUB_TOKEN}"
poll_interval_secs = 120
events = ["issue_comment", "issues", "pull_request"]

# Pattern: route issues with specific labels
[[channels.jyc_repo.patterns]]
name = "dev"
enabled = true
template = "jyc-dev"

[channels.jyc_repo.patterns.rules]
labels = ["bug", "enhancement", "question"]

# Pattern: route PRs labeled 'review' to review agent
[[channels.jyc_repo.patterns]]
name = "review"
enabled = true
template = "jyc-review"

[channels.jyc_repo.patterns.rules]
labels = ["review"]
```

## Event Types

| Event | Description | Config |
|-------|-------------|--------|
| `issues` | New open issues | `events = ["issues"]` |
| `issue_comment` | Comments on open issues and PRs | `events = ["issue_comment"]` |
| `pull_request` | New open PRs | `events = ["pull_request"]` |

## Thread Naming

- Issues: `github-<issue_number>` (e.g., `github-42`)
- PRs linked to issues: routed to `github-<issue_number>` (same thread)
- PRs not linked: `github-<pr_number>`

## Issue → PR Lifecycle

```
1. User creates Issue #42 (label: bug)
   → Thread github-42 created, dev agent assigned
   
2. Dev agent creates branch: fix/issue-42
   → Agent implements fix (incremental-dev skill)
   
3. Dev agent creates PR #45 (body: "Fixes #42")
   → PR linked to issue, comments routed to github-42 thread
   
4. User labels PR with "review"
   → Review agent picks up PR, posts review via pr-review skill
   
5. Review comments appear on PR
   → Routed to github-42 thread, dev agent fixes issues
   
6. User approves and merges PR
   → Issue closed, thread becomes inactive
```

## Deduplication

- **Issues**: Tracked by `issue.id` in memory. Same issue not re-processed across polls.
- **Comments**: Tracked by `comment.id` in memory. Same comment not re-processed.
- **Bot comments**: Detected by `[bot]` suffix in username or reply footer (`Model:/Mode:`). Skipped to prevent feedback loops.

## Pattern Matching

| Rule | Description |
|------|-------------|
| `labels` | Match issues/PRs with any of the specified labels |
| `sender.exact` | Match by GitHub user ID |
| `sender.regex` | Match by GitHub user ID pattern |

## Data Model Mapping

| GitHub Field | InboundMessage Field |
|-------------|---------------------|
| Issue/PR body | `content.markdown` |
| Issue/PR title | `topic` |
| Issue/PR number | `channel_uid`, `metadata["issue_number"]` |
| User login | `sender` |
| User ID | `sender_address` |
| Labels | `metadata["labels"]` |
| Event type | `metadata["event_type"]` |
| HTML URL | `metadata["html_url"]` |
| Is PR | `metadata["is_pr"]` |
| Linked issue | `metadata["linked_issue"]` |

## Outbound

Replies are posted as comments on the issue/PR via `POST /repos/{owner}/{repo}/issues/{number}/comments`.

## Files

| File | Purpose |
|------|---------|
| `src/channels/github/config.rs` | Channel configuration |
| `src/channels/github/types.rs` | GitHub API types |
| `src/channels/github/client.rs` | HTTP client for GitHub REST API |
| `src/channels/github/inbound.rs` | Polling, pattern matching, message routing |
| `src/channels/github/outbound.rs` | Posting comments to issues/PRs |
