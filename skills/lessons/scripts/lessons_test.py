#!/usr/bin/env python3
"""Tests for lessons.py — run with `python3 lessons_test.py`.

Each case drives the script the way the skill does, through its command line,
against a throwaway state dir, and checks the files afterwards.
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("lessons.py")
HEADER = "# Lessons Learned"
TWO = (
    f"{HEADER}\n\n"
    "- 2026-10-01 — first rule\n"
    "- 2026-10-02 — second rule\n"
)


class LessonsTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.state = self.root / "state"
        self.topic = self.root / "topic"
        self.state.mkdir()
        self.topic.mkdir()
        self.lessons = self.state / "lessons.md"
        self.agents = self.topic / "AGENTS.md"
        self.session = self.state / "lesson-compact"

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def run_script(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(SCRIPT), *args],
            capture_output=True,
            text=True,
            check=False,
        )

    def snapshot(self, *extra: str) -> subprocess.CompletedProcess:
        return self.run_script(
            "snapshot",
            "--state-dir",
            str(self.state),
            "--topic-dir",
            str(self.topic),
            *extra,
        )

    def apply(self) -> subprocess.CompletedProcess:
        return self.run_script("apply", "--state-dir", str(self.state))

    def status(self) -> subprocess.CompletedProcess:
        return self.run_script("status", "--state-dir", str(self.state))

    def session_json(self) -> dict:
        return json.loads((self.session / "session.json").read_text(encoding="utf-8"))

    # ── snapshot ──────────────────────────────────────────────────────

    def test_snapshot_refuses_without_a_lessons_file(self) -> None:
        result = self.snapshot()
        self.assertEqual(result.returncode, 1)
        self.assertIn("no lessons.md", result.stderr)
        self.assertFalse(self.session.exists())

    def test_snapshot_refuses_a_header_only_file(self) -> None:
        self.lessons.write_text(HEADER + "\n", encoding="utf-8")
        result = self.snapshot()
        self.assertEqual(result.returncode, 1)
        self.assertIn("no lessons", result.stderr)

    def test_snapshot_freezes_the_inputs(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.agents.write_text("# Project\n", encoding="utf-8")

        result = self.snapshot()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.session / "input.md").read_text(encoding="utf-8"), TWO)
        self.assertEqual(
            (self.session / "agents.md.orig").read_text(encoding="utf-8"), "# Project\n"
        )
        session = self.session_json()
        self.assertEqual(session["lessons"]["count"], 2)
        self.assertTrue(session["agents"]["exists"])

    def test_snapshot_records_a_missing_agents_file(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")

        result = self.snapshot()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.session / "agents.md.orig").exists())
        self.assertFalse(self.session_json()["agents"]["exists"])

    def test_snapshot_refuses_to_replace_a_pending_compaction(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.assertEqual(self.snapshot().returncode, 0)
        frozen = (self.session / "input.md").read_text(encoding="utf-8")

        self.lessons.write_text(TWO + "- 2026-10-03 — third rule\n", encoding="utf-8")
        result = self.snapshot()

        self.assertEqual(result.returncode, 1)
        self.assertIn("still pending", result.stderr)
        self.assertEqual((self.session / "input.md").read_text(encoding="utf-8"), frozen)

    def test_snapshot_force_replaces_a_pending_compaction(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.snapshot()
        (self.session / "report.md").write_text("draft", encoding="utf-8")

        self.lessons.write_text(TWO + "- 2026-10-03 — third rule\n", encoding="utf-8")
        result = self.snapshot("--force")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.session / "report.md").exists(), "a stale draft must not survive")
        self.assertEqual(self.session_json()["lessons"]["count"], 3)

    def test_snapshot_force_keeps_the_old_session_when_the_new_input_is_bad(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.snapshot()
        (self.session / "report.md").write_text("draft", encoding="utf-8")

        self.lessons.write_text(HEADER + "\n", encoding="utf-8")
        result = self.snapshot("--force")

        self.assertEqual(result.returncode, 1)
        self.assertIn("no lessons", result.stderr)
        self.assertTrue((self.session / "session.json").exists())
        self.assertEqual((self.session / "report.md").read_text(encoding="utf-8"), "draft")

    # ── status ────────────────────────────────────────────────────────

    def test_status_says_nothing_is_pending(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        result = self.status()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("2 in", result.stdout)
        self.assertIn("none pending", result.stdout)

    def test_status_names_the_missing_outputs(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.snapshot()

        result = self.status()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("pending since", result.stdout)
        self.assertIn("report         missing", result.stdout)
        self.assertIn("unchanged since the snapshot", result.stdout)

    def test_status_flags_lessons_that_changed_after_the_snapshot(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.snapshot()
        self.lessons.write_text(TWO + "- 2026-10-03 — third rule\n", encoding="utf-8")

        result = self.status()

        self.assertIn("CHANGED", result.stdout)
        self.assertIn("2 → 3 lessons, +1", result.stdout)

    def test_status_flags_an_agents_file_created_after_the_snapshot(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.snapshot()
        self.agents.write_text("# Project\n", encoding="utf-8")

        result = self.status()

        self.assertIn("was created after the snapshot", result.stdout)

    # ── apply ─────────────────────────────────────────────────────────

    def test_apply_refuses_without_a_pending_compaction(self) -> None:
        result = self.apply()
        self.assertEqual(result.returncode, 1)
        self.assertIn("no pending compaction", result.stderr)

    def test_apply_refuses_when_lessons_changed(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.snapshot()
        (self.session / "agents.md.new").write_text("# Project\n", encoding="utf-8")
        grown = TWO + "- 2026-10-03 — third rule\n"
        self.lessons.write_text(grown, encoding="utf-8")

        result = self.apply()

        self.assertEqual(result.returncode, 1)
        self.assertIn("changed after the snapshot", result.stderr)
        self.assertEqual(self.lessons.read_text(encoding="utf-8"), grown)
        self.assertFalse(self.agents.exists(), "a refused apply must write nothing")
        self.assertTrue(self.session.exists(), "a refused apply must keep the session")

    def test_apply_refuses_when_agents_changed(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.agents.write_text("# Project\n", encoding="utf-8")
        self.snapshot()
        (self.session / "agents.md.new").write_text("# Project\n\nnew rule\n", encoding="utf-8")
        self.agents.write_text("# Project\n\nedited by hand\n", encoding="utf-8")

        result = self.apply()

        self.assertEqual(result.returncode, 1)
        self.assertIn("AGENTS.md changed", result.stderr)
        self.assertEqual(self.agents.read_text(encoding="utf-8"), "# Project\n\nedited by hand\n")

    def test_apply_writes_agents_archives_and_resets(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.agents.write_text("# Project\n", encoding="utf-8")
        self.snapshot()
        (self.session / "report.md").write_text("# Report\n", encoding="utf-8")
        (self.session / "agents.md.new").write_text("# Project\n\n- new rule\n", encoding="utf-8")

        result = self.apply()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.agents.read_text(encoding="utf-8"), "# Project\n\n- new rule\n")
        self.assertEqual(self.lessons.read_text(encoding="utf-8"), HEADER + "\n")
        archived = list((self.state / "lessons-archive").glob("*.md"))
        archived = [p for p in archived if not p.name.endswith(".report.md")]
        self.assertEqual(len(archived), 1)
        self.assertEqual(archived[0].read_text(encoding="utf-8"), TWO)
        report = archived[0].with_suffix(".report.md")
        self.assertEqual(report.read_text(encoding="utf-8"), "# Report\n")
        self.assertFalse(self.session.exists(), "apply consumes the session")

    def test_apply_without_a_new_agents_file_leaves_agents_alone(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.agents.write_text("# Project\n", encoding="utf-8")
        self.snapshot()

        result = self.apply()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.agents.read_text(encoding="utf-8"), "# Project\n")
        self.assertEqual(self.lessons.read_text(encoding="utf-8"), HEADER + "\n")

    def test_apply_creates_agents_that_did_not_exist(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.snapshot()
        (self.session / "agents.md.new").write_text("# Project\n\n- new rule\n", encoding="utf-8")

        result = self.apply()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.agents.read_text(encoding="utf-8"), "# Project\n\n- new rule\n")

    def test_apply_keeps_the_lines_in_keep_md(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.snapshot()
        (self.session / "keep.md").write_text(
            "- 2026-10-02 — second rule\n", encoding="utf-8"
        )

        result = self.apply()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.lessons.read_text(encoding="utf-8"),
            HEADER + "\n\n- 2026-10-02 — second rule\n",
        )

    def test_apply_refuses_a_kept_line_that_is_not_in_the_snapshot(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.snapshot()
        (self.session / "keep.md").write_text(
            "- 2026-10-02 — second rule\n- 2026-10-09 — invented rule\n", encoding="utf-8"
        )

        result = self.apply()

        self.assertEqual(result.returncode, 1)
        self.assertIn("not in the snapshot", result.stderr)
        self.assertIn("invented rule", result.stderr)
        self.assertEqual(self.lessons.read_text(encoding="utf-8"), TWO)
        self.assertTrue((self.session / "session.json").exists())

    def test_apply_writes_a_repeated_kept_line_once(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.snapshot()
        (self.session / "keep.md").write_text(
            "- 2026-10-02 — second rule\n- 2026-10-01 — first rule\n"
            "- 2026-10-02 — second rule\n",
            encoding="utf-8",
        )

        result = self.apply()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.lessons.read_text(encoding="utf-8"),
            HEADER + "\n\n- 2026-10-02 — second rule\n- 2026-10-01 — first rule\n",
        )

    def test_apply_refuses_an_empty_agents_md_new(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.snapshot()
        (self.session / "agents.md.new").write_text("  \n", encoding="utf-8")

        result = self.apply()

        self.assertEqual(result.returncode, 1)
        self.assertIn("is empty", result.stderr)
        self.assertEqual(self.lessons.read_text(encoding="utf-8"), TWO)

    def test_apply_keeps_the_session_when_it_refuses(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.snapshot()
        (self.session / "agents.md.new").write_text("", encoding="utf-8")

        self.apply()

        self.assertTrue((self.session / "session.json").exists())

    # ── cancel ────────────────────────────────────────────────────────

    def test_cancel_drops_the_session_and_touches_nothing_else(self) -> None:
        self.lessons.write_text(TWO, encoding="utf-8")
        self.agents.write_text("# Project\n", encoding="utf-8")
        self.snapshot()

        result = self.run_script("cancel", "--state-dir", str(self.state))

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.session.exists())
        self.assertEqual(self.lessons.read_text(encoding="utf-8"), TWO)
        self.assertEqual(self.agents.read_text(encoding="utf-8"), "# Project\n")

    def test_cancel_without_a_session_says_so(self) -> None:
        result = self.run_script("cancel", "--state-dir", str(self.state))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("nothing to cancel", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
