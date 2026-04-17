---
name: plan-solution
description: |
  Create structured implementation plans with incremental steps.
  Use when: in plan mode, creating implementation plans, analyzing requirements, proposing solutions.
---

## Plan Solution — Structured Implementation Planning

When asked to plan a solution, follow this structure:

### 1. Understand the Requirement
- Read the user's request carefully
- Read the project's documentation for context:
  - `AGENTS.md` or `CLAUDE.md` — coding conventions and constraints
  - `DESIGN.md` — architecture and design principles (if present)
  - `README.md` — build commands, prerequisites
  - `.opencode/skills/` or `.claude/` — project-specific workflows
- Read relevant source code (use `read`, `glob`, `grep` — NOT `task` tool)
- Summarize your understanding in 2-3 sentences

### 2. Create Implementation Plan
Break the solution into the smallest possible steps. Each step must:
- Be independently verifiable (passes check + tests)
- Leave the codebase in a working state
- Build on the previous step

Format:
```
## Implementation Plan

### Step 1: <title>
- Files: <list files to change>
- Change: <what to do>
- Verify: run check + test commands (per project type)

### Step 2: <title>
- Files: <list files to change>
- Change: <what to do>
- Verify: run check + test commands (per project type)
```

Use the project's actual check/test commands (from AGENTS.md, README.md, or
detected from config files like Cargo.toml, package.json, .cdsrc.json).

### 3. Rules for Planning
- Maximum 5-10 steps per plan
- Each step changes 1-3 files maximum
- Do NOT explore the entire codebase — read docs first (AGENTS.md, DESIGN.md, README.md, CHANGELOG.md)
- Do NOT use the `task` tool for exploration — use `read`, `glob`, `grep` directly
- Reference existing patterns in the codebase
- Consider backward compatibility
- Consider the project's design principles (from AGENTS.md or DESIGN.md)
- Always create a feature branch before implementation: `git checkout -b feat/<name>`

### 4. Present and Wait
- Present the complete plan
- Send reply with the plan
- STOP and wait for user approval before any implementation
- Do NOT start implementing until the user confirms
