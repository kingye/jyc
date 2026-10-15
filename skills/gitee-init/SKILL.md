---
name: gitee-init
description: |
  Initialize a Gitee-triggered topic by cloning the repository INTO the topic directory itself.
  Use ALWAYS when the incoming message is a Gitee event (its body contains a
  `repository: <owner>/<repo>` line) and there is no `.git` in the topic directory yet.
  Run this BEFORE any code reading, planning, development, or review work.
---

## Gitee Topic Initialization

The topic directory **is** the working copy. Clone into `.` — never into a `repo/`
subdirectory.

### Where the repository name comes from

Every Gitee trigger message carries it in the body:

```
gitee event: pull_request
repository: <owner>/<repo>
number: 42
type: pull_request
action: opened
actor: someone
```

Read `<owner>/<repo>` from the `repository:` line of the triggering message. If the
current message lacks it, read the earliest message in the chat history.

### Steps

1. **Already initialized?** If `.git/` exists here, stop — the repo is ready.

2. **Clone.** A delivered attachment lands in `.attachments/` inside this directory,
   and `git clone <url> .` refuses a directory that is not empty. The framework's
   own state is not in here (the topic's `.jyc` is a sibling of this directory), but
   clone beside it and move the git directory in anyway:

   ```bash
   git clone "https://gitee.com/<owner>/<repo>.git" .init-clone
   mv .init-clone/.git .
   rm -rf .init-clone
   git reset --hard HEAD    # materializes tracked files; untracked framework files survive
   ```

   For a private repository, put the token in the URL:
   `https://<user>:${GITEE_TOKEN}@gitee.com/<owner>/<repo>.git`. The token is in
   the channel config, available as `GITEE_TOKEN` in the environment.

3. **Keep framework files out of git.** Use `.git/info/exclude` — it is local-only and
   never shows up as a repository change:

   ```bash
   printf '.attachments/\n' >> .git/info/exclude
   ```

4. **Verify:**

   ```bash
   git log --oneline -1
   git status --short    # must NOT list .attachments/
   ```

### After cloning

- The repository's own `AGENTS.md` now sits at the topic root, so the framework loads it
  as project instructions from the next turn onward. Read it yourself now.
- Skills shipped in the repository (`.claude/skills/`, `.opencode/skills/`, `.jyc/skills/`)
  are discovered automatically. Do not copy them anywhere.

### Rules

- NEVER delete or re-clone an existing checkout. If a clone fails, troubleshoot it
  (network, token, repo visibility) — do not `rm -rf` and retry.
- NEVER commit `.attachments/` or any other framework file.
- If authentication is missing, say so in your reply. Never silently skip initialization
  and then act as if the code was unavailable.