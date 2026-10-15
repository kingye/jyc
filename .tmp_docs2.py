import pathlib, sys

edits = {
    "DESIGN.md": [
        ("""The directory is chosen **once**, at the topic's first activation
(`topic_path::topic_work_dir`): a topic that already exists in an older
layout — its state in the home itself, or nested at `<home>/<topic>/.jyc` —
keeps the directory it was created with, with its state dir untouched. Only a
fresh topic gets `<home>/.data`. A mixed instance is therefore expected after
this change, and no existing topic moves under an agent because of it.""",
         """The directory is chosen **once**, at the topic's first activation, and then
**recorded**: `<home>/.jyc/topic-path` holds it and
`topic_path::topic_work_dir_at` reads that record before the filesystem. The
record is what keeps the choice stable — `<home>/.jyc` is where the *new*
layout keeps the state dir, so its presence can no longer be read as "the home
is the working directory". A topic with no record — anything created before
this change — keeps the directory it was created with: its state in the home
itself, or nested at `<home>/<topic>/.jyc`, with the state dir untouched. Only
a fresh topic gets `<home>/.data`. A mixed instance is therefore expected after
this change, and no existing topic moves under an agent because of it."""),
        ("""- **L3 (topic)**: `<topic_path>/.jyc/` — `config.toml` (the topic's own overrides, see the table below), `skills/`, `templates/`, sessions, chat history, task list (`.jyc/tasks.json`)""",
         """- **L3 (topic)**: the topic's state dir — `<home>/.jyc/` (see Terminology: `topic_path` is the working dir, `.data` beside it) — `config.toml` (the topic's own overrides, see the table below), `skills/`, `templates/`, sessions, chat history, task list (`tasks.json`)"""),
        ("""| `topic_path::{default_topic_dir, topic_home, topic_work_dir}` |""",
         """| `topic_path::{default_topic_dir, topic_home, topic_work_dir, topic_work_dir_at, record_topic_work_dir}` |"""),
    ],
    "crates/jyc-core/src/job_store.rs": [
        ("""//! Each job is stored as `<topic_path>/.jyc/jobs/<id>.json`. The store""",
         """//! Each job is stored in its topic's state dir, `<state dir>/jobs/<id>.json`.
//! The store"""),
    ],
    "crates/jyc-core/src/topic_manager/job_scheduler.rs": [
        ("""//! Jobs are stored per-topic in `<topic>/.jyc/jobs/<id>.json`. The scheduler""",
         """//! Jobs are stored per-topic in the topic's state dir (`.jyc/jobs/<id>.json`).
//! The scheduler"""),
    ],
}

for path, pairs in edits.items():
    p = pathlib.Path(path)
    s = p.read_text()
    for old, new in pairs:
        n = s.count(old)
        if n != 1:
            print(f"!! {path}: expected 1, found {n}: {old[:60]!r}")
            sys.exit(1)
        s = s.replace(old, new)
    p.write_text(s)
    print(f"ok {path}")