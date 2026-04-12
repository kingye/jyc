---
name: dev-workflow
description: |
  Development workflow for jyc - branching, fix/feature flow, release process, version bump, commit conventions.
  Use when: planning development work, creating branches, merging, preparing releases, bumping version, updating changelog.
---

## Development Flow: Trunk-Based with Short-Lived Branches

main is always deployable. No release branches. Tag main directly for releases.

```
main ──●──●──●──●──●──●──●──●──● (always deployable)
        \   /   \     /
    fix/x  feat/a  feat/b
   (hours)  (1-2 days)
```

## For Fixes

1. `git checkout -b fix/<description>` from main
2. Fix the issue
3. Run tests: `cargo test`
4. Build clean: `cargo build --release` (zero warnings)
5. Commit with `fix:` prefix
6. Merge to main: `git checkout main && git merge fix/<name> --no-ff`
7. Push: `git push origin main`
8. Deploy if needed

## For Features

1. `git checkout -b feat/<description>` from main
2. Develop in small increments, commit frequently
3. If main has new commits, rebase: `git rebase main`
4. Run tests and build clean before merge
5. Merge to main: `git checkout main && git merge feat/<name> --no-ff`
6. Push

## For Releases (Version Bump)

### Step 1: Verify Prerequisites

```bash
cd jyc
git checkout main
git status  # Must be clean
LAST_TAG=$(git describe --tags --abbrev=0 2>/dev/null || echo "v0.0.0")
echo "Last tag: $LAST_TAG"
```

### Step 2: Review Changes Since Last Release

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

- Parse current version from `Cargo.toml`
- Bump PATCH for fixes and small features
- Bump MINOR for significant features or breaking changes
- Bump MAJOR for fundamental architecture changes

Present the suggested version to user for confirmation.

### Step 4: Update Files (TWO-PHASE CONFIRMATION required)

Phase 1 — Present plan:
- Show current version, new version, and all changes to be documented
- List files to modify: `Cargo.toml`, `CHANGELOG.md`, optionally `DESIGN.md`

Phase 2 — After user confirms, execute ALL steps in sequence:

1. Update `Cargo.toml` version field
2. Update `CHANGELOG.md`:
   - Add new version section at top (after header)
   - Group changes: Added, Fixed, Changed, Removed
   - Include date: `## [X.Y.Z] - YYYY-MM-DD`
3. Update `DESIGN.md` if architecture changed
4. Run `cargo test` to verify
5. Commit ALL changes: `chore: prepare release vX.Y.Z`
6. Tag: `git tag -a vX.Y.Z -m "vX.Y.Z: summary of key changes"`
7. Push with tags: `git push origin main --tags`

IMPORTANT: Steps 5-7 (commit, tag, push) are part of the release process.
Do NOT stop after updating files — complete all steps through push.

## Critical Rules

- Fixes ALWAYS go to main first — never fix on a feature branch
- Feature branches rebase on main regularly — don't let them diverge
- Keep feature branches short (1-2 days) — break large features into smaller merges
- Every commit on main must build with zero warnings
- Run `cargo test` before every merge to main
- Never force-push to main
- NEVER run `git config user.name` or `git config user.email`

## GitHub CLI (gh)

Use `gh` for ALL GitHub operations. Do NOT use `webfetch`, `curl`, or `wget` to access GitHub.
The `gh` CLI is pre-authenticated. Always run from inside the repo directory (`cd jyc`).

### Required Token Scopes

The GitHub PAT (Personal Access Token) must have these scopes:
- `repo` — full access to repositories (read/write code, issues, PRs)
- `read:org` — read organization membership (required for `gh pr view --comments`)

Setup: `gh auth login --with-token <<< "ghp_your_token"`

### Common Commands

```bash
# View PR details
cd jyc && gh pr view <number>

# View PR diff
cd jyc && gh pr diff <number>

# View PR review comments
cd jyc && gh pr view <number> --comments
cd jyc && gh api repos/{owner}/{repo}/pulls/<number>/reviews
cd jyc && gh api repos/{owner}/{repo}/pulls/<number>/comments

# Create PR
cd jyc && gh pr create --title "..." --body "..."

# List open PRs
cd jyc && gh pr list

# Post review comment
cd jyc && gh pr review <number> --comment --body "..."
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
