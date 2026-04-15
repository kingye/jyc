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

### Before Starting

- Always create a feature branch: `git checkout -b feat/<description>` or `fix/<description>`
- NEVER work directly on main
- See `dev-workflow` skill for branching conventions

### Principle

Break every task into the smallest possible steps. Each step must be:
1. **Self-contained** — compiles and passes tests independently
2. **Validated** — verified before moving to the next step
3. **Approved** — user confirms before proceeding

### Implementation Flow

For each step:

```
1. Describe what this step will do (brief, 1-2 sentences)
2. Make the change (smallest possible unit)
3. Verify: cargo build --release (zero warnings)
4. Verify: cargo test (all pass)
5. Send reply with:
   - What was done
   - Build result (pass/fail)
   - Test result (pass/fail)
   - What the next step will be
6. STOP and WAIT for user approval before next step
```

### Rules

- **ONE change per step** — do not combine multiple changes
- **NEVER skip validation** — every step must compile and pass tests
- **NEVER proceed without approval** — wait for user to say "yes", "continue", "next", or similar
- **If build fails** — fix it in the SAME step before reporting
- **If tests fail** — fix them in the SAME step before reporting
- **Do NOT batch steps** — even if you know all the steps, execute one at a time

### Planning

When creating an implementation plan, also follow this principle:
- Break the plan into numbered small steps
- Each step should be independently verifiable
- Each step should leave the codebase in a working state
- Indicate what will be verified at each step
- Present the plan and wait for approval before starting

### Reply Format After Each Step

```
✅ Step N/Total: <what was done>

Build: ✅ zero warnings
Tests: ✅ N tests pass

Next step: <brief description of next step>

Proceed? (yes/no)
```

### If Something Goes Wrong

```
❌ Step N/Total: <what was attempted>

Issue: <what went wrong>
Fix: <how it was fixed or what needs to change>

Build: ✅/❌
Tests: ✅/❌

Shall I retry or adjust the approach?
```

### Anti-Patterns (DO NOT)

- Do NOT make 5 file changes and then run build
- Do NOT skip tests "because it's a small change"
- Do NOT continue to the next step without user approval
- Do NOT present a large diff as "one step"
- Do NOT assume the user approves — wait for explicit confirmation
