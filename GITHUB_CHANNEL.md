# GitHub Channel

GitHub Issue/PR channel for JYC — enables multi-agent workflows on GitHub
repositories through issue discussion, PR development, and code review.

## Design Principles

1. **Channel = Lightweight Trigger + Router** — Channel only polls events and
   routes them. Agents use `gh` CLI to read/write actual content.
2. **Label-Based Routing** — Routing is purely label-driven:
   - **Labels** (`jyc:plan`, `jyc:develop`, `jyc:review`) for routing.
     Agents add labels when creating PRs or handing off. Auto-labels are derived
     from the pattern's `role` field (e.g., `role = "Developer"` → `jyc:develop`).
   - **`@jyc:<role>` markers** in comments are informational only and do NOT affect routing.
3. **One Token, Role Prefix + Self-Loop Prevention** — Single GitHub PAT. Agents
   prefix comments with `[Planner]`, `[Developer]`, `[Reviewer]`. Each pattern
   only skips comments from its **own** role (self-loop prevention), but allows
   comments from other roles through for cross-agent visibility.
4. **Independent Threads** — Each agent role gets its own thread with separate
   repo clone, AGENTS.md, and context.
5. **Immediate Close + Delete** — When issue/PR closes, thread is immediately
   terminated and directory deleted.

## Architecture

```
GitHub API (polling)
    ↓
┌─────────────────────────────────────────────────────────────────┐
│ GitHub InboundAdapter                                           │
│                                                                 │
│ poll_events() — every N seconds                                 │
│   ├─ GET /repos/{owner}/{repo}/issues?state=open&since=...     │
│   ├─ GET /repos/{owner}/{repo}/issues/comments?since=...       │
│   └─ GET /repos/{owner}/{repo}/pulls/{n}/reviews?since=...     │
│                                                                 │
│ classify_event()                                                │
│   ├─ Determine event type (issue/pr/comment/review/close)      │
│   ├─ Skip bot's own comments                                   │
│   ├─ Parse slash commands (/develop, /review, etc.)             │
│   └─ Build minimal InboundMessage (trigger only, no content)   │
│                                                                 │
│ Route via ChannelMatcher                                        │
│   ├─ Match by github_type + labels                             │
│   └─ Derive thread name: issue-{N}, pr-{N}, review-pr-{N}     │
└─────────────────────────────────────────────────────────────────┘
    ↓
┌─────────────────────────────────────────────────────────────────┐
│ ThreadManager (existing)                                        │
│   ├─ Thread: issue-42  (Planner agent)                         │
│   ├─ Thread: pr-43     (Developer agent)                       │
│   └─ Thread: review-pr-43 (Reviewer agent)                     │
└─────────────────────────────────────────────────────────────────┘
    ↓
┌─────────────────────────────────────────────────────────────────┐
│ Agent (OpenCode + gh CLI)                                       │
│   ├─ Reads content: gh issue view, gh pr view, gh pr diff      │
│   ├─ Writes: gh issue comment, gh pr comment, gh pr review     │
│   ├─ Labels: gh issue edit --add-label, gh label create        │
│   └─ Code: git clone, git commit, git push                    │
└─────────────────────────────────────────────────────────────────┘
    ↓
┌─────────────────────────────────────────────────────────────────┐
│ GitHub OutboundAdapter                                          │
│   └─ jyc_reply_reply_message → POST comment on issue/PR       │
└─────────────────────────────────────────────────────────────────┘
```

## InboundMessage Design (Minimal Trigger)

The InboundMessage contains **only trigger metadata**. Agents use `gh` CLI
to read actual content. This avoids content parsing errors that plagued the
previous GitHub channel implementation.

```
InboundMessage {
    channel: "my_repo",                          // from config
    channel_uid: "issue-42-comment-12345",       // unique event ID for dedup
    sender: "user1",                             // GitHub username
    sender_address: "user1",
    topic: "#42 Add dark mode support",          // #{number} + title
    content: MessageContent {
        text: "github event: issue_comment\n
               number: 42\n
               type: issue\n
               action: created\n
               actor: user1\n
               labels: planning, ready-for-dev\n
               \n
               Use `gh issue view 42` to read the full issue.\n
               Use `gh issue view 42 --comments` to read all comments."
    },
    metadata: {
        "github_event": "issue_comment",         // event type
        "github_number": 42,                     // issue/PR number
        "github_type": "issue",                  // "issue" | "pull_request"
        "github_action": "created",              // "opened"|"created"|"labeled"|...
        "github_labels": ["planning"],           // current labels
        "github_linked_issue": null,             // for PRs: linked issue number
    }
}
```

## Event Classification

### Events to Track

| Source | API Endpoint | Events |
|--------|-------------|--------|
| Issues | `GET /repos/{o}/{r}/issues?state=open&since=...` | opened, labeled |
| Comments | `GET /repos/{o}/{r}/issues/comments?since=...` | created (on issues + PRs) |
| Reviews | `GET /repos/{o}/{r}/pulls/{n}/reviews?since=...` | submitted |
| Close | `GET /repos/{o}/{r}/issues?state=closed&since=...` | closed, merged |

### Event → Thread Routing

| Event | Condition | Thread | Action |
|-------|-----------|--------|--------|
| issue.opened | Labels match pattern | `issue-{N}` | Create thread, trigger agent |
| issue.commented | Not self-loop | `issue-{N}` | Trigger agent |
| issue.labeled | New labels match pattern | `issue-{N}` | Trigger agent (re-route) |
| issue.closed | — | `issue-{N}` | **Close + delete thread** |
| pr.opened | Labels match pattern | `pr-{N}` | Create thread, trigger agent |
| pr.commented | Not self-loop | `pr-{N}` | Trigger agent |
| pr.labeled | New labels match pattern | `pr-{N}` | Trigger agent (re-route) |
| pr.review_submitted | — | `pr-{N}` | Trigger developer agent |
| pr.merged | — | `pr-{N}`, `review-pr-{N}`, linked `issue-{N}` | **Close + delete all** |
| pr.closed (not merged) | — | `pr-{N}`, `review-pr-{N}` | **Close + delete** (keep issue thread) |

**Label change detection**: On each poll cycle, the adapter compares the current
labels on each issue/PR against the previously cached labels. If new labels were
added, a `labeled` event is generated and routed through normal pattern matching.
This allows users to add labels (e.g., `jyc:plan`) to existing issues and have
them routed to the correct agent.

### Self-Loop Prevention (Comment Filtering)

Agent comments are identified by the `[Role]` prefix (e.g., `[Developer]`, `[Reviewer]`).
Instead of globally filtering all agent comments, each pattern only skips comments
from its **own** role. This enables cross-agent visibility:

- `[Developer]` comment → **skipped** by developer pattern, **visible** to reviewer pattern
- `[Reviewer]` comment → **skipped** by reviewer pattern, **visible** to developer pattern
- Human comments (no prefix) → visible to all patterns

The `comment_role` is extracted from the prefix and stored in message metadata.
During pattern matching, if `comment_role` matches the pattern's `role`, the
pattern is skipped (self-loop prevention).

## Pattern Matching

### PatternRules Fields (GitHub-specific)

```rust
pub struct PatternRules {
    // ... existing fields (sender, subject, etc.) ...

    // GitHub-specific
    pub github_type: Option<Vec<String>>,    // ["issue"] or ["pull_request"]
    pub labels: Option<Vec<String>>,          // ["bug", "custom-label"]
}
```

### Auto-Label from Role

Each pattern with a `role` field and `github_type = ["pull_request"]` gets an
implicit routing label derived from the role name:

| Role | Auto-Label | Applies to |
|------|-----------|------------|
| `Planner` | `jyc:plan` | **PR only** (not applied to issue patterns) |
| `Developer` | `jyc:develop` | **PR only** |
| `Reviewer` | `jyc:review` | **PR only** |

**Issue patterns do not get auto-labels.** Issues are created by users who may
not add routing labels. If you want to filter issues by label, use the explicit
`labels` field in the pattern config.

The auto-label is combined (OR) with any explicit `labels` in the pattern config.
A pattern matches if ANY of the effective labels (explicit + auto-label) is present
on the issue/PR.

**Match logic:**
- `github_type` + labels: AND across fields, OR within each field
- Labels = explicit `labels` config + auto-label from `role` (OR logic)
- Self-loop check: skip if comment is from the pattern's own role

### Routing

Routing is purely label-driven. The `@jyc:<role>` mentions in comments are
informational (for human readability) and do NOT affect routing.

Matching logic: `github_type` + labels + self-loop check

### Configuration Example

```toml
[channels.my_repo]
type = "github"

[channels.my_repo.github]
owner = "kingye"
repo = "jyc"
token = "${GITHUB_TOKEN}"
poll_interval_secs = 60

# Pattern 1: Issues with jyc:plan label → Planner
[[channels.my_repo.patterns]]
name = "planner"
role = "Planner"
enabled = true
template = "github-planner"

[channels.my_repo.patterns.rules]
github_type = ["issue"]
# Auto-label "jyc:plan" is implicit from role = "Planner"

# Pattern 2: PRs with jyc:develop label → Developer
[[channels.my_repo.patterns]]
name = "developer"
role = "Developer"
enabled = true
template = "github-developer"
live_injection = false

[channels.my_repo.patterns.rules]
github_type = ["pull_request"]
# Auto-label "jyc:develop" is implicit from role = "Developer"

# Pattern 3: PRs with jyc:review label → Reviewer
[[channels.my_repo.patterns]]
name = "reviewer"
role = "Reviewer"
enabled = true
template = "github-reviewer"

[channels.my_repo.patterns.rules]
github_type = ["pull_request"]
# Auto-label "jyc:review" is implicit from role = "Reviewer"
```

## Thread Naming

| Event Type | Thread Name | Example |
|-----------|------------|---------|
| Issue | `issue-{number}` | `issue-42` |
| Pull Request (developer) | `pr-{number}` | `pr-43` |
| Pull Request (reviewer) | `review-pr-{number}` | `review-pr-43` |

Each thread gets its own directory:
```
<workdir>/<channel_name>/workspace/
  issue-42/           ← Planner agent
    .jyc/
    chat_history_*.md
    jyc/              ← Repo clone (read-only for planner)
    AGENTS.md         ← Planner role definition
  pr-43/              ← Developer agent
    .jyc/
    chat_history_*.md
    jyc/              ← Repo clone (developer works here)
    AGENTS.md         ← Developer role definition
  review-pr-43/       ← Reviewer agent
    .jyc/
    chat_history_*.md
    jyc/              ← Repo clone (read-only for reviewer)
    AGENTS.md         ← Reviewer role definition
```

## Routing Labels

Agents manage routing labels via `gh` CLI when handing off work.
Labels are a fixed convention hardcoded in agent templates.

### Predefined Labels

| Label | Purpose | Added By |
|-------|---------|----------|
| `jyc:plan` | Issue needs planning | User (or auto from config) |
| `jyc:develop` | PR needs development | Planner (when creating PR) |
| `jyc:review` | PR needs code review | Developer (when done implementing) |

### Label Usage by Agents

```bash
# Planner: create PR with develop label
gh pr create --title "feat: ..." --label "jyc:develop" --body "..."

# Developer: add review label when done
gh pr edit 43 --add-label "jyc:review"

# Reviewer: re-add develop label when requesting changes
gh pr edit 43 --add-label "jyc:develop"
```

## Agent Roles & Skills

### Agent A: Planner (github-planner)

**Thread**: `issue-{N}`
**Role**: Discuss requirements with user, create PR with spec when ready.

**Workflow**:
1. Triggered by issue (planner pattern matches issues)
2. Read issue: `gh issue view {N}`
3. Read comments: `gh issue view {N} --comments`
4. Discuss with user (reply via jyc_reply → posts issue comment)
5. When requirements clear:
   - Create branch: `git checkout -b feat/issue-{N}`
   - Create PR with label: `gh pr create --label "jyc:develop" --body "...@jyc:developer"`
6. Continue monitoring issue for user feedback

### Agent B: Developer (github-developer)

**Thread**: `pr-{N}`
**Role**: Implement code based on PR spec, address review feedback.

**Workflow**:
1. Triggered by PR with `jyc:develop` label
2. Read PR spec: `gh pr view {N}`
3. Read linked issue: `gh issue view {linked_issue}`
4. Clone repo, checkout PR branch
5. Implement code (incremental-dev approach)
6. Commit, push
7. Hand over: remove `jyc:develop` label, add `jyc:review` label
8. When review feedback received (`[Reviewer]` comment on PR with `jyc:develop` label):
   - Read reviews: `gh pr view {N} --comments`
   - Fix issues, commit, push
   - Hand over: remove `jyc:develop` label, add `jyc:review` label

### Agent C: Reviewer (github-reviewer)

**Thread**: `review-pr-{N}`
**Role**: Review PR code quality, approve or request changes.

**Workflow**:
1. Triggered by PR with `jyc:review` label
2. Read PR: `gh pr view {N}`
3. Read diff: `gh pr diff {N}`
4. Review code
5. Submit review: `gh pr review {N} --approve` or `--request-changes`
6. If changes requested: remove `jyc:review` label, add `jyc:develop` label

## Close & Cleanup

### Close Behavior

Threads are closed **immediately** with no agent notification. The entire
thread directory is deleted.

### Close Event Matrix

| Event | Threads Closed & Deleted |
|-------|--------------------------|
| Issue closed (manually) | `issue-{N}` |
| Issue closed (by PR merge "Fixes #N") | `issue-{N}` |
| PR merged | `pr-{N}` + `review-pr-{N}` + linked `issue-{N}` |
| PR closed (not merged) | `pr-{N}` + `review-pr-{N}` (keep `issue-{N}` open) |

### Close Flow

```
Close event detected
    ↓
1. Stop accepting new messages for thread (remove from queue map)
2. Wait for current message processing to finish (if any)
3. Delete thread directory: rm -rf workspace/{thread_name}/
4. Log: "Thread {thread_name} closed and deleted"
```

### PR Merge Cascade

When PR #43 is merged and its body contains "Fixes #42":

```
PR #43 merged
    ↓
├─ Delete workspace/pr-43/
├─ Delete workspace/review-pr-43/
└─ Delete workspace/issue-42/    (GitHub auto-closes issue #42)
```

## Outbound Adapter

### jyc_reply → GitHub Comment

When an agent uses `jyc_reply_reply_message`, the OutboundAdapter posts a
comment on the corresponding issue/PR.

```rust
async fn send_reply(&self, original, reply_text, ...) -> Result<SendResult> {
    let number = original.metadata["github_number"];
    // POST /repos/{owner}/{repo}/issues/{number}/comments
    // (GitHub API uses /issues/ endpoint for both issue and PR comments)
    let comment_body = format!("[{}] {}", agent_role, reply_text);
    github_client.create_comment(number, &comment_body).await
}
```

**Role prefix**: The OutboundAdapter reads the agent role from the thread's
template/config and prepends `[Planner]`, `[Developer]`, or `[Reviewer]`.
These prefixes are used for self-loop prevention: each pattern skips comments
from its own role but allows comments from other roles through.

### Direct gh CLI Operations

Agents can also interact with GitHub directly via `gh` CLI (not through
OutboundAdapter). This is used for:
- Cross-thread communication (planner commenting on PR)
- Creating PRs, branches
- Adding/removing labels
- Submitting reviews

## Polling Strategy

### Poll Cycle

```
Every poll_interval_secs:

1. Fetch open issues updated since last poll
   GET /repos/{o}/{r}/issues?state=open&since={last_poll}&sort=updated

2. Fetch comments since last poll
   GET /repos/{o}/{r}/issues/comments?since={last_poll}&sort=updated

3. Fetch recently closed issues/PRs (for close events)
   GET /repos/{o}/{r}/issues?state=closed&since={last_poll}&sort=updated

4. For each open PR with 'ready-for-review' label: fetch reviews
   GET /repos/{o}/{r}/pulls/{n}/reviews?since={last_poll}
```

### Deduplication

Each event gets a unique ID for dedup:
- Issue opened: `issue-{number}-opened`
- Comment: `comment-{comment_id}`
- Review: `review-{review_id}`
- Label change: `issue-{number}-labeled-{label}-{timestamp}`
- Close: `issue-{number}-closed-{timestamp}`

Processed event IDs are stored in a set (persisted to disk between restarts).

### Rate Limiting

GitHub API rate limit: 5000 requests/hour for PAT.
With 60s poll interval and 4 API calls per cycle: ~240 requests/hour.
Well within limits even with review fetching.

## Bot Identity

Single GitHub Personal Access Token (PAT) with scopes:
- `repo` — read/write issues, PRs, comments, labels
- `read:user` — fetch bot's own username for logging

All agents share the same token. Comments are prefixed with role for
identification and self-loop prevention:
```
[Planner] I have some questions about the requirements...
[Developer] Implementation complete. Ready for review.
[Reviewer] Code looks good overall. Two minor issues found.
```

Agent comments are **not** globally filtered. Instead, each pattern only skips
comments from its own role. A `[Developer]` comment is visible to the reviewer
pattern, and a `[Reviewer]` comment is visible to the developer pattern.

## Full Workflow Example

```
User1                    Agent A (Planner)        Agent B (Developer)      Agent C (Reviewer)
  │                       issue-42                  pr-43                   review-pr-43
  │                                │                      │                       │
  ├─ Creates Issue #42 ──────────►│                      │                       │
  │                                ├─ gh issue view 42    │                       │
  │                                ├─ Analyzes req        │                       │
  │  ◄── [Planner] Questions ─────┤                      │                       │
  │                                │                      │                       │
  ├─ Reply (answers) ────────────►│                      │                       │
  │                                │                      │                       │
  │                                ├─ Requirements clear   │                       │
  │                                ├─ git checkout -b feat/issue-42               │
  │                                ├─ gh pr create --label "jyc:develop"          │
  │                                │                      │                       │
  │                                │   [poll: PR+label] ──►│                      │
  │                                │                      ├─ gh pr view 43        │
  │                                │                      ├─ gh issue view 42     │
  │                                │                      ├─ Implement code       │
  │                                │                      ├─ git commit + push    │
  │                                │                      ├─ remove jyc:develop   │
  │                                │                      ├─ add jyc:review       │
  │                                │                      │                       │
  │                                │                      │   [poll: label] ─────►│
  │                                │                      │                       ├─ gh pr view 43
  │                                │                      │                       ├─ gh pr diff 43
  │                                │                      │                       ├─ Review code
  │                                │                      │                       ├─ gh pr review 43
  │                                │                      │                       │   --request-changes
  │                                │                      │                       ├─ remove jyc:review
  │                                │                      │                       ├─ add jyc:develop
  │                                │                      │                       │
  │                                │                      │◄── [poll: label] ─────┤
  │                                │                      ├─ gh pr view 43 --comments
  │                                │                      ├─ Fix code             │
  │                                │                      ├─ git push             │
  │                                │                      ├─ remove jyc:develop   │
  │                                │                      ├─ add jyc:review       │
  │                                │                      │                       │
  │                                │                      │   [poll: label] ─────►│
  │                                │                      │                       ├─ gh pr diff 43
  │                                │                      │                       ├─ gh pr review 43
  │                                │                      │                       │   --approve
  │                                │                      │                       │
  │  User merges PR #43            │                      │                       │
  │                                │                      │                       │
  │  [poll: pr.merged] ───────────►│ CLOSE + DELETE ──────►│ CLOSE + DELETE ──────►│ CLOSE + DELETE
  │                                                                                
```

## Implementation Phases

Each phase has a **clear test objective**. Only proceed to next phase after
the current phase passes human testing.

### Phase 1: Skeleton — Config + Build

**Goal**: GitHub channel compiles, loads config, does nothing yet.

**Implement**:
- `src/channels/github/mod.rs` — module declaration
- `src/channels/github/config.rs` — `GithubConfig` struct
- `src/config/types.rs` — add `github: Option<GithubConfig>` to `ChannelConfig`
- `src/channels/types.rs` — add `github_type`, `labels` to `PatternRules`
- `src/channels/github/inbound.rs` — stub `GitHubMatcher` + `GitHubInboundAdapter`
- `src/channels/github/outbound.rs` — stub `GitHubOutboundAdapter`
- `src/cli/monitor.rs` — wire up GitHub channel (disabled by default)

**Test**: `cargo test` passes. `cargo build --release` passes. No runtime behavior.

### Phase 2: Polling — Fetch Events

**Goal**: Channel connects to GitHub API, polls events, logs them.

**Implement**:
- `src/channels/github/client.rs` — GitHub API client (issues, comments, user)
- `GitHubInboundAdapter::start()` — polling loop
- Bot identity: `GET /user` at startup
- Fetch issues + comments since last poll
- Log each event to tracing (no routing yet)

**Test**: Start JYC with GitHub channel config. Verify logs show:
- Bot username identified
- Issues fetched
- Comments fetched
- Bot's own comments skipped
- Create a comment on a test issue → verify it appears in logs

### Phase 3: Routing — Events to Threads

**Goal**: Events create InboundMessages and route to threads.

**Implement**:
- Event classification (issue/PR/comment/review)
- Build minimal InboundMessage from event
- `GitHubMatcher::match_message()` — pattern matching
- `GitHubMatcher::derive_thread_name()` — thread naming
- Deduplication (skip already-processed events)

**Test**: Configure one pattern (issues → planner). Create issue on GitHub.
Verify:
- Thread `issue-42` is created
- Agent receives the trigger message
- Agent can use `gh issue view 42` to read content
- Comment on issue → agent is triggered again

### Phase 4: Outbound — Post Comments

**Goal**: Agent replies appear as GitHub comments.

**Implement**:
- `GitHubOutboundAdapter::send_reply()` — post comment via API
- Role prefix: `[Planner]`, `[Developer]`, `[Reviewer]`
- `client.rs` — `create_comment()` method

**Test**: Agent replies to an issue → verify comment appears on GitHub
with `[Planner]` prefix. Reply to a PR → verify comment appears.

### Phase 5: Labels + Slash Commands

**Goal**: Label-based routing works. Slash commands add labels.

**Implement**:
- Slash command parsing in InboundAdapter
- Auto-add labels when slash commands detected
- Label-based pattern matching in `GitHubMatcher`
- Multiple patterns: planner (issues), developer (PR + ready-for-dev)

**Test**:
- Create issue → planner agent discusses
- User comments `/develop` → label `ready-for-dev` added
- PR created with that label → developer thread created
- Agent B triggered

### Phase 6: Close + Cleanup

**Goal**: Closing issues/PRs deletes threads.

**Implement**:
- Close event detection in polling
- `on_thread_close` callback
- Directory deletion
- PR merge cascade (close PR + review + linked issue threads)

**Test**:
- Close an issue → verify thread directory deleted
- Merge a PR → verify PR, review, and linked issue threads deleted
- Close PR without merge → verify issue thread preserved

### Phase 7: Skills + Templates

**Goal**: Full multi-agent workflow with proper skills.

**Implement**:
- `github-planner` skill
- `github-developer` skill
- `github-reviewer` skill
- Agent templates (AGENTS.md for each role)

**Test**: Full end-to-end workflow:
1. Create issue → Planner discusses → creates PR
2. Developer implements → requests review
3. Reviewer reviews → requests changes
4. Developer fixes → Reviewer approves
5. Merge → all threads cleaned up

## References

- [DESIGN.md](DESIGN.md) — JYC architecture
- [IMPLEMENTATION.md](IMPLEMENTATION.md) — Implementation phases
- [CHANGELOG.md](CHANGELOG.md) — Version history
