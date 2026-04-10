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
- Do NOT run builds (cargo build) or tests (cargo test)
- Do NOT deploy anything
- Do NOT fix issues — only describe them and suggest fixes in comments
- ONLY read, analyze, and post review comments

### Step 0: Check GitHub CLI

```bash
gh auth status 2>&1
```

If authenticated, use `gh` for PR info and posting comments.
If not, fall back to git commands and output review as text.

### Step 1: Fetch PR Information

**With gh:**
```bash
gh pr view <number> --json title,body,state,commits,files
gh pr diff <number>
```

**Without gh:**
```bash
git fetch origin
git log --oneline main..<branch>
git diff main..<branch> --stat
git diff main..<branch>
```

### Step 2: Review Against Project Standards

**Design Principles (see DESIGN.md):**
- Channel-agnostic: no channel-specific logic in core modules
- AI-agent-agnostic: no coupling to specific AI backend
- Error handling: `?` with `.context()`, no `.unwrap()` on fallible ops
- Logging: `tracing` only (never `println!`), appropriate log levels
- Public functions have doc comments

**Code Quality:**
- Zero compiler warnings expected
- New functionality should have tests
- No secrets in code (API keys, passwords, tokens)
- No path traversal vulnerabilities in user input handling
- Consistent naming conventions
- Dead code cleaned up

**Documentation:**
- CHANGELOG.md updated for user-facing changes
- DESIGN.md updated for architecture changes

### Step 3: Format Findings

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

### Step 4: Post Review

**With gh (preferred):**
```bash
gh pr review <number> --approve --body "$(cat <<'EOF'
## PR Review

<findings>

**Verdict: Approve**
EOF
)"
```

Use `--request-changes` for critical/high issues:
```bash
gh pr review <number> --request-changes --body "$(cat <<'EOF'
## PR Review

<findings>

**Verdict: Request Changes**
EOF
)"
```

Use `--comment` for medium/low only:
```bash
gh pr review <number> --comment --body "$(cat <<'EOF'
## PR Review

<findings>

**Verdict: Comment — approve with suggestions**
EOF
)"
```

**Without gh:**
Output the full review as text for the user to post manually.
