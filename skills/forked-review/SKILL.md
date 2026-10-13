---
name: forked-review
description: |
  Delegate a review to a subagent that works in your own directory with a fresh context —
  it reads your uncommitted changes, not a copy of them. Use when: review my changes,
  second opinion, independent review, delegate a review, check my work before committing.
---

## Delegated review in your own tree

An independent review needs three things: the tree as it is *now* (uncommitted
changes included), a context that has not already decided the answer, and a
reviewer that cannot quietly fix what it finds. `jyc_spawn_subagent` gives all
three in one call, the third through the `review` type.

### Steps

1. `jyc_spawn_subagent(name="review-<what>", share_topic_path=true, type="review")`
   — the child works in your `topic_path`, so it reads exactly what you have,
   and it starts as the `review` role: plan mode, the review skills only, and
   `write`/`edit`/`bash` disabled. Do **not** pass `inherit_context`: a review
   that starts with your reasoning in it is not a second opinion (this is the
   default, so just leave it out).
2. `jyc_send_to_topic(channel=…, topic=<child>, require_reply=true)` with the
   task. Name the question, where to start, and what the report looks like:

   > Review the uncommitted changes in this directory (`git status`, `git diff`).
   > Read the surrounding code, not only the diff. Report findings as: severity,
   > file:line, what is wrong, and what you would do instead.

3. End your turn. The report arrives as a new message in this topic.

### The `review` type

If the spawn refuses with `unknown subagent type 'review'`, the type is not
defined on this machine yet. Install it once, either way round (see
`config.example.toml` → "Subagent Types"):

```toml
# ~/.config/jyc/subagents/review/config.toml
mode = "plan"
skills          = ["pr-review", "ponytail-review"]
disabled_skills = ["forked-review"]
disabled_tools  = ["write", "edit", "bash", "jyc_spawn_subagent"]
instructions    = """
You are the reviewer for this delegation: read, verify, report. Change no file.
"""
```

or, equivalently, `[subagents.review]` in `~/.config/jyc/config.toml` with the
same keys (`instructions` then lives under the same table). A type is expanded
at spawn time, so a child spawned earlier keeps the role it started with.

### Rules

- **The type is the boundary, not your wording.** With `type="review"` the child
  cannot write files or spawn its own reviewer, and it cannot see the skills it
  would need to do your job instead of judging it. Say what to review; the role
  takes care of what it may do.
- **Sharpen the task, not the permissions.** If the review needs something the
  role forbids (running a test build, say), that is a different job with a
  different type — do not talk the reviewer out of its role.
- **One reviewer, one task.** Continue the same child with `jyc_send_to_topic`
  for follow-ups (it keeps its own context); spawning a second reviewer on the
  same files buys nothing. Three subagents per topic is the cap.
- The child's state is its own (`.jyc` under the agents root) and yours is not
  shared with it — read the report, not its state dir.
- Closing your topic closes the reviewer with it; close it yourself
  (`/close`) if you want the slot back for something else.