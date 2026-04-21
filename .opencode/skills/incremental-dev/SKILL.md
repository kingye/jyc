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

### Detect Project Type and Conventions (do this ONCE at the start)

**Step 1: Read project documentation (if present)**

Check for and read these files in the repository root (in order of priority):
- `AGENTS.md` or `CLAUDE.md` — coding conventions, forbidden actions, toolchain
- `README.md` — build/test/run commands, prerequisites
- `.opencode/skills/` — skill files may define specific workflows and commands
- `.claude/` — may contain project-specific instructions

If these files specify build, test, or check commands, **use those** instead of
the defaults in Step 2.

**Step 2: Detect project type by config files (fallback)**

| Priority | Type | Detection | Source files | Default commands |
|----------|------|-----------|-------------|-----------------|
| 1 | SAP CDS | `.cdsrc.json` exists, OR `package.json` has `@sap/cds` in dependencies | `.cds`, `.js`, `.ts`, `.csv`, `.properties` | Read `scripts` from `package.json` |
| 2 | Rust | `Cargo.toml` exists | `.rs` | check: `cargo check` / test: `cargo test` / build: `cargo build --release` |
| 3 | Node.js | `package.json` exists (no `@sap/cds`) | `.js`, `.ts` | check: `npm run lint` / test: `npm test` / build: `npm run build` |

**Important:**
- Check SAP CDS before Node.js (both have `package.json`)
- For SAP CDS: ALWAYS read `package.json` scripts — do NOT hardcode commands
- For Node.js: check `package.json` scripts for available commands
- Project docs (AGENTS.md, README.md) override these defaults

After detection, you have three commands for the rest of this workflow:
- `{check_command}` — fast syntax/type check
- `{test_command}` — run tests
- `{build_command}` — full production build (run only once at the end)

### Coding Principles

Follow the `coding-principles` skill for behavioral guidelines, especially:
- **Principle 2 (Simplicity First)**: Write minimum code that solves the problem
- **Principle 3 (Surgical Changes)**: Touch only what you must; every changed line should trace to user's request
- **Principle 4 (Goal-Driven Execution)**: Define verifiable success criteria per step

### Principle

Break every task into the smallest possible steps. Each step must be:
1. **Self-contained** — passes check and tests independently
2. **Validated** — verified before moving to the next step
3. **Approved** — user confirms before proceeding (interactive mode only)

### Modes of Operation

**Interactive mode** (default — email, Feishu, direct user interaction):
- Execute one step at a time
- Send a reply after each step with results
- STOP and WAIT for user approval before the next step

**Autonomous mode** (GitHub developer agent — PR-based workflows):
- Execute all steps sequentially without waiting for approval
- Commit and push after each step (one commit per plan step)
- Run check/test before each commit — fix issues before proceeding
- Request review only after all steps are complete
- The Implementation Plan from the PR spec defines the steps

The mode is determined by the agent template. If you are a GitHub developer
agent (AGENTS.md says "GitHub Developer Agent"), use autonomous mode.

### Implementation Flow

For each step (interactive mode — wait for approval):

```
1. Describe what this step will do (brief, 1-2 sentences)
2. Make the change (smallest possible unit)
3. Verify: {check_command} (no errors — fast syntax/type check)
4. Verify: {test_command} (all pass)
5. Commit and push: git add . && git commit -m "<step description>" && git push
6. Send reply with:
   - What was done
   - Check result (pass/fail)
   - Test result (pass/fail)
   - What the next step will be
7. STOP and WAIT for user approval before next step
```

For each step (autonomous mode — no approval needed):

```
1. Read the step's requirements from the plan
2. Make the change
3. Verify: {check_command} (no errors)
4. Verify: {test_command} (all pass)
5. Commit: git add -A && git commit -m "feat: step N - <step title>" && git push
6. Proceed to next step immediately
```

After ALL steps are complete:
```
7. Run: {build_command} (clean build, zero warnings)
8. Report final results
```

Note: Use the fast check command instead of the full build command for per-step
validation. Full build only runs once at the end.
- Rust: `cargo check` (seconds) vs `cargo build` (minutes) — both catch compile errors
- Node/CDS: `npm run lint` (if available) vs `npm run build`

### Rules

- **ONE change per step** — do not combine multiple changes
- **NEVER skip validation** — every step must pass check and test commands
- **ALWAYS commit and push** — every step must be committed and pushed after validation
- **NEVER proceed without approval** — in interactive mode, wait for user to say "yes", "continue", "next", or similar. In autonomous mode, proceed immediately after validation passes.
- **If check fails** — fix it in the SAME step before reporting/proceeding
- **If tests fail** — fix them in the SAME step before reporting/proceeding
- **Do NOT batch steps** — even if you know all the steps, execute one at a time
- **Use the fast check command, not the full build** — full build only runs once at the end

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

Check: ✅ no errors
Tests: ✅ N tests pass
Commit: <short commit hash>

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

- Do NOT make 5 file changes and then run check
- Do NOT skip tests "because it's a small change"
- Do NOT run the full build on every step — use the fast check command instead
- Do NOT continue to the next step without user approval
- Do NOT present a large diff as "one step"
- Do NOT assume the user approves — wait for explicit confirmation
