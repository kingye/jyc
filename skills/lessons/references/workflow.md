# Classification: which lessons earn a place in `AGENTS.md`

`AGENTS.md` is read by every agent that works in this repo, on every turn, and it
is reviewed and committed like code. A lesson is one agent's note from one
topic. Promoting is therefore a narrowing step, not a copy: most lessons stay
out, and the ones that go in are rewritten as rules.

## The four buckets

### 1. Promote — a durable, project-wide rule

It goes into `AGENTS.md` only when all of these hold:

- **Durable** — it will still be true next month. "The formatter rejects trailing
  whitespace" qualifies; "the CI run failed this morning" does not.
- **Project-wide** — it is a fact about this repo, this toolchain or this team's
  conventions, not about this topic's current task.
- **Actionable** — it says what to do, in the imperative, with the condition
  attached: "Use `cargo check -p <crate>` before pushing; never `--workspace`."
  A story ("we once lost an hour to…") is not a rule.
- **Verified** — you can point at the evidence: the same failure twice, a
  documented behaviour, a decision the user made. A single confusing incident is
  not evidence.

Rewriting a lesson into a rule is part of the job. Compress to one or two
sentences; drop the date, the transcript and the story; keep the condition and
the command.

### 2. Merge — several lessons, one rule

Repeated lessons about the same trap are one rule with more evidence behind it.
Merge them into a single line and say in the report which lessons it replaces.
If two lessons contradict each other, do not pick one silently — put both in
"Open questions" and name the evidence for each.

### 3. Keep — stays live in `lessons.md`

Correct as a lesson, not ready as a rule:

- still accumulating evidence, or learned minutes ago and untested;
- true for this topic's work but not for the repo;
- a preference of this user that has not been settled;
- something the user asked to keep.

Put the exact `- ` lines into `keep.md`, copied from `input.md` — `apply`
refuses a line the snapshot does not hold, so a reworded or invented one stops
the whole compaction instead of slipping into the live file. They survive
`apply`; everything else in the old file does not.

### 4. Discard — archived, not promoted, not kept

- one-off incident with no general rule behind it;
- superseded (the behaviour changed, the tool is gone);
- wrong, or a misreading of a tool's output;
- a duplicate of a rule `AGENTS.md` already states.

Discarding does not delete anything: the old file is archived whole. Say why in
the report so the user can overrule you.

## Writing the new `AGENTS.md`

- Start from `agents.md.orig` and edit it; never regenerate it from memory. A
  compaction must not silently drop or reword sections nobody asked about.
- Put a new rule in the section that already covers that ground. Invent a
  section only when nothing fits, and only when there are at least two rules to
  put in it.
- Match the file's voice and language. If `AGENTS.md` is written in terse
  Chinese imperatives, add terse Chinese imperatives — not an English paragraph.
- Prefer one line per rule. The file is prompt text on every turn; length is a
  cost, not a virtue.
- No secrets, credentials, personal data, absolute paths from one machine, or
  anything that only makes sense in this topic's directory.

## The report

`report.md` is what the user reads to decide. Keep it scannable:

```markdown
# Lessons compaction — <YYYY-MM-DD>

Snapshot: 93 lessons, 44590 bytes → promoting 11, merging 6 into 3,
keeping 4, archiving the rest.

## Promote
- **<rule>** ← lessons of 2026-10-22 (×3), 2026-10-30
  Section: "Local vs CI". Evidence: the same clippy failure twice.

## Merge
- 4 lessons about IMAP cursors → one rule in "Channels".

## Keep live
- 2026-11-01 — <line> — why it is not a rule yet.

## Discard
- 2026-10-14 — <line> — one-off debugging trail; the rule, if any, is the
  one already promoted above.

## AGENTS.md
Sections touched: … · one paragraph per change, or the diff summary.

## Open questions
- Two lessons disagree about <X>: <evidence A> vs <evidence B>. Which holds?
```

Then show the diff:

```bash
orig="<state_dir>/lesson-compact/agents.md.orig"
[ -f "$orig" ] || orig=/dev/null   # AGENTS.md did not exist at snapshot time
diff -u "$orig" "<state_dir>/lesson-compact/agents.md.new"
```

## What this is not

This is not a summary of the lessons and not a reason to rewrite them. The old
file is archived verbatim and the live file keeps only what `keep.md` lists —
anything you neither promote nor keep is still in the archive, but it is out of
the agent's view. When in doubt, keep it live or ask; do not discard quietly.
