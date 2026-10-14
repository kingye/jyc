---
name: delegated-dev
description: |
  Delegated development - handing a settled plan to a `develop` subagent: giving it a directory of
  its own (a `git worktree` in this repo), passing the plan in the task message, and collecting the
  review and CI verdicts it comes back with.
  Use when: the work is more than one file, changes logic, or has to open a PR and you would rather
  plan and report than implement - delegate this feature, hand off this branch, spawn a developer,
  develop in a worktree, get a PR built by a subagent.
---

## When to Hand Work Over

The developer keeps its own context: you keep the plan and the conversation with the user, the child
burns its context on building. That is worth a hop when the work is multi-step.

- **Delegate**: more than one file, changed logic, or anything that has to open a PR.
- **Do it yourself**: a typo, a one-line config change, a rename. The handover costs more than the
  work — and you have to plan it either way.

The plan is the input. Never delegate "figure out what to do" — decide that first (`plan-solution`),
then hand the decision over.

## The Developer Role

`subagents/develop/config.toml` — install once with `cp -r subagents/develop <config dir>/subagents/`.
It starts the child in build mode with `dev-workflow` as its whole skill allow-list, and with no
`disabled_tools`: this is the role that has to write and edit files, run `bash` for the gate and
`git`, and spawn the `review` and `ci-watch` children that judge it. Mode and its own instructions
are what restrain it, not a tool list.

The role decides where the child *starts*, not what it is stuck with.

## The Sequence

1. `jyc_spawn_subagent(name = "develop-<what>", type = "develop")` — pass nothing else. No
   `inherit_context`: the child's job is the plan, not your reasoning. No `share_topic_path`: it
   needs a directory of its own. The reply names that directory.
2. Prepare it: `git worktree add <dir>/repo -b <branch>`. **Inside** the child's directory, not as
   the directory itself: the child's state dir is `<dir>/.jyc`, so a checkout at `<dir>` would put
   `.jyc` inside the repository. The branch is part of the plan, and it already exists when the
   developer starts — the developer does not create it. Name it the way this repo names branches
   (`feat/<description>`, `fix/<description>`, with the issue number when there is one, per
   `AGENTS.md`).
3. Send the plan as the task (`jyc_send_to_topic`, `require_reply = true`): the branch name, where
   the checkout is (`repo/`), what done means, and "make this a task list first". In the message,
   not in a file inside the checkout — a plan file would show up in the diff. Two things the message
   must not leave out:
   - **a target dir outside the worktree** — `CARGO_TARGET_DIR=<the main checkout>/target`, or
     `--target-dir` on every cargo call. A fresh worktree has no `target/` of its own, so the first
     gate re-pays the whole dependency graph and keeps a copy of it: exactly the slow gate and the
     filled disk this project's rules warn about, arriving as a surprise in the child's first turn;
   - **the checkout path in every task you send a `review` or `ci-watch` child of the developer**.
     Those children work in the developer's directory, one level above the checkout, so at their own
     cwd `git status` answers "not a git repository" until the task tells them where to go.
4. End your turn. The child delegates the review and the CI wait itself, and both of those children
   share *its* directory, so they read its tree — not yours. Nothing comes back until the one report
   does.
5. Report to the user: the PR link, the review verdict, the CI verdict. **Merging stays the user's
   call** — the developer never merges.

## Clean Up the Checkout

`git worktree remove <dir>/repo` when you are done with the developer — **yourself**, and **before**
you close it: a closed topic cannot be sent a command, so nothing about this can happen afterwards.

That order is also what gets `<dir>` itself put away. With the checkout out of it the topic dir is
empty, and an emptied topic dir is removed with its state; close first and it survives holding only
`repo/`, which costs you two things — a branch left checked out of a directory nobody is working in,
and the name, since `<agents dir>/<name>` existing is what makes a later spawn of that name refuse
("topic already exists"). `git worktree prune` is no rescue here: it clears entries whose directory
is already *gone*, and this one is still on disk. If something is left anyway, `rm -rf <dir>` after
`git worktree remove --force <dir>/repo`.

Closing your own topic closes the developer with it, and still leaves the checkout behind — the same
two steps, in the same order, apply.

## Not Only Git

None of this is framework knowledge. jyc gives a child a directory of its own and lets the parent
write into it; nothing in it knows what a worktree is. This repo's projects are git, so here the
directory is prepared as one — another project would prepare it another way, and the skill's job is
only to say whose it is and who clears it up.

## Rules

- **One developer at a time.** Parallel PRs are banned, so a second `develop` child on the same
  repository buys nothing (and `git worktree` refuses the same branch twice anyway).
- **The report is the deliverable**, not a summary of the child's steps. If it comes back thin, ask
  the same child for the missing piece (`jyc_send_to_topic`) instead of reading its state dir.
- **Read the report, not the child's `.jyc`** — that is its own state, and closing it is what
  removes it.
- If the child's plan no longer fits what you learned, send it the new decision as a message; do not
  spawn a second developer on the same branch.