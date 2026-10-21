---
name: coding-principles
description: |
  Andrej Karpathy's 4 coding principles for LLM agents: Think Before Coding, Simplicity First,
  Surgical Changes, Goal-Driven Execution — plus three house rules that apply to all coding:
  Single Point of Responsibility, Reuse Before Adding, No Overdesign. Use when writing code,
  planning, or reviewing code to reduce common LLM coding mistakes.
---

## Coding Principles

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

### 1. Think Before Coding

Don't assume. Don't hide confusion. Surface tradeoffs.

Before implementing:

- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them — don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

### 2. Simplicity First

Minimum code that solves the problem. Nothing speculative.

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.
- Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

### 3. Surgical Changes

Touch only what you must. Clean up only your own mess.

When editing existing code:

- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it — don't delete it.

When your changes create orphans:

- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

**The test:** Every changed line should trace directly to the user's request.

### 4. Goal-Driven Execution

Define success criteria. Loop until verified.

Transform tasks into verifiable goals:

- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

**A proxy is not a proof.** "It compiles" never stands in for "it passes". Name the tier that
actually ran; when the real check runs somewhere else (CI), report it as pending, not verified.

For multi-step tasks, state a brief plan:

1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]

**Strong success criteria** let you loop independently. **Weak criteria** ("make it work") require constant clarification.

### 5. Single Point of Responsibility

One responsibility, one owner — for everything, not just reads.

Every responsibility has exactly one owner: the one place that knows how it is done. Resolving a fact, applying a rule, maintaining a state, deciding an outcome — each is owned once, and every other place asks the owner instead of doing it a second way.

- The owner holds the knowledge: the function that knows how, the type that holds the state, the module that owns the rule.
- Two owners of one responsibility means the outcome is decided twice, and the two decisions drift.
- Two paths to one result are a bug waiting: whoever changes it must keep both paths honest.
- It covers writes too — whoever may change a thing owns *how* it changes (one writer, one invariant), not only who may read it.
- When a second place needs the same responsibility, call the owner — don't copy it, don't re-derive it, don't add a parallel path.

**The test:** name the owner of every responsibility your change touches. A name you cannot give is a responsibility nobody owns yet.

### 6. Reuse Before Adding

Reach for what exists before you write something new.

- Search the tree for the behavior before implementing it — a function, a type, an endpoint, a rule.
- Lift a rule to the layer that owns it and call it from every caller: one implementation, many callers.
- The CLI, the server and the in-process command go through the same internal function for the same fact.
- Prefer extending what exists over adding a parallel path beside it.
- Two surfaces showing the same thing show it from the same source.

**The test:** every new function either calls something that already existed, or is called by something that already existed.

### 7. No Overdesign

The smallest design that solves the problem. Every abstraction must earn itself.

- A trait with one implementor, a struct that mirrors another struct, a wrapper that adds nothing, a parameter nobody sets — overdesign, not flexibility.
- Don't build for a caller that does not exist yet; when it arrives, move the shared part into the owner rather than copying it.
- A seam added only for tests is a cost — it must buy a behavior you cannot pin any other way.
- Prefer the smallest shape that carries the fact (four fields, not twelve).
- When a change grows, say where the lines went: moved, reused, new — and whether the new ones earn their place.

**The test:** name the abstraction's second caller. If you cannot, delete it and write the direct code.

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, no responsibility owned twice, new code that reuses what already exists, and clarifying questions come before implementation rather than after mistakes.