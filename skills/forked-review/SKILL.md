---
name: forked-review
description: |
  Delegate a review to a subagent that works in your own directory with a fresh context —
  it reads the changes on your branch as they are now (uncommitted, or the commits you
  just made), not a copy of them. Use when: you finished a non-trivial change and are
  about to push or open a PR, review my changes, second opinion, independent review,
  delegate a review, check my work before committing.
---

## Delegated review in your own tree

An independent review needs three things: the tree as it is *now* (uncommitted
changes, or the commits you just made), a context that has not already decided
the answer, and a reviewer that cannot quietly fix what it finds. `jyc_spawn_subagent`
gives all three in one call, the third through the `review` type.

### Steps

1. `jyc_spawn_subagent(name="review-<what>", share_topic_path=true, type="review")`
   — the child works in your `topic_path`, so it reads exactly what you have,
   and it starts as the `review` role (see below). Do **not** pass
   `inherit_context`: a review that starts with your reasoning in it is not a
   second opinion (this is the default, so just leave it out).
2. `jyc_send_to_topic(channel=…, topic=<child>, require_reply=true)` with the
   task. Name the question, where to start, and what the report looks like:

   > Review this branch against its base (`git diff <base>..HEAD`, plus `git
   > status` for anything still uncommitted). Read the surrounding code, not only
   > the diff. Report findings as: severity, file:line, what is wrong, and what
   > you would do instead.

3. End your turn. The report arrives as a new message in this topic.

### The `review` type

This repository ships the role at `subagents/review/config.toml`. What it sets:

| Key | Value | Why |
|---|---|---|
| `mode` | `plan` | It reasons about the change, it does not act on it |
| `skills` | `pr-review`, `ponytail-review`, `coding-principles`, `ponytail` | The review methodology and the review lenses, nothing else |
| `disabled_skills` | `forked-review` | It does not hand the review on |
| `disabled_tools` | `write`, `edit`, `jyc_spawn_subagent` | It cannot change your files |
| `instructions` | report-only standing orders | Severity + `file:line` + suggested fix, unverified claims marked as such |

`bash` is deliberately **not** disabled: a review of a branch needs `git status`
(anything still uncommitted), `git diff <base>..HEAD` and `git log`, and no
read-only shell exists. Plan mode and the role's instructions are what restrain
it — the tools that edit files are simply absent, which is the part wording
cannot undo.

Install it once, if the spawn refuses with `unknown subagent type 'review'`:

```bash
cp -r subagents/review ~/.config/jyc/subagents/      # from a jyc checkout
```

Equivalently, the same keys as `[subagents.review]` in
`~/.config/jyc/config.toml`; defined both ways, they merge key by key with the
in-config table winning. A type is expanded at spawn time, so a child spawned
earlier keeps the role it started with.

### Rules

- **The type is the boundary, not your wording.** With `type="review"` the child
  has no `write` and no `edit` and cannot spawn its own reviewer, and it cannot
  see the skills it would need to do your job instead of judging it. Say what to
  review; the role takes care of what it may do.
- **Sharpen the task, not the permissions.** If the review needs something the
  role forbids (running a test build, say), that is a different job with a
  different type — do not talk the reviewer out of its role.
- **One reviewer, one task.** Continue the same child with `jyc_send_to_topic`
  for follow-ups (it keeps its own context); spawning a second reviewer on the
  same files buys nothing.
- The child's state is its own (`.jyc` under the agents root) and yours is not
  shared with it — read the report, not its state dir.
- Closing your topic closes the reviewer with it; close it yourself
  (`/close`) when the review is done and you want nothing more from it.