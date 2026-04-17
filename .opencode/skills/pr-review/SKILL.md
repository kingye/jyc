---
name: pr-review
description: |
  Review pull requests. Read-only analysis — does NOT modify code, build, deploy, or run tests.
  Use when: review PR, code review, check branch changes.
---

## PR Review — STRICTLY READ-ONLY

CRITICAL: This skill is strictly read-only.
- Do NOT edit, create, or delete any files in the repository
- Do NOT make commits or push changes
- Do NOT run builds or tests
- Do NOT deploy anything
- Do NOT fix issues — only describe them and suggest fixes in comments
- ONLY read, analyze, and post review comments

IMPORTANT: All `gh` and `git` commands MUST be run from inside the repository directory.
Use `cd repo && <command>` for every command.

### Step 0: Ensure Repository

```bash
# Clone repo if not present (use the repository from the trigger message)
if [ ! -d repo ]; then gh repo clone <owner>/<repo> repo; fi

# Fetch latest
cd repo && git fetch origin
```

NOTE: `gh` CLI is pre-configured and authenticated. Do NOT run `gh auth login`,
`gh auth refresh`, or any other auth commands. Just use `gh` directly.

### Step 1: Understand Project Conventions

Before reviewing, read the project's own documentation to understand its standards:

```bash
cd repo
# Read coding conventions (check whichever exist)
cat AGENTS.md 2>/dev/null || cat CLAUDE.md 2>/dev/null || true
cat README.md 2>/dev/null | head -100 || true
# Check for project-specific skills or instructions
ls .opencode/skills/ 2>/dev/null || ls .claude/ 2>/dev/null || true
```

Use the conventions found in these files as the basis for your review.
If no project-specific conventions are found, use general best practices.

### Step 2: Fetch PR Information

**With gh:**
```bash
cd repo && gh pr view <number> --json title,body,state,commits,files
cd repo && gh pr diff <number>
```

**Without gh:**
```bash
cd repo && git log --oneline main..<branch>
cd repo && git diff main..<branch> --stat
cd repo && git diff main..<branch>
```

### Step 3: Review Against Project Standards

**Project-specific conventions** (from AGENTS.md / CLAUDE.md / README.md):
- Apply whatever coding conventions, error handling patterns, logging rules,
  and documentation requirements the project defines
- If the project has a DESIGN.md, check that changes align with the architecture

**General code quality** (always apply):
- New functionality should have tests
- No secrets in code (API keys, passwords, tokens)
- No path traversal vulnerabilities in user input handling
- Consistent naming conventions
- Dead code cleaned up

**Documentation:**
- CHANGELOG.md updated for user-facing changes
- DESIGN.md updated for architecture changes (if the project has one)

### Step 4: Format Findings

Categorize each finding by severity:
- **Critical**: security issues, data loss, crashes
- **High**: design principle violations, missing error handling, broken functionality
- **Medium**: missing tests, inconsistent naming, dead code
- **Low**: documentation gaps, style suggestions

For each finding:
```
**[SEVERITY]** `file:line` — description
Suggestion: how to fix
```

End with overall verdict:
- **Approve**: no critical or high issues
- **Request Changes**: critical or high issues found
- **Comment**: only medium/low issues, approve with suggestions

### Step 5: Post Review

**With gh (preferred) — run from inside repo/ directory:**

```bash
cd repo && gh pr review <number> --approve --body "$(cat <<'EOF'
## PR Review

<findings>

**Verdict: Approve**
EOF
)"
```

Use `--request-changes` for critical/high issues:
```bash
cd repo && gh pr review <number> --request-changes --body "$(cat <<'EOF'
## PR Review

<findings>

**Verdict: Request Changes**
EOF
)"
```

Use `--comment` for medium/low only:
```bash
cd repo && gh pr review <number> --comment --body "$(cat <<'EOF'
## PR Review

<findings>

**Verdict: Comment — approve with suggestions**
EOF
)"
```

**Without gh:**
Output the full review as text for the user to post manually.
