# GitHub Channel

GitHub Issue/PR channel for JYC — enables multi-agent workflows on GitHub
repositories through issue discussion, PR development, and code review.

## Design Principles

1. **Channel = Lightweight Trigger + Router** — Channel only polls events and
   routes them. Agents use `gh` CLI to read/write actual content.
2. **Label-Driven Hand-over** — Agents signal each other by adding labels.
   Channel routes based on label changes.
3. **One Token, Role Prefix** — Single GitHub PAT. Agents prefix comments
   with `[Planner]`, `[Developer]`, `[Reviewer]` to identify themselves.
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
| issue.opened | — | `issue-{N}` | Create thread, trigger agent |
| issue.commented | Not bot | `issue-{N}` | Trigger agent |
| issue.labeled | Label matches pattern | `issue-{N}` | Trigger agent |
| issue.closed | — | `issue-{N}` | **Close + delete thread** |
| pr.opened | — | `pr-{N}` | Create thread, trigger agent |
| pr.commented | Not bot | `pr-{N}` | Trigger agent |
| pr.labeled | Label matches pattern | `pr-{N}` | Trigger agent |
| pr.review_submitted | — | `pr-{N}` | Trigger developer agent |
| pr.merged | — | `pr-{N}`, `review-pr-{N}`, linked `issue-{N}` | **Close + delete all** |
| pr.closed (not merged) | — | `pr-{N}`, `review-pr-{N}` | **Close + delete** (keep issue thread) |

### Bot Comment Filtering

At startup, the channel fetches the authenticated user (`GET /user`) to get
the bot's GitHub username. All comments from this username are skipped during
polling to prevent infinite loops.

## Pattern Matching

### New PatternRules Fields

```rust
pub struct PatternRules {
    // ... existing fields (sender, subject, etc.) ...

    // GitHub-specific
    pub github_type: Option<Vec<String>>,    // ["issue"] or ["pull_request"]
    pub github_event: Option<Vec<String>>,   // ["opened", "labeled"]
    pub labels: Option<Vec<String>>,          // ["ready-for-dev", "bug"]
}
```

**Match logic (AND across fields, OR within each field):**
- `github_type: ["issue"]` → only match issue events, not PR events
- `labels: ["ready-for-dev", "bug"]` → match if ANY of these labels present
- Both set → must be issue AND have one of the labels

### Configuration Example

```toml
[channels.my_repo]
type = "github"

[channels.my_repo.github]
owner = "kingye"
repo = "jyc"
token = "${GITHUB_TOKEN}"
poll_interval_secs = 60

# Pattern 1: New issues → Planner
[[channels.my_repo.patterns]]
name = "planner"
enabled = true
template = "github-planner"

[channels.my_repo.patterns.rules]
github_type = ["issue"]

# Pattern 2: PRs labeled 'ready-for-dev' → Developer
[[channels.my_repo.patterns]]
name = "developer"
enabled = true
template = "github-developer"
live_injection = false

[channels.my_repo.patterns.rules]
github_type = ["pull_request"]
labels = ["ready-for-dev"]

# Pattern 3: PRs labeled 'ready-for-review' → Reviewer
[[channels.my_repo.patterns]]
name = "reviewer"
enabled = true
template = "github-reviewer"

[channels.my_repo.patterns.rules]
github_type = ["pull_request"]
labels = ["ready-for-review"]
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

## Slash Commands

Users and agents can use slash commands in GitHub comments. The channel
parses these and adds corresponding labels automatically.

| Command | Label Added | Effect |
|---------|-------------|--------|
| `/develop` | `ready-for-dev` | Trigger Developer agent |
| `/review` | `ready-for-review` | Trigger Reviewer agent |
| `/approve` | `approved` | Mark PR as approved |
| `/close` | — | Close issue/PR |

Slash commands are parsed by the InboundAdapter **before** routing. The
channel adds the label via GitHub API, which then triggers the appropriate
pattern match on the next poll cycle.

## Labels

Agents manage labels themselves via `gh` CLI. If a label doesn't exist,
the agent creates it.

### Predefined Labels

| Label | Color | Purpose | Added By |
|-------|-------|---------|----------|
| `ready-for-dev` | `#0E8A16` (green) | Planner finished, developer can start | Agent A |
| `ready-for-review` | `#1D76DB` (blue) | Developer finished, reviewer can start | Agent B |
| `changes-requested` | `#E4E669` (yellow) | Reviewer requested changes | Agent C |
| `approved` | `#0E8A16` (green) | Reviewer approved | Agent C |

### Label Creation by Agent

```bash
# In skill instructions:
gh label create "ready-for-dev" --description "Ready for development" --color "0E8A16" 2>/dev/null
gh issue edit 42 --add-label "ready-for-dev"
```

## Agent Roles & Skills

### Agent A: Planner (github-planner)

**Thread**: `issue-{N}`
**Role**: Discuss requirements with user, create PR with spec when ready.

**Workflow**:
1. Triggered by new issue or issue comment
2. Read issue: `gh issue view {N}`
3. Read comments: `gh issue view {N} --comments`
4. Discuss with user (reply via jyc_reply → posts issue comment)
5. When requirements clear:
   - Create branch: `git checkout -b feat/issue-{N}`
   - Create PR: `gh pr create --title "..." --body "spec..." `
   - Add label: `gh issue edit {N} --add-label "ready-for-dev"`
6. Continue monitoring issue for user feedback
7. Can comment on PR: `gh pr comment {PR_N} --body "[Planner] ..."`

### Agent B: Developer (github-developer)

**Thread**: `pr-{N}`
**Role**: Implement code based on PR spec, address review feedback.

**Workflow**:
1. Triggered by PR with `ready-for-dev` label
2. Read PR spec: `gh pr view {N}`
3. Read linked issue: `gh issue view {linked_issue}`
4. Clone repo, checkout PR branch
5. Implement code (incremental-dev approach)
6. Commit, push
7. Add label: `gh pr edit {N} --add-label "ready-for-review"`
8. When review feedback received:
   - Read reviews: `gh pr view {N} --comments`
   - Fix issues, commit, push
   - Re-request review: `gh pr edit {N} --add-label "ready-for-review"`

### Agent C: Reviewer (github-reviewer)

**Thread**: `review-pr-{N}`
**Role**: Review PR code quality, approve or request changes.

**Workflow**:
1. Triggered by PR with `ready-for-review` label
2. Read PR: `gh pr view {N}`
3. Read diff: `gh pr diff {N}`
4. Review code
5. Submit review: `gh pr review {N} --approve` or `gh pr review {N} --request-changes --body "..."`
6. If changes requested: `gh pr edit {N} --add-label "changes-requested"`

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
- `read:user` — fetch bot's own username for comment filtering

All agents share the same token. Comments are prefixed with role:
```
[Planner] I have some questions about the requirements...
[Developer] Implementation complete. Ready for review.
[Reviewer] Code looks good overall. Two minor issues found.
```

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
  │                                ├─ gh pr create         │                       │
  │                                ├─ gh issue edit 42 --add-label ready-for-dev  │
  │                                │                      │                       │
  │                                │   [poll: PR + label] ►│                      │
  │                                │                      ├─ gh pr view 43        │
  │                                │                      ├─ gh issue view 42     │
  │                                │                      ├─ Implement code       │
  │                                │                      ├─ git commit + push    │
  │                                │                      ├─ gh pr edit 43 --add-label ready-for-review
  │                                │                      │                       │
  │                                │                      │   [poll: label] ─────►│
  │                                │                      │                       ├─ gh pr view 43
  │                                │                      │                       ├─ gh pr diff 43
  │                                │                      │                       ├─ Review code
  │                                │                      │                       ├─ gh pr review 43
  │                                │                      │                       │   --request-changes
  │                                │                      │                       ├─ gh pr edit 43
  │                                │                      │                       │   --add-label changes-requested
  │                                │                      │                       │
  │                                │                      │◄─ [poll: review] ─────┤
  │                                │                      ├─ gh pr view 43 --comments
  │                                │                      ├─ Fix code             │
  │                                │                      ├─ git push             │
  │                                │                      ├─ gh pr edit 43 --add-label ready-for-review
  │                                │                      │                       │
  │                                │                      │   [poll: label] ─────►│
  │                                │                      │                       ├─ gh pr diff 43
  │                                │                      │                       ├─ gh pr review 43
  │                                │                      │                       │   --approve
  │                                │                      │                       │
  │                                │                      │                       │
  │  User merges PR #43            │                      │                       │
  │  (or bot merges if approved)   │                      │                       │
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
