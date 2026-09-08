---
name: incremental-dev
description: |
  Incremental development methodology - small step iteration with validation.
  Use when: implementing features, fixing bugs, making code changes, planning implementation.
  渐进式小步迭代开发方法。
---

## Incremental Development — 渐进式小步迭代

CRITICAL: All code changes MUST follow the incremental small-step iteration method.
This applies to both implementation AND planning.

### Setup

- Branch per `dev-workflow` (`feat/…` / `fix/…`, never main).
- Commands come from `dev-workflow` detection: `{check_command}` (fast),
  `{test_command}`, `{build_command}`. Project docs (AGENTS.md, README) override.
- Behavioral guidelines: `coding-principles` skill (esp. Simplicity, Surgical Changes).

### Principle

Break every task into the smallest possible steps. Each step must be:
1. **Self-contained** — ONE change, never a batch or a large diff; passes
   `{check_command}` + `{test_command}` independently
2. **Validated** — fix any failure in the SAME step before proceeding
3. **Approved** — user confirms before the next step (interactive mode only)

Commit and push after each validated step — one commit per step.
`{build_command}` runs ONCE at the end, never per step.

### Modes

**Interactive** (default — email, Feishu, direct chat): execute one step,
report, STOP and WAIT for explicit approval ("yes"/"continue"/"next").
Never assume approval.

**Autonomous** (GitHub developer agent — AGENTS.md says so): execute all
steps from the PR spec's Implementation Plan sequentially, commit+push each,
request review only when all steps are done.

### Report After Each Step

    ✅ Step N/Total: <what was done>
    Check: ✅ | Tests: ✅ N pass | Commit: <hash>
    Next: <brief description>
    Proceed? (yes/no)          ← interactive mode only

On failure: report the step, the issue, what was fixed or needs changing,
and check/test status — then ask whether to retry or adjust.

### Planning

Plans (see `plan-solution`) follow the same principle: numbered small steps,
each independently verifiable, each leaving the codebase working. Present
the plan and wait for approval before starting.
