---
name: dev-workflow
description: |
  Development workflow - branching, fix/feature flow, release process, version bump, commit conventions,
  delegating a review of your changes before push, and handing a slow wait (CI on a PR, a deploy) to a `ci-watch` subagent.
  Use when: planning development work, creating branches, merging, preparing releases, bumping version, updating changelog,
  review my changes, second opinion, independent review, delegate a review, check my work before committing, wait for CI, monitor a PR's checks.
---

## Development Flow: Trunk-Based with Short-Lived Branches

main is always deployable. No release branches. Tag main directly for releases.

```
main ──●──●──●──●──●──●──●──●──● (always deployable)
        \   /   \     /
    fix/x  feat/a  feat/b
   (hours)  (1-2 days)
```

## Detect Project Type and Conventions (do this ONCE at the start)

**Step 1: Read project documentation (if present)**

Check for and read these files in the repository root (in order of priority):
- `AGENTS.md` or `CLAUDE.md` — coding conventions, forbidden actions, toolchain
- `README.md` — build/test/run commands, prerequisites
- `.opencode/skills/` — skill files may define specific workflows and commands
- `.claude/` — may contain project-specific instructions

If these files specify build, test, or check commands, **use those** instead of
the defaults in Step 2.

**Step 2: Detect project type by config files (fallback)**

| Priority | Type | Detection | Default commands | Version file |
|----------|------|-----------|-----------------|-------------|
| 1 | SAP CDS | `.cdsrc.json` exists, OR `package.json` has `@sap/cds` | Read `scripts` from `package.json` | `package.json` |
| 2 | Rust | `Cargo.toml` exists | test: `cargo test` / build: `cargo build --release` | `Cargo.toml` |
| 3 | Node.js | `package.json` exists (no `@sap/cds`) | test: `npm test` / build: `npm run build` | `package.json` |

After detection, you have: `{test_command}`, `{build_command}`, `{version_file}`.

### Local vs CI — where validation runs

Read the project's own rules (`AGENTS.md` / `CLAUDE.md`, plus its CI workflow files) and
pick the cheapest tier that matches the risk of the change. Heavier tiers are not "safer",
they are only slower and they cost the machine.

| Tier | What | Run locally when |
|------|------|------------------|
| compile | `{check_command}` (e.g. `cargo check -p <crate>`) | always — this is the per-step gate |
| local gate | formatter + compile of test targets (e.g. `cargo fmt -- --check`, `cargo check -p <crate> --tests`) | always, before commit |
| suite | `{test_command}` / `{lint_command}` / coverage | only when nothing else runs them (no CI, or CI doesn't cover this) |

When CI owns the suite, the local obligation is: tests written and committed, compile clean,
pushed. CI is the one that executes them — and its result always has to come back, so every push
that starts a run also arranges the wait (below). What is forbidden is *you* holding your turn
open for it: no polling `gh pr checks` / `gh run watch`, no `sleep` retries, and no putting the
push or the reply off because a run is still going. Hand the link over and end the turn; red CI
is fixed in the turn its report arrives in.

Hand the wait off — **default: a `ci-watch` child**, which polls on the small model and reports
back as a message. Arrange one wait only, for the head you leave behind: if you are going to push
again in this turn, do not start a waiter that the next push will supersede.

1. `jyc_spawn_subagent(name="ci-watch-<pr>", type="ci-watch")` (`subagents/ci-watch/config.toml`;
   install once with `cp -r subagents/ci-watch <config dir>/subagents/`)
2. `jyc_send_to_topic(channel=…, topic=<child>, require_reply=true)` — give it only two things:
   the repo (**name it always**: a watcher that a `develop` child spawned works in that child's topic
   dir, which holds the checkout without being one, and `gh` there answers "not a git repository")
   and the PR or branch. The child does exactly one thing: `gh pr checks <pr> --repo <owner/name>
   --watch --interval 30`, with the deadline inside the command (`timeout 1500 gh pr checks … |
   tail -c 20000`, tool timeout 1560) so the pipe still prints the last table when its budget runs
   out. It reads the table, not the exit code — pass/fail/skipping are the terminal states, and
   "no checks reported" is not a pass. It never picks a run, a workflow, or a head, and never
   verifies SHAs; which head the PR carries is yours to know, not the child's.
3. End your turn; the report arrives as a new message in this topic. The child always reports back
   with `jyc_send_to_topic` to the channel/topic in the task's `Source:` header — whether or not you
   asked for a reply. On failure it names the failing check and appends the log tail from the run id
   in that row's URL.

The equivalent older way, when you would rather not hold a child open: `job_create` with a one-time
`at` at the end of the budget, whose prompt says to check that PR's checks and report. It re-enters
this topic as a new turn — one wake-up, cheaper, but an early failure is only known then. One waiter
per PR either way: if a job is already pending for this PR, replace it rather than adding a second,
and when you push again, continue the waiter you have (another task to a watcher is a new turn in it)
instead of spawning or scheduling a second one.

**A dead watch looks like nothing — until the budget is out.** Silence in this
topic once the watcher's ~30-minute budget has run out is a dead watch, not a
report: when you are next awake here, check that PR's checks yourself once, or send the
*same* watcher another task — a new message opens a new turn in it (a second
watcher on one PR is a duplicate the rules forbid; spawn a fresh one only if that
child is gone). A reply that instead *announces* a poll is the same failure seen early, and it
is visible only in the watcher's own topic — that turn ended, and nothing resumes
it. The role file states the invariant from its own side: every turn carries the
poll call or the report, never a promise to poll.

**What the compile tier proves — and what it does not.** `cargo check -p <crate> --tests` compiles
the test targets; it never runs them. Green means "the tests compile", not "the tests pass". Never
report or imply test results from a compile tier, in a step report, a commit message, or a PR body.

**What the gate costs, and what makes it spike.** A warm gate is cheap — seconds, even for a 28k-line
crate with thousands of lines of test code. It spikes to minutes when the dependency graph has been
invalidated: `check` artifacts are a different kind from `build` / `test` artifacts, so one local
`cargo test` evicts the check cache and the *next* gate re-pays for the whole graph — serially, if
`CARGO_BUILD_JOBS=1`. A test-target invocation also adds a second unit: in a bin-only crate the
whole crate is re-type-checked under the test cfg with all `#[cfg(test)]` code switched on. So run
the gate ONCE per change set, never after every edit, and make the test-file edits last. Slowness is
a cache-and-parallelism symptom, never a reason to escalate to the suite or a local build —
escalating is what caused it.

## Delegated Review: Before Push or Merge

An independent review needs three things: the tree as it is *now* (uncommitted changes, or the
commits you just made), a context that has not already decided the answer, and a reviewer that
cannot quietly fix what it finds. `jyc_spawn_subagent` gives all three in one call — the third
through the `review` type.

1. `jyc_spawn_subagent(name="review-<what>", type="review")` — the child works in your
   `topic_path` (the role declares `share_topic_path` itself, so you pass nothing), so it reads
   exactly what you have. Do **not** pass `inherit_context`: a review that starts with your
   reasoning in it is not a second opinion.
2. `jyc_send_to_topic(channel=…, topic=<child>, require_reply=true)` with the task. Name the
   question, where to start, and what the report looks like:

   > Review this branch against its base (`git diff <base>..HEAD`, plus `git status` for
   > anything still uncommitted). Read the surrounding code, not only the diff. Report
   > findings as: severity, file:line, what is wrong, and what you would do instead.

3. End your turn. The report arrives as a new message in this topic; a finding that holds
   becomes a follow-up commit on the same PR.

The role is `subagents/review/config.toml` (install once with
`cp -r subagents/review <config dir>/subagents/`): plan mode, the review skills and nothing
else, no `write`/`edit`, no delegation of its own, `bash` kept for `git status`/`git diff`/
`git log`.

Rules:

- **The type is the boundary, not your wording.** Say what to review; the role takes care of
  what it may do.
- **Sharpen the task, not the permissions.** If the review needs something the role forbids
  (running a test build, say), that is a different job with a different type — do not talk the
  reviewer out of its role.
- **One reviewer, one task.** Continue the same child with `jyc_send_to_topic` for follow-ups
  (it keeps its own context); a second reviewer on the same files buys nothing.
- Read the report, not the child's state dir — its `.jyc` is its own. Closing your topic
  closes the reviewer with it; while you still have it, close the child yourself (send it
  `/close`) once you want nothing more from it — a reviewer or a watcher that has reported is
  done.

For a change small enough to check yourself, skip this: the rules above are for non-trivial
changes — several files, changed logic, or work you could not test locally.

## For Fixes

1. `git checkout -b fix/<description>` from main
2. Fix the issue
3. Validate at the tier the project owns (see "Local vs CI" above) — for a non-trivial change,
   delegate the review first (see "Delegated Review" above)
4. Build clean: `{build_command}` (zero warnings). In *this* repo that means `cargo check -p
   <crate>` plus `cargo fmt -- --check` — AGENTS.md forbids the local suite (`cargo build` /
   `cargo test` / `clippy`); CI owns it
5. Commit with `fix:` prefix
6. Summarize work done and ask for user approval before merging to main
7. Merge to main: `git checkout main && git merge fix/<name> --no-ff`
8. Push: `git push origin main`
9. Deploy if needed

## For Features

1. `git checkout -b feat/<description>` from main
2. Develop in small increments, commit frequently
3. If main has new commits, rebase: `git rebase main`
4. Validate before merge per "Local vs CI" above; keep `{build_command}` clean. A non-trivial
   change gets a delegated review before it is pushed (see "Delegated Review" above)
5. Summarize work done and ask for user approval before merging to main
6. Merge to main: `git checkout main && git merge feat/<name> --no-ff`
7. Push

## For Releases (Version Bump)

### Step 1: Verify Prerequisites

```bash
git checkout main
git status  # Must be clean
LAST_TAG=$(git describe --tags --abbrev=0 2>/dev/null || echo "v0.0.0")
echo "Last tag: $LAST_TAG"
```

### Step 2: Read the Changes Since Last Release

```bash
git log ${LAST_TAG}..HEAD --oneline
git log ${LAST_TAG}..HEAD --pretty=format:"%s" | head -30
```

Categorize commits by type:
- `feat:` → Added section
- `fix:` → Fixed section
- `refactor:` / `chore:` → Changed section
- `docs:` → Documentation section

### Step 3: Determine Version Number

- Parse current version from `{version_file}`
  - Rust: `version = "X.Y.Z"` in `Cargo.toml`
  - Node/CDS: `"version": "X.Y.Z"` in `package.json`
- Bump PATCH for fixes and small features
- Bump MINOR for significant features or breaking changes
- Bump MAJOR for fundamental architecture changes

Present the suggested version to user for confirmation.

### Step 4: Update Files (TWO-PHASE CONFIRMATION required)

Phase 1 — Present plan:
- Show current version, new version, and all changes to be documented
- List files to modify: `{version_file}`, `CHANGELOG.md`, optionally `DESIGN.md`

Phase 2 — After user confirms, execute ALL steps in sequence:

1. Update `{version_file}` version field
2. Update `CHANGELOG.md`:
   - Add new version section at top (after header)
   - Group changes: Added, Fixed, Changed, Removed
   - Include date: `## [X.Y.Z] - YYYY-MM-DD`
3. Update `DESIGN.md` if architecture changed
4. Verify at the project's tier: local gate always; `{test_command}` locally only if CI does
   not gate the release (otherwise require CI green on the release commit)
5. Commit ALL changes: `chore: prepare release vX.Y.Z`
6. Tag: `git tag -a vX.Y.Z -m "vX.Y.Z: summary of key changes"`
7. Push with tags: `git push origin main --tags`

IMPORTANT: Steps 5-7 (commit, tag, push) are part of the release process.
Do NOT stop after updating files — complete all steps through push.

## Critical Rules

- Fixes get their own `fix/` branch off main — never smuggled into another feature's branch; a follow-up to an open PR goes on that PR's branch (AGENTS.md's PR rules)
- Feature branches rebase on main regularly — don't let them diverge
- Keep feature branches short (1-2 days) — break large features into smaller merges
- Every commit on main must build with zero warnings
- Before every merge to main the change must be validated at the project's tier:
  local gate always; the test suite by CI, or locally only when nothing else runs it
- Work only in the directory you were given. For a delegated developer that is a checkout the
  caller prepared (in this repo a `git worktree` made from the directory itself, and its
  `git diff` base comes from the task message). Never create or remove a checkout
  yourself: the caller does both, and before it closes you — a message to a closed child starts a
  different topic
- A delegated developer stops at the pushed PR, on the branch it was given — which already exists,
  so it has no branch to create. It does not merge to main and does not push main: the fix and
  feature flows above describe a developer working directly in the repository and merging with the
  user's approval, while what lands on main from a delegated branch is the caller's user's call
- Never force-push to main
- NEVER run `git config user.name` or `git config user.email`

## GitHub CLI (gh)

Use `gh` for ALL GitHub operations. Do NOT use `webfetch`, `curl`, or `wget` to access GitHub.
The `gh` CLI is pre-authenticated. Always run from inside the repo directory.

### Required Token Scopes

The GitHub PAT (Personal Access Token) must have these scopes:
- `repo` — full access to repositories (read/write code, issues, PRs)
- `read:org` — read organization membership (required for `gh pr view --comments`)

Setup: `gh auth login --with-token <<< "ghp_your_token"`

### Common Commands

```bash
# View PR details
gh pr view <number>

# View PR diff
gh pr diff <number>

# View PR review comments
gh pr view <number> --comments
gh api repos/{owner}/{repo}/pulls/<number>/reviews
gh api repos/{owner}/{repo}/pulls/<number>/comments

# Create PR
gh pr create --title "..." --body "..."

# List open PRs
gh pr list

# Post review comment
gh pr review <number> --comment --body "..."
```

Do NOT attempt to access GitHub via HTTP URLs — the repo may be private.

## Commit Message Convention

- `fix:` — bug fix
- `feat:` — new feature
- `chore:` — maintenance, cleanup, version bump
- `docs:` — documentation only
- `refactor:` — code restructuring without behavior change

## Version Numbering

- Format: `MAJOR.MINOR.PATCH` (e.g., 0.1.5)
- Bump PATCH for fixes and small features
- Bump MINOR for significant features or breaking changes
- Bump MAJOR for fundamental architecture changes

## CHANGELOG Format

```markdown
## [X.Y.Z] - YYYY-MM-DD

### Added
- **Feature name** — description

### Fixed
- **Bug name** — description

### Changed
- Description of change

### Removed
- Description of what was removed
```
