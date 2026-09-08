# GitHub Channel

The GitHub channel uses the GitHub REST API to:
- Poll for new issues, PRs, comments, reviews, and CI check-run results
- Relay agent replies as comments on issues/PRs
- Support label-based routing for planner/developer/reviewer roles

Like `gitee`, the GitHub channel owns **no** TopicManager, agent service,
outbound adapter, or orchestrator registration — all topics live in the pipe
target. Dedup/cursor state lives at `<workdir>/channels/<channel>/.github/`.
See `docs/architecture/overview.md` for the full pipe architecture.

## Configuration

```toml
[channels.my_repo]
type = "github"

[channels.my_repo.github]
owner = "myorg"
repo = "myrepo"
token = "${GITHUB_TOKEN}"             # PAT scopes: repo, read:user
poll_interval_secs = 60
# api_url = "https://api.github.com"  # Default. For GitHub Enterprise,
                                      # use https://github.example.com/api/v3
```

Every enabled GitHub pattern **must** declare a `pipe` target — matching
messages are dropped otherwise (warned at startup). The pipe target routes
into an agent channel (`channel`) or an agent topic (`agent`). Comments are
plain agent text via the reply forwarder, so `[channels.<name>.footer]` does
not apply.

## Polled Events

| Event | Notes |
|-------|-------|
| Issues / PRs opened | New items in the repo |
| Issue & PR comments | Skipped for closed items and for the bot's own role-tagged echoes (see Reply Relaying) |
| PR reviews & review comments | Polled only for open PRs with an **active topic** |
| CI check runs | Polled per open PR's head SHA (active topics only); a `check_run` event fires once all runs complete and the status transitions to failure/timed_out, listing the failed checks |
| Closes | Issue/PR closed → close event (see Close Events) |

Events are deduplicated by stable UIDs plus an on-disk cursor, so restarts
never re-fire old items.

## Multi-Agent Workflow

### Required Labels

Create these labels in your GitHub repository before using the workflow:

| Label | Purpose |
|-------|---------|
| `feature-plan` | Routes an issue to the High-Level Planner instead of the detail Planner |
| `ready-for-dev` | Triggers the developer agent |
| `ready-for-review` | Triggers the reviewer agent |

### Pattern Configuration

```toml
# Pattern: Issues with 'feature-plan' label → High-Level Planner
[[channels.my_repo.patterns]]
name = "high-level-planner"
enabled = true
role = "High-Level Planner"
rules = { github_type = ["issue"], labels = ["feature-plan"] }
pipe = { agent = "jyc_git", topic = "plan-${msg.issue_number}" }

# Pattern: Issues without 'feature-plan' → detail Planner
# exclude_labels keeps this silent while High-Level Planner is active.
[[channels.my_repo.patterns]]
name = "planner"
enabled = true
role = "Planner"
rules = { github_type = ["issue"], exclude_labels = ["feature-plan"] }
pipe = { agent = "jyc_git", topic = "plan-${msg.issue_number}" }

# Pattern: PRs with 'ready-for-dev' → Developer (also receives CI failures)
[[channels.my_repo.patterns]]
name = "developer"
enabled = true
role = "Developer"
rules = { github_type = ["pull_request"], labels = ["ready-for-dev"] }
pipe = { agent = "jyc_git", topic = "dev-${msg.pr_number}" }

# Pattern: PRs with 'ready-for-review' → Reviewer
[[channels.my_repo.patterns]]
name = "reviewer"
enabled = true
role = "Reviewer"
rules = { github_type = ["pull_request"], labels = ["ready-for-review"] }
pipe = { agent = "jyc_git", topic = "review-pr-${msg.pr_number}" }
```

Rule keys: `github_type`, `labels` (flat list = OR, nested lists = AND of
ORs), `exclude_labels`, `assignees` (OR). Every pattern must declare a `role`
— patterns without one never match. Reviewer-role patterns are evaluated
first (leftover developer-phase labels can't shadow a `ready-for-review` PR);
within each group, config order is preserved. The first match wins.

### Topic Placeholders

GitHub messages populate `repo`, `github_number`, `github_type`
(`issue` / `pull_request`), `github_action`, `github_labels`,
`github_assignees`, plus `pr_number` **or** `issue_number` — type-gated, so a
PR event carries only `pr_number` and an issue event only `issue_number`.

Pattern-level `topic_prefix` yields `{prefix}-{github_number}` topics;
without it the default is `{type}-{number}` (e.g. `pr-731`). The reviewer
role additionally falls back to a legacy `review-pr-{N}` name when no prefix
is configured (deprecated — a match-time warning asks you to set
`topic_prefix = "review-pr"` explicitly).

### Reply Relaying

The forwarder keeps a `topic → (number, role)` map recorded on inbound and
posts replies as comments on the issue/PR via the REST API. GitHub's issues
and PRs share **one number space**, so no is-PR lookup is needed (unlike
Gitee). The `[Role]` prefix is always added if the reply lacks it — it is
also how the poller recognizes the bot's own comments: a pattern skips
same-role echoes (a `[Planner]` comment never re-triggers the Planner) while
cross-role handoffs like `[Reviewer]` → developer still flow. Plain comments
without a known role prefix always route normally. No footer; comments carry
no attachments.

### Close Events

When an issue/PR is closed, the hub-side close handling resolves the topics
mapped from pattern `topic` templates and the routing state and closes them.

### Initialization is a Skill

Topic initialization is a **skill** (`github-init`, `github-planner`,
`github-developer` — bundled under `skills/`, copy them to your runtime
`{workdir}/skills/`), not a per-pattern template. The `github-reviewer`
skill is **not bundled yet**; until it is ported, a reviewer topic is created
but the agent has no reviewer instructions. `github-init` clones the
repository into the topic directory itself, so the topic *is* the checkout.

The agent side additionally needs the **`gh` CLI** installed and
authenticated (PR creation, comments, label edits, `gh pr review`, CI logs)
plus working `git` credentials — the workflow skills drive everything beyond
what the channel polls.

## Limitations

- **Polling only** — there is no webhook listener; expect up to
  `poll_interval_secs` of delivery latency.
- **Rate limits** — authenticated REST budget (~5000 req/h) is consumed per
  poll cycle across open items; very large repos may want a longer interval.
- **One repo per channel instance** — declare one `[channels.<name>]` block per
  repository.
