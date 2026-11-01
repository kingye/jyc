#!/usr/bin/env python3
"""Deterministic file operations for the `lessons` skill.

The skill does the thinking: it reads the frozen lessons, classifies them and
writes the report and the proposed `AGENTS.md` (see `../references/workflow.md`).
This script does the parts that must not depend on a model:

* `snapshot` — freeze `lessons.md` and `AGENTS.md` and record their hashes;
* `status`   — say what a pending compaction still has, and whether the inputs
               changed underneath it;
* `apply`    — re-check the hashes, write the proposed `AGENTS.md`, archive the
               old lessons and start a fresh file;
* `cancel`   — drop the pending compaction, touching no other file.

Usage:
    lessons.py snapshot --state-dir DIR [--topic-dir DIR] [--agents FILE] [--force]
    lessons.py status   --state-dir DIR [--topic-dir DIR] [--agents FILE]
    lessons.py apply    --state-dir DIR
    lessons.py cancel   --state-dir DIR

Everything lives under the topic's state dir:

    lessons.md                        the live file (owned by `lesson_add`)
    lesson-compact/session.json       the pending compaction and its hashes
    lesson-compact/input.md           frozen copy of lessons.md at snapshot time
    lesson-compact/agents.md.orig     frozen copy of AGENTS.md at snapshot time
    lesson-compact/report.md          the classification (written by the agent)
    lesson-compact/agents.md.new      the proposed AGENTS.md (written by the agent)
    lesson-compact/keep.md            lessons that stay live (written by the agent)
    lessons-archive/<ts>-<sha>.md     one archived file per apply

Exit code is 0 on success and 1 on a refusal, with the reason on stderr — a
refusal leaves every file as it was.
"""

import argparse
import datetime
import hashlib
import json
import os
import shutil
import sys
from pathlib import Path

LESSONS_FILE = "lessons.md"
HEADER = "# Lessons Learned"
SESSION_DIR = "lesson-compact"
ARCHIVE_DIR = "lessons-archive"
SESSION_FILE = "session.json"
INPUT_FILE = "input.md"
ORIG_FILE = "agents.md.orig"
REPORT_FILE = "report.md"
NEW_FILE = "agents.md.new"
KEEP_FILE = "keep.md"


def fail(message: str) -> "None":
    """Refuse: say why on stderr and exit non-zero, changing nothing."""
    print(f"lessons: {message}", file=sys.stderr)
    sys.exit(1)


def sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def read_text(path: Path) -> "str | None":
    try:
        return path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return None


def lesson_count(text: str) -> int:
    """The lessons in a file: one `- ` line each, the header excluded."""
    return sum(1 for line in text.splitlines() if line.startswith("- "))


def lesson_lines(text: str) -> list:
    return [line for line in text.splitlines() if line.startswith("- ")]


def write_atomic(path: Path, text: str) -> None:
    """Write `text` to `path` so a crash leaves either old or new, never half."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)


def state_dir(args: argparse.Namespace) -> Path:
    if not args.state_dir:
        fail("--state-dir is required (use the state_dir the framework gave you)")
    return Path(args.state_dir).resolve()


def session_dir(args: argparse.Namespace) -> Path:
    return state_dir(args) / SESSION_DIR


def load_session(sdir: Path) -> "dict | None":
    text = read_text(sdir / SESSION_FILE)
    if text is None:
        return None
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        fail(f"{sdir / SESSION_FILE} is not valid JSON — run `cancel` and compact again")


def agents_path(args: argparse.Namespace) -> Path:
    """Where the project's AGENTS.md is (or would be created)."""
    if args.agents:
        return Path(args.agents).resolve()
    topic_dir = Path(args.topic_dir or os.getcwd()).resolve()
    return topic_dir / "AGENTS.md"


def cmd_snapshot(args: argparse.Namespace) -> None:
    sdir = session_dir(args)
    existing = load_session(sdir)
    if existing is not None and not args.force:
        fail(
            f"a compaction from {existing.get('created_at', 'earlier')} is still pending "
            f"in {sdir} — review it with `status`, finish it with `apply`, drop it with "
            f"`cancel`, or replace it with `snapshot --force`"
        )
    # Validate the new snapshot before touching the old session: `--force`
    # must never destroy a pending compaction it then fails to replace.
    lessons_path = state_dir(args) / LESSONS_FILE
    lessons = read_text(lessons_path)
    if lessons is None:
        fail(f"no {LESSONS_FILE} at {lessons_path} — this topic has recorded no lessons")
    count = lesson_count(lessons)
    if count == 0:
        fail(f"{lessons_path} holds no lessons — nothing to compact")

    agents = agents_path(args)
    agents_text = read_text(agents)

    if sdir.exists():
        shutil.rmtree(sdir)

    sdir.mkdir(parents=True)
    # The frozen copy is what the agent classifies: it is complete, while the
    # system prompt carries only the newest 2 KiB and `lesson_read` the next
    # 16 KiB, so neither can be trusted as a full snapshot.
    write_atomic(sdir / INPUT_FILE, lessons)
    if agents_text is not None:
        write_atomic(sdir / ORIG_FILE, agents_text)

    session = {
        "version": 1,
        "created_at": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "lessons": {
            "path": str(lessons_path),
            "sha256": sha256(lessons),
            "bytes": len(lessons.encode("utf-8")),
            "count": count,
        },
        "agents": {
            "path": str(agents),
            "exists": agents_text is not None,
            "sha256": sha256(agents_text) if agents_text is not None else None,
        },
    }
    write_atomic(sdir / SESSION_FILE, json.dumps(session, indent=2) + "\n")

    print(f"snapshot: {count} lessons, {session['lessons']['bytes']} bytes")
    print(f"  lessons  {lessons_path}")
    print(f"  agents   {agents}{'' if agents_text is not None else ' (does not exist yet)'}")
    print(f"  session  {sdir}")
    print()
    print("next: read input.md in full, then write report.md and agents.md.new")
    print("      (and keep.md when some lessons should stay live) in the session dir")


def input_changes(session: dict) -> "list[str]":
    """What changed since the snapshot, as human-readable lines (empty: nothing)."""
    changes = []

    recorded = session["lessons"]
    current = read_text(Path(recorded["path"]))
    if current is None:
        changes.append(f"{recorded['path']} is gone")
    elif sha256(current) != recorded["sha256"]:
        delta = lesson_count(current) - recorded["count"]
        changes.append(
            f"{recorded['path']} changed ({recorded['count']} → {lesson_count(current)} lessons"
            f"{f', {delta:+d}' if delta else ''})"
        )

    recorded_agents = session["agents"]
    current_agents = read_text(Path(recorded_agents["path"]))
    if recorded_agents["exists"]:
        if current_agents is None:
            changes.append(f"{recorded_agents['path']} is gone")
        elif sha256(current_agents) != recorded_agents["sha256"]:
            changes.append(f"{recorded_agents['path']} changed")
    elif current_agents is not None:
        changes.append(f"{recorded_agents['path']} was created after the snapshot")

    return changes


def report_status(session: dict, sdir: Path) -> None:
    recorded = session["lessons"]
    live = read_text(Path(recorded["path"]))
    if live is None:
        print(f"lessons: {recorded['count']} at snapshot time; {recorded['path']} is gone")
    else:
        print(
            f"lessons: {recorded['count']} at snapshot time, "
            f"{lesson_count(live)} now in {recorded['path']} "
            f"({len(live.encode('utf-8'))} bytes)"
        )
    print(f"compact: pending since {session.get('created_at', 'unknown')}")
    for name, label in (
        (REPORT_FILE, "report"),
        (NEW_FILE, "new AGENTS.md"),
        (KEEP_FILE, "kept lessons"),
    ):
        print(f"  {label:<14} {'present' if (sdir / name).exists() else 'missing'}")
    changes = input_changes(session)
    if changes:
        print("inputs: CHANGED since the snapshot — `apply` will refuse:")
        for line in changes:
            print(f"  - {line}")
        print("  (compact again to pick the change up)")
    else:
        print("inputs: unchanged since the snapshot — `apply` will go through")


def cmd_status(args: argparse.Namespace) -> None:
    sdir = session_dir(args)
    session = load_session(sdir)
    if session is None:
        lessons_path = state_dir(args) / LESSONS_FILE
        lessons = read_text(lessons_path)
        if lessons is None:
            print(f"lessons: no {LESSONS_FILE} yet")
        else:
            print(
                f"lessons: {lesson_count(lessons)} in {lessons_path} "
                f"({len(lessons.encode('utf-8'))} bytes)"
            )
        print("compact: none pending")
        return
    report_status(session, sdir)


def cmd_apply(args: argparse.Namespace) -> None:
    sdir = session_dir(args)
    session = load_session(sdir)
    if session is None:
        fail(f"no pending compaction in {sdir} — run `snapshot` first")

    changes = input_changes(session)
    if changes:
        fail(
            "the inputs changed after the snapshot, so applying it would discard work:\n  - "
            + "\n  - ".join(changes)
            + "\ncompact again and re-review"
        )

    lessons_path = Path(session["lessons"]["path"])
    agents_recorded = session["agents"]
    agents_file = Path(agents_recorded["path"])

    new_agents = read_text(sdir / NEW_FILE)
    if new_agents is not None and not new_agents.strip():
        fail(f"{sdir / NEW_FILE} is empty — delete it if nothing should be promoted")

    keep = read_text(sdir / KEEP_FILE)
    kept = lesson_lines(keep) if keep else []
    # A kept line must be a lesson the snapshot actually holds: `apply` is the
    # only writer of the live file, so a line invented or copied twice here
    # would land in `lessons.md` with nothing to check it against.
    source = set(lesson_lines(read_text(sdir / INPUT_FILE) or ""))
    foreign = [line for line in kept if line not in source]
    if foreign:
        fail(
            f"{sdir / KEEP_FILE} holds lines that are not in the snapshot:\n  - "
            + "\n  - ".join(sorted(set(foreign)))
            + "\ncopy the kept lines verbatim from input.md"
        )
    deduped = list(dict.fromkeys(kept))
    dropped = len(kept) - len(deduped)
    kept = deduped

    if new_agents is not None:
        write_atomic(agents_file, new_agents)

    archived_text = read_text(lessons_path) or ""
    stamp = datetime.datetime.now().strftime("%Y%m%dT%H%M%S")
    archived = state_dir(args) / ARCHIVE_DIR / f"{stamp}-{session['lessons']['sha256'][:8]}.md"
    write_atomic(archived, archived_text)

    fresh = HEADER + "\n"
    if kept:
        fresh = HEADER + "\n\n" + "\n".join(kept) + "\n"
    write_atomic(lessons_path, fresh)

    report = read_text(sdir / REPORT_FILE)
    if report is not None:
        write_atomic(archived.with_suffix(".report.md"), report)
    shutil.rmtree(sdir)

    print("applied:")
    if new_agents is not None:
        print(f"  agents   {agents_file} written ({len(new_agents.encode('utf-8'))} bytes)")
    else:
        print("  agents   unchanged (no agents.md.new)")
    print(f"  archived {archived} ({session['lessons']['count']} lessons)")
    print(f"  lessons  {lessons_path} reset, {len(kept)} kept live")
    if dropped:
        print(f"  ({dropped} duplicate line(s) in {KEEP_FILE} written once)")


def cmd_cancel(args: argparse.Namespace) -> None:
    sdir = session_dir(args)
    if not sdir.exists():
        print("cancelled: nothing to cancel")
        return
    shutil.rmtree(sdir)
    print(f"cancelled: {sdir} removed; lessons and AGENTS.md untouched")


def main(argv: "list[str] | None" = None) -> None:
    parser = argparse.ArgumentParser(
        prog="lessons.py",
        description="Freeze, verify and apply a compaction of this topic's lessons.",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    def common(p: argparse.ArgumentParser) -> None:
        p.add_argument("--state-dir", required=True, help="the topic's state dir ($…/.jyc)")
        p.add_argument("--topic-dir", help="the topic's working dir (default: cwd)")
        p.add_argument("--agents", help="explicit AGENTS.md path (default: TOPIC_DIR/AGENTS.md)")

    snap = sub.add_parser("snapshot", help="freeze lessons.md and AGENTS.md")
    common(snap)
    snap.add_argument("--force", action="store_true", help="replace a pending compaction")
    snap.set_defaults(func=cmd_snapshot)

    stat = sub.add_parser("status", help="show the pending compaction and the live file")
    common(stat)
    stat.set_defaults(func=cmd_status)

    app = sub.add_parser("apply", help="write AGENTS.md, archive lessons, reset the file")
    app.add_argument("--state-dir", required=True)
    app.set_defaults(func=cmd_apply)

    cancel = sub.add_parser("cancel", help="drop the pending compaction")
    cancel.add_argument("--state-dir", required=True)
    cancel.set_defaults(func=cmd_cancel)

    args = parser.parse_args(argv)
    args.func(args)


if __name__ == "__main__":
    main()
