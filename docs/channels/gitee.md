# Gitee Channel

JYC supports multi-agent workflows on Gitee issues and Pull Requests, similar
to the GitHub channel. Since the pipe-only migration, the Gitee channel is a
**pure pipe adapter**: it polls the Gitee API, re-targets each event into a
hub channel (or an agent topic), and relays agent replies back as
issue/PR comments.

## Overview

The Gitee channel uses the Gitee API v5 to:
- Poll for new issues, PRs, and comments
- Relay agent replies as comments on issues/PRs
- Support label-based routing for planner/developer roles

Like `github`, the Gitee channel owns **no** TopicManager, agent service,
outbound adapter, or orchestrator registration — all topics live in the pipe
target. Dedup/cursor state lives at `<workdir>/channels/<channel>/.gitee/`
(one-time rename from the old `<workdir>/<channel>/.gitee/`). See
`docs/core-hub-adapters.md` for the full pipe architecture.

## Configuration

```toml
[channels.mygitee]
type = "gitee"

[channels.mygitee.gitee]
owner = "myuser"
repo = "myproject"
token = "${GITEE_TOKEN}"              # Personal Access Token
poll_interval_secs = 60
# api_url = "https://gitee.com/api/v5"  # Default
```

Every enabled Gitee pattern **must** declare a `pipe` target — matching
messages are dropped otherwise (warned at startup). The pipe target routes
into a hub channel (`channel`) or an agent topic (`agent`).

## Multi-Agent Workflow

### Required Labels

Create these labels in your Gitee repository before using the workflow:

| Label | Purpose |
|-------|---------|
| `ready-for-dev` | Triggers the developer agent |
| `ready-for-review` | Triggers the reviewer agent |

### Pattern Configuration

```toml
# Pattern: Issues → Planner
[[channels.mygitee.patterns]]
name = "planner"
enabled = true
role = "Planner"
rules = { github_type = ["issue"] }
pipe = { agent = "jyc_git", topic = "plan-${msg.issue_number}" }

# Pattern: Pull Requests with 'ready-for-dev' label → Developer
[[channels.mygitee.patterns]]
name = "developer"
enabled = true
role = "Developer"
rules = { github_type = ["pull_request"], labels = ["ready-for-dev"] }
pipe = { agent = "jyc_git", topic = "dev-${msg.pr_number}" }

# Pattern: Pull Requests with 'ready-for-review' label → Reviewer
[[channels.mygitee.patterns]]
name = "reviewer"
enabled = true
role = "Reviewer"
rules = { github_type = ["pull_request"], labels = ["ready-for-review"] }
pipe = { agent = "jyc_git", topic = "review-${msg.pr_number}" }
```

### Topic Placeholders

Gitee messages populate `repo`, `gitee_number`, `gitee_type`
(`pull_request` / `issue`), `gitee_action`, `gitee_labels`,
`gitee_assignees`, plus `pr_number` **or** `issue_number` — type-gated, so a
PR event carries only `pr_number` and an issue event only `issue_number`
(exactly like GitHub).

### Reply Relaying

The forwarder keeps a `topic → (number, role, is_pr)` map recorded on inbound
and posts replies as comments via the Gitee API. The `[Role]` prefix is
preserved — it is also how the poller recognizes its own comments and avoids
self-loops. There is **no** model/mode/token footer, and Gitee comments carry
no attachments. Gitee uses **separate number spaces for issues and PRs**, so
the map records whether the item is a PR; a close event only matches topics of
the same type.

### Close Events

A closed issue/PR closes the routed agent topics in the hub workspace
(`plan-<N>`, `dev-<N>`, `review-<N>`, ...), derived from config
(`close_event_topics`) and unioned with what this process routed.

### Initialization is a Skill

Like GitHub, Gitee patterns no longer inject a `template`. The agent
initializes its own topic directory by following a skill. Two ready-made
skills ship in `skills/`; copy them into `{workdir}/skills/`:

| skill | job |
|---|---|
| `gitee-init` | Clones the repository **into the topic directory itself** (plain `git clone`, no `repo/` subdirectory), and excludes framework files via `.git/info/exclude`. |
| `gitee-planner` | The planner role, ported from `templates/gitee-planner/AGENTS.md`; delegates setup to `gitee-init`. |
| `gitee-developer` | The developer role, ported from `templates/gitee-developer/AGENTS.md`; delegates setup to `gitee-init`. |

> **Note:** no `gitee-reviewer` skill ships yet — only `gitee-init`, `gitee-planner`,
> and `gitee-developer` are ported. A reviewer pattern still creates its topic, but
> the agent has no reviewer instructions until it is ported from
> `templates/gitee-reviewer/AGENTS.md` (same conversion as the github reviewer skill).

## Differences from GitHub Channel

| Feature | GitHub | Gitee |
|---------|--------|-------|
| CLI Tool | `gh` | `curl` + `jq` |
| PR Reviews | `gh pr review` | Comment-based (no formal review API) |
| Label Management | `gh pr edit --add-label` | API via `curl` |
| Draft PRs | `gh pr create --draft` | Not supported (empty PR + labels) |
| Issue/PR number spaces | Shared | Separate (per-type `is_pr` flag) |

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
