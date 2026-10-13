---
name: forked-review
description: |
  Delegate a review to a subagent that works in your own directory with a fresh context —
  it reads your uncommitted changes, not a copy of them. Use when: review my changes,
  second opinion, independent review, delegate a review, check my work before committing.
---

## Delegated review in your own tree

An independent review needs two things: the tree as it is *now* (uncommitted
changes included), and a context that has not already decided the answer.
`jyc_spawn_subagent` gives both in one call.

### Steps

1. `jyc_spawn_subagent(name="review-<what>", share_topic_path=true)` — the child
   works in your `topic_path`, so it reads exactly what you have. Do **not** pass
   `inherit_context`: a review that starts with your reasoning in it is not a
   second opinion (this is the default, so just leave it out).
2. `jyc_send_to_topic(channel=…, topic=<child>, require_reply=true)` with the
   task. Name the question, where to start, and what the report looks like:

   > Review the uncommitted changes in this directory (`git status`, `git diff`).
   > Read the surrounding code, not only the diff. Report findings as: severity,
   > file:line, what is wrong, and what you would do instead. Do not change any
   > file — plan mode, read-only.

3. End your turn. The report arrives as a new message in this topic.

### Rules

- **Change nothing**: the child shares your tree, and it inherits the mode you
  are in — if you are in build mode, nothing technical stops it from editing your
  files. Say "do not change any file" in the task; it is the boundary.
- **One reviewer, one task.** Continue the same child with `jyc_send_to_topic`
  for follow-ups (it keeps its own context); spawning a second reviewer on the
  same files buys nothing. Three subagents per topic is the cap.
- The child's state is its own (`.jyc` under the agents root) and yours is not
  shared with it — read the report, not its state dir.
- Closing your topic closes the reviewer with it; close it yourself
  (`/close`) if you want the slot back for something else.
- This is a review, not a test run: the reviewer may run read-only commands to
  understand the code, and anything it executes sees your working tree.