---
name: pr-review
description: |
  Review pull requests. Read-only analysis — does NOT modify code, build, deploy, or run tests.
  Use when: review PR, code review, check branch changes.
---

## PR Review — Strictly Read-Only Review Methodology

### Constraints
- Do NOT edit, create, or delete any files in the repository
- Do NOT make commits or push changes
- Do NOT run builds or tests
- Do NOT deploy anything
- Do NOT fix issues — only describe them and suggest fixes in comments
- ONLY read, analyze, and post review comments

### Trust but Verify — BLOCKING Rule 🔴

**Do NOT trust the developer's completion summary in PR comments.** You MUST:
1. Read the full code diff (every changed file, every changed line)
2. Check each claim in the developer's comment — is it actually implemented in the code?
3. If a claim is not verifiable from the diff, flag it as **Critical** severity
4. Do NOT approve if the diff does not match the developer's stated work
5. **Test evidence must come from an actual test run** (CI). "Compiles with `cargo check --tests`"
   or "the local gate is green" proves the tests compile, not that they pass — flag the claim
   rather than accepting it

This rule is **BLOCKING** — violating it means approving changes you haven't verified, which defeats the purpose of review.

### Review Checklist

Evaluate every change against the following criteria:

1. **Correctness**: Does the code do what the spec/PR description says? Does it actually fix the issue?
2. **Design**: Is the approach reasonable? Are there simpler, more maintainable alternatives?
3. **Code quality**: Readability, naming, error handling, consistent style with the surrounding codebase
4. **Tests**: Are there tests for the changes? Do they actually test the behavior? Are edge cases covered?
5. **Edge cases**: Missing error handling, boundary conditions, null/empty inputs, concurrent access
6. **Project conventions**: Does the code follow the project's own rules from AGENTS.md / CLAUDE.md / README.md?
7. **Coding principles** (check against `coding-principles` skill):
   - **Simplicity First (P2)**: Flag overcomplication — code beyond what was asked, unnecessary abstractions
   - **Surgical Changes (P3)**: Flag unnecessary changes — "improvements" to unrelated code, style changes
   - **Goal-Driven Execution (P4)**: Check goal traceability — every changed line should trace to user's request
   - **Single Point of Responsibility (P5)**: Flag a responsibility owned twice — a second place that re-derives, re-implements or re-decides what an owner already decides
   - **Reuse Before Adding (P6)**: Flag new code that duplicates an existing implementation instead of calling it
   - **No Overdesign (P7)**: Flag abstractions that don't earn themselves — a trait with one implementor, a struct mirroring another, a wrapper that adds nothing. P7 is P2 sharpened: report the finding once, under P7.
   - **Growth in functional code**: Count the diff's *functional* lines — excluding comments, doc comments, documentation and tests — against the base. If they grew, apply P5/P6/P7 with extra care and **state the numbers in the verdict section**: added functional lines vs. moved or reused ones, where the growth went, and whether every new abstraction earns its keep — even when the verdict is clean. Growth with no stated reason is a finding, at least Medium 🟡; a responsibility owned twice, or an abstraction nobody uses, is **High 🟠**.
8. **Documentation**: CHANGELOG.md updated for user-facing changes; DESIGN.md updated for architecture changes (if applicable)
9. **Commit structure**: Does each commit map to one step from the Implementation Plan? Are commit messages clear?

### Severity Classification

Categorize each finding by severity:
- **Critical 🔴**: security issues, data loss, crashes, trust-but-verify violations
- **High 🟠**: design principle violations, missing error handling, broken functionality
- **Medium 🟡**: missing tests, inconsistent naming, dead code
- **Low 🟢**: documentation gaps, style suggestions, minor improvements

For each finding, use this format:
```
**[SEVERITY]** `file:line` — description
Suggestion: how to fix
```

### Overall Verdict

- **Approve**: no Critical or High issues
- **Request Changes**: Critical or High issues found
- **Comment (approve with suggestions)**: only Medium/Low issues
