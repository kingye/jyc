# Gitee Channel

JYC supports multi-agent workflows on Gitee issues and Pull Requests, similar to the GitHub channel.

## Overview

The Gitee channel uses the Gitee API v5 to:
- Poll for new issues, PRs, and comments
- Post replies as comments on issues/PRs
- Support label-based routing for planner/developer/reviewer roles

## Configuration

```toml
[channels.mygitee]
type = "gitee"

[channels.mygitee.config]
owner = "myuser"
repo = "myproject"
token = "${GITEE_TOKEN}"              # Personal Access Token
poll_interval_secs = 60
# api_url = "https://gitee.com/api/v5"  # Default
```

## Multi-Agent Workflow

### Required Labels

Create these labels in your Gitee repository before using the workflow:

| Label | Purpose |
|-------|---------|
| `ready-for-dev` | Triggers the developer agent |
| `ready-for-review` | Triggers the reviewer agent |

### Pattern Configuration

```toml
[[channels.mygitee.patterns]]
name = "planner"
role = "Planner"
template = "gitee-planner"
thread_prefix = "issue"
rules = { github_type = ["issue"] }

[[channels.mygitee.patterns]]
name = "developer"
role = "Developer"
template = "gitee-developer"
thread_prefix = "pr"
rules = { github_type = ["pull_request"], labels = ["ready-for-dev"] }

[[channels.mygitee.patterns]]
name = "reviewer"
role = "Reviewer"
template = "gitee-reviewer"
thread_prefix = "review-pr"
rules = { github_type = ["pull_request"], labels = ["ready-for-review"] }
```

## Differences from GitHub Channel

| Feature | GitHub | Gitee |
|---------|--------|-------|
| CLI Tool | `gh` | `curl` + `jq` |
| PR Reviews | `gh pr review` | Comment-based (no formal review API) |
| Label Management | `gh pr edit --add-label` | API via `curl` |
| CI Status | GitHub Actions check-runs | Gitee Go build status |

## Authentication

Generate a Personal Access Token at:
**Settings → Security Settings → Private Token**

Required scopes:
- `projects` (read/write)
- `pull_requests` (read/write)
- `hook` (read)

## Limitations

1. **PR Reviews**: Gitee does not have a formal PR review API equivalent to GitHub's. The reviewer agent posts comments instead.
2. **API Rate Limits**: Gitee's free tier has stricter rate limits than GitHub. Consider increasing `poll_interval_secs` if you encounter rate limiting.
3. **CI Integration**: Gitee Go CI status polling is supported but may require additional configuration.
