---
name: dev-workflow
description: |
  Development workflow - branching, fix/feature flow, release process, version bump, commit conventions.
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

## For Fixes

1. `git checkout -b fix/<description>` from main
2. Fix the issue
3. Run tests: `{test_command}`
4. Build clean: `{build_command}` (zero warnings)
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
4. Run `{test_command}` to verify
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
- Run `{test_command}` before every merge to main
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
