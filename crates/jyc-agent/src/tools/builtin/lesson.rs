//! Lesson tools — the agent's own record of what it learned.
//!
//! A lesson is one distilled, reusable rule the agent wants to keep for
//! itself: a trap it fell into, a fix that worked, a convention of this repo
//! or this user. They live in `<state dir>/lessons.md` — one file per topic,
//! beside the user's own `notes.md` — and the head of that file is injected
//! into the system prompt, so a lesson is in view on every later turn
//! without a tool call.
//!
//! The file is append-only: the agent can add and read, never rewrite or
//! drop. Pruning is the user's call.
//!
//! Each topic's worker owns its file, so there is nothing to lock.

use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use jyc_types::state_dir::resolve_state_dir;
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolOutput};

/// The lessons file, in the topic's state dir.
pub(crate) const LESSONS_FILE: &str = "lessons.md";

/// How much of the file the system prompt carries. The rest is the archive,
/// reachable with `lesson_read`. Small on purpose: this is paid on every
/// turn of every topic, and a lesson is a rule, not a transcript.
pub(crate) const INJECT_BYTES: usize = 2048;

/// One lesson is a rule, not an essay.
const MAX_LESSON_CHARS: usize = 1000;

/// What `lesson_read` returns when the file outgrew a comfortable read: the
/// tail, where the recent lessons are.
const READ_TAIL_BYTES: usize = 16 * 1024;

/// The header the file starts with.
const HEADER: &str = "# Lessons Learned";

/// The topic's `.jyc` dir, resolved the way `task_tools` resolves it:
/// `working_dir` is the topic directory and `current_topic` keys the
/// state-dir registry (which can point a topic's state elsewhere).
fn lessons_path(ctx: &ToolContext<'_>) -> PathBuf {
    resolve_state_dir(ctx.current_topic.as_deref().unwrap_or(""), ctx.working_dir)
        .join(LESSONS_FILE)
}

/// Today, as `YYYY-MM-DD`.
fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// The lesson text of one file line, without its date.
fn lesson_text(line: &str) -> &str {
    line.strip_prefix("- ")
        .and_then(|rest| rest.split_once(" — "))
        .map(|(_, text)| text.trim())
        .unwrap_or(line.trim())
}

/// The byte length of `content` up to [`INJECT_BYTES`], on a char boundary.
fn head_len(content: &str) -> usize {
    let mut end = INJECT_BYTES.min(content.len());
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// The byte offset where the last `bytes` of `content` start, on a char
/// boundary.
fn tail_start(content: &str, bytes: usize) -> usize {
    let mut start = content.len().saturating_sub(bytes);
    while start < content.len() && !content.is_char_boundary(start) {
        start += 1;
    }
    start
}

/// The lessons section for the system prompt: the head of the file, capped,
/// pointing at the rest. `None` when there is nothing to say.
pub(crate) fn prompt_section(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    // The file's own markdown header would repeat the section title.
    let body = content
        .strip_prefix(HEADER)
        .map(str::trim_start)
        .unwrap_or(content);
    if body.len() <= INJECT_BYTES {
        return Some(format!(
            "## Lessons Learned (from {LESSONS_FILE})\n\n{body}\n\n"
        ));
    }
    Some(format!(
        "## Lessons Learned (from {LESSONS_FILE})\n\n{}\n\n\
         The file is {} bytes and this is the first {INJECT_BYTES}. \
         Call `lesson_read` for the rest.\n\n",
        &body[..head_len(body)],
        body.len()
    ))
}

/// Record one lesson the agent learned.
pub struct LessonAddTool;

#[async_trait]
impl Tool for LessonAddTool {
    fn name(&self) -> &str {
        "lesson_add"
    }

    fn description(&self) -> &str {
        "Record one lesson you learned, in this topic's own `lessons.md`: a reusable \
         fact you want to keep for yourself — a trap you fell into, a fix that \
         worked, a rule for this repo or this user. Write it as one concrete rule \
         ('do X, because Y'), not as a story. The head of the file is injected into \
         your system prompt, so it is in view on every later turn; the rest is \
         reachable with `lesson_read`. Record a lesson when something non-obvious \
         is learned, not after every step."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "lesson": {
                    "type": "string",
                    "description": "One lesson, as a concrete reusable rule."
                }
            },
            "required": ["lesson"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let Some(raw) = input.get("lesson").and_then(Value::as_str) else {
            return Ok(ToolOutput::error("Missing 'lesson' parameter"));
        };
        // One line per lesson: the file is read back by a model, and a
        // multi-line entry would break the one-rule-per-line shape.
        let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.is_empty() {
            return Ok(ToolOutput::error("The lesson must not be empty"));
        }
        let mut lesson: String = flat.chars().take(MAX_LESSON_CHARS).collect();
        let truncated = flat.chars().count() > MAX_LESSON_CHARS;
        if truncated {
            lesson.push_str(" …");
        }

        let path = lessons_path(ctx);
        let mut content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        if content.trim().is_empty() {
            content = format!("{HEADER}\n\n");
        }
        // The same rule twice is noise, and a retried tool call must not
        // duplicate it.
        if content.lines().any(|line| lesson_text(line) == lesson) {
            return Ok(ToolOutput::success(format!("Already recorded: {lesson}")));
        }
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&format!("- {} — {lesson}\n", today()));

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to create the state dir: {e}"))?;
        }
        tokio::fs::write(&path, &content)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write {LESSONS_FILE}: {e}"))?;

        Ok(ToolOutput::success(format!(
            "Recorded in {LESSONS_FILE}: {lesson}{}",
            if truncated { " (truncated)" } else { "" }
        )))
    }
}

/// Read the lessons this topic has recorded.
pub struct LessonReadTool;

#[async_trait]
impl Tool for LessonReadTool {
    fn name(&self) -> &str {
        "lesson_read"
    }

    fn description(&self) -> &str {
        "Read this topic's recorded lessons (`lessons.md`). The head of the file is \
         already in your system prompt; this returns what the prompt could not \
         carry. Use it to reach the older lessons."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let path = lessons_path(ctx);
        let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        if content.trim().is_empty() {
            return Ok(ToolOutput::success(
                "No lessons recorded yet. Record one with `lesson_add`.",
            ));
        }
        if content.len() <= INJECT_BYTES {
            return Ok(ToolOutput::success(content));
        }
        // The prompt carries the head; this carries the rest, and when even
        // that is too much, the recent end of it.
        let rest = &content[head_len(&content)..];
        if rest.len() <= READ_TAIL_BYTES {
            return Ok(ToolOutput::success(format!(
                "The first {INJECT_BYTES} bytes are in your system prompt; the rest \
                 follows.\n\n{rest}"
            )));
        }
        let start = tail_start(rest, READ_TAIL_BYTES);
        Ok(ToolOutput::success(format!(
            "The first {INJECT_BYTES} bytes are in your system prompt; showing the \
             last {READ_TAIL_BYTES} of the remaining {} bytes.\n\n{}",
            rest.len(),
            &rest[start..]
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A context pointed at a throwaway topic dir. `current_topic` is set
    /// because `lessons_path` uses it as the state-dir registry key; each
    /// test passes its own topic name and registers it in-dir (no silent
    /// fallback — state must be registered before access).
    fn ctx_for<'a>(topic: &str, working_dir: &'a Path) -> ToolContext<'a> {
        jyc_types::state_dir::register(topic, &working_dir.join(".jyc"));
        let mut ctx = ToolContext::new(working_dir);
        ctx.current_topic = Some(topic.to_string());
        ctx
    }

    async fn add(topic: &str, dir: &Path, lesson: &str) -> ToolOutput {
        LessonAddTool
            .execute(json!({ "lesson": lesson }), &ctx_for(topic, dir))
            .await
            .unwrap()
    }

    async fn read(topic: &str, dir: &Path) -> ToolOutput {
        LessonReadTool
            .execute(json!({}), &ctx_for(topic, dir))
            .await
            .unwrap()
    }

    fn lessons_file(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(".jyc").join(LESSONS_FILE)).unwrap()
    }

    #[tokio::test]
    async fn add_creates_the_file_with_a_header_and_one_line() {
        let tmp = tempfile::tempdir().unwrap();
        let out = add(
            "lesson-add-creates",
            tmp.path(),
            "prefer `cargo check -p` over a workspace-wide check",
        )
        .await;
        assert!(!out.is_error, "{}", out.content);

        let content = lessons_file(tmp.path());
        assert!(content.starts_with("# Lessons Learned\n"), "{content}");
        assert_eq!(content.lines().filter(|l| l.starts_with("- ")).count(), 1);
        assert!(
            content.contains("prefer `cargo check -p` over a workspace-wide check"),
            "{content}"
        );
    }

    #[tokio::test]
    async fn add_dedupes_the_same_lesson() {
        let tmp = tempfile::tempdir().unwrap();
        add(
            "lesson-add-dedupes",
            tmp.path(),
            "run the local gate once per changeset",
        )
        .await;
        let out = add(
            "lesson-add-dedupes",
            tmp.path(),
            "run the local gate once per changeset",
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("Already recorded"), "{}", out.content);

        let content = lessons_file(tmp.path());
        assert_eq!(content.lines().filter(|l| l.starts_with("- ")).count(), 1);
    }

    #[tokio::test]
    async fn add_keeps_distinct_lessons_apart() {
        let tmp = tempfile::tempdir().unwrap();
        add("lesson-add-distinct", tmp.path(), "first rule").await;
        add("lesson-add-distinct", tmp.path(), "second rule").await;
        let content = lessons_file(tmp.path());
        assert_eq!(content.lines().filter(|l| l.starts_with("- ")).count(), 2);
    }

    #[tokio::test]
    async fn add_truncates_a_long_lesson() {
        let tmp = tempfile::tempdir().unwrap();
        let long = "x".repeat(MAX_LESSON_CHARS + 500);
        let out = add("lesson-add-truncates", tmp.path(), &long).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("truncated"), "{}", out.content);

        let content = lessons_file(tmp.path());
        let line = content.lines().find(|l| l.starts_with("- ")).unwrap();
        assert!(line.len() < long.len() + 100, "{}", line.len());
    }

    #[tokio::test]
    async fn add_rejects_an_empty_lesson() {
        let tmp = tempfile::tempdir().unwrap();
        let out = add("lesson-add-empty", tmp.path(), "   ").await;
        assert!(out.is_error, "{}", out.content);
        assert!(!tmp.path().join(".jyc").join(LESSONS_FILE).exists());
    }

    #[tokio::test]
    async fn read_reports_when_there_is_nothing_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let out = read("lesson-read-empty", tmp.path()).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("No lessons recorded yet"),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn read_returns_what_the_prompt_cannot_carry() {
        let tmp = tempfile::tempdir().unwrap();
        // One lesson is capped at 1000 chars, so it takes several to outgrow
        // the injected head — the file is written directly here.
        let path = tmp.path().join(".jyc").join(LESSONS_FILE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{HEADER}\n\n- {}\n- TAILMARKER\n",
                "y".repeat(INJECT_BYTES + 100)
            ),
        )
        .unwrap();

        let out = read("lesson-read-rest", tmp.path()).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("are in your system prompt"),
            "{}",
            out.content
        );
        assert!(out.content.contains("TAILMARKER"), "{}", out.content);
    }

    #[tokio::test]
    async fn read_shows_the_tail_when_the_file_is_huge() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".jyc").join(LESSONS_FILE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let huge = "z".repeat(INJECT_BYTES + READ_TAIL_BYTES + 1000);
        std::fs::write(&path, format!("{HEADER}\n\n- {huge}\n")).unwrap();

        let out = read("lesson-read-huge", tmp.path()).await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("showing the last"), "{}", out.content);
        assert!(
            out.content.len() < huge.len(),
            "the whole file should not come back: {}",
            out.content.len()
        );
    }

    #[test]
    fn prompt_section_is_none_without_lessons() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(prompt_section(&tmp.path().join(LESSONS_FILE)).is_none());
    }

    #[test]
    fn prompt_section_carries_the_head_and_points_at_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(LESSONS_FILE);
        std::fs::write(&path, format!("{HEADER}\n\n- a rule worth keeping\n")).unwrap();
        let section = prompt_section(&path).unwrap();
        assert!(section.contains("## Lessons Learned"), "{section}");
        assert!(section.contains("a rule worth keeping"), "{section}");
        assert!(!section.contains("# Lessons Learned\n\n-"), "{section}");
    }

    #[test]
    fn prompt_section_caps_the_head() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(LESSONS_FILE);
        let long = "q".repeat(INJECT_BYTES + 5000);
        std::fs::write(&path, format!("{HEADER}\n\n- {long}\n")).unwrap();
        let section = prompt_section(&path).unwrap();
        assert!(
            section.contains("Call `lesson_read` for the rest"),
            "{section}"
        );
        // The section itself stays small: the head, not the whole file.
        assert!(section.len() < INJECT_BYTES + 500, "{}", section.len());
    }
}
