---
name: jyc-cron
description: |
  Create or update a crontab job that sends one or more messages to an existing
  jyc agent or topic with `jyc send`. Use when the user asks for a scheduled,
  periodic, daily, weekly, recurring or reminder task, or mentions cron.
---

## Schedule messages with crontab

Use `crontab` to trigger `jyc send`; do not build a resident loop or use
`sleep` for a schedule.

### 1. Settle the job

Confirm these before editing crontab, and ask for anything missing:

- the target agent;
- the optional topic (when omitted, the agent's own topic is used);
- the message or messages, in order;
- the cron expression and the timezone it is meant to use.

The target must already exist: `jyc send` never creates an agent or topic.
Messages are positional, sent one at a time in argument order, and each quoted
argument is a separate message. Quote every message, for example:

```bash
jyc send myagent "First task" "Second task"
```

Use `command -v jyc` now to get the absolute binary path. Put that path in the
crontab line: cron's `PATH` is minimal and may not contain `jyc`.

Keep the message simple enough to survive both the shell and crontab parsing.
If it contains `%`, newlines, quotes or other shell metacharacters, put it in a
small executable script and schedule that script instead. A literal `%` in a
crontab command otherwise starts cron's special input handling.

### 2. Edit crontab without losing entries

Choose a short, stable job id from the target and purpose, such as
`myagent-daily-report`.

Read the current crontab first and preserve every unrelated line:

```bash
tmp=$(mktemp)
crontab -l > "$tmp"
# Edit $tmp, then install it with: crontab "$tmp"
```

If `crontab -l` reports that the user has no crontab, start from an empty file.
Any other failure is real: stop and show it rather than overwriting the
crontab with only the new job.

Represent the job as one marked block:

```cron
# jyc-send:myagent-daily-report
0 9 * * * /absolute/path/to/jyc send myagent "Generate today's report" >> /tmp/jyc-send-myagent-daily-report.log 2>&1
```

Use `--topic <topic>` between the agent and the messages when the job targets a
named topic:

```cron
# jyc-send:myagent-reports
0 * * * * /absolute/path/to/jyc send myagent --topic reports "Collect data" "Generate report" >> /tmp/jyc-send-myagent-reports.log 2>&1
```

The `# jyc-send:<id>` line is the identity of this job. When the id already
exists, update that block in place; never append a second copy. Do not replace
the whole crontab with only the new job.

If the schedule depends on a timezone, state it explicitly. Cron normally uses
the host timezone; if the installed cron supports `CRON_TZ`, put it on its own
line before the job. Do not silently assume the user's timezone.

### 3. Install and report

Install the edited file with `crontab "$tmp"`, then verify with `crontab -l`.
Report the final marked entry, its intended timezone and the log path. If the
user asked only to inspect or draft the job, show the entry without installing
it.