//! Task list tools — the agent's plan for the work in this topic.
//!
//! The agent writes a list up front (`task_create`), then keeps it current as
//! it works (`task_update`), so the plan survives context compression and the
//! user can watch progress in the TUI topic-info pane and `/info`. One list per
//! topic: creating a new one replaces the old, and `/reset` / `/new` clear it.
//! The file is `.jyc/tasks.json`, read and written only through
//! [`jyc_core::session_state`] so every view sees the same thing. Each topic's
//! worker owns its file, so there is nothing to lock.

use anyhow::Result;
use async_trait::async_trait;
use jyc_core::session_state::{read_tasks_at, write_tasks_at};
use jyc_types::state_dir::jyc_dir;
use jyc_types::task::{TaskItem, TaskList, TaskStatus};
use serde_json::{Value, json};
use std::path::PathBuf;

use super::super::{Tool, ToolContext, ToolOutput};

/// Max items in one list — a plan is a handful of steps, not a transcript.
const MAX_ITEMS: usize = 20;

/// Max characters per item (every item renders as one line).
const MAX_ITEM_CHARS: usize = 120;

/// The topic's `.jyc` dir, resolved the way `job_tools` resolves it:
/// `working_dir` is the topic directory and `current_topic` keys the state-dir
/// registry (which can point a topic's state elsewhere).
fn tasks_dir(ctx: &ToolContext<'_>) -> PathBuf {
    jyc_dir(ctx.current_topic.as_deref().unwrap_or(""), ctx.working_dir)
}

/// Render the list the way the user sees it in `/info` and the TUI pane:
/// `Tasks (2/5):` then one marked, numbered line per item. The ids shown are
/// the ones `task_update` takes.
fn render(list: &TaskList) -> String {
    let (done, total) = list.progress();
    let mut out = format!("Tasks ({done}/{total}):\n");
    for item in &list.items {
        out.push_str(&format!(
            "  {} {}. {}\n",
            item.status.marker(),
            item.id,
            item.text
        ));
    }
    out
}

/// Show this topic's current task list.
pub struct TaskListTool;

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }

    fn description(&self) -> &str {
        "Show this topic's task list: the plan for the work in progress, one \
         line per item with the id `task_update` takes. Read it before updating \
         an item you did not just create, and again after a context reset. Says \
         so when there is no list yet — create one with `task_create`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        Ok(match read_tasks_at(&tasks_dir(ctx)).await {
            Some(list) => ToolOutput::success(render(&list)),
            None => ToolOutput::success(
                "No task list for this topic yet. Create one with `task_create` \
                 before starting work that takes more than one step.",
            ),
        })
    }
}

/// Create the task list for the current piece of work.
pub struct TaskCreateTool;

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "task_create"
    }

    fn description(&self) -> &str {
        "Create this topic's task list, replacing any existing one. Write it up \
         front whenever the work needs more than one step (a plan, an \
         implementation, a fix), then keep it current with `task_update`: \
         `in_progress` when you start an item, `completed` when it is actually \
         done. Items are numbered in the order given and the ids stay stable \
         until you create a new list. Returns the rendered list."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": format!(
                        "Ordered task texts, one line each (1..={MAX_ITEMS} items, {MAX_ITEM_CHARS} chars max)."
                    ),
                }
            },
            "required": ["items"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let raw = input
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let texts: Vec<&str> = raw
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect();

        if texts.is_empty() {
            return Ok(ToolOutput::error(
                "`items` must be a non-empty array of task texts.",
            ));
        }
        if texts.len() > MAX_ITEMS {
            return Ok(ToolOutput::error(format!(
                "{} items is too many (max {MAX_ITEMS}) — group the work into fewer steps.",
                texts.len()
            )));
        }
        if let Some(long) = texts.iter().find(|t| t.chars().count() > MAX_ITEM_CHARS) {
            return Ok(ToolOutput::error(format!(
                "task text is {} chars, over the {MAX_ITEM_CHARS} limit: {long}",
                long.chars().count()
            )));
        }

        let list = TaskList {
            items: texts
                .iter()
                .enumerate()
                .map(|(i, text)| TaskItem {
                    id: (i + 1) as u32,
                    text: (*text).to_string(),
                    status: TaskStatus::Pending,
                })
                .collect(),
        };
        write_tasks_at(&tasks_dir(ctx), &list).await?;
        Ok(ToolOutput::success(render(&list)))
    }
}

/// Set the status of one item in the task list.
pub struct TaskUpdateTool;

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        "task_update"
    }

    fn description(&self) -> &str {
        "Set one item's status in this topic's task list: `pending`, \
         `in_progress`, or `completed`. Mark an item `in_progress` when you \
         start it and `completed` only once it is genuinely done — never \
         optimistically, and never with validation still failing. `task_list` \
         shows the ids; `task_create` replaces the whole list. Returns the \
         rendered list."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "integer",
                    "description": "Item id, as shown by `task_list` or `task_create`.",
                },
                "status": {
                    "type": "string",
                    "enum": TaskStatus::NAMES,
                    "description": "New status for the item.",
                },
            },
            "required": ["id", "status"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let Some(id) = input.get("id").and_then(Value::as_u64) else {
            return Ok(ToolOutput::error("`id` (integer) is required."));
        };
        let status = match input.get("status").and_then(Value::as_str) {
            Some(raw) => match serde_json::from_value::<TaskStatus>(json!(raw.trim())) {
                Ok(status) => status,
                Err(_) => {
                    return Ok(ToolOutput::error(format!(
                        "unknown status `{raw}` — expected one of {:?}",
                        TaskStatus::NAMES
                    )));
                }
            },
            None => return Ok(ToolOutput::error("`status` is required.")),
        };
        let Some(mut list) = read_tasks_at(&tasks_dir(ctx)).await else {
            return Ok(ToolOutput::error(
                "This topic has no task list — create one with `task_create` first.",
            ));
        };
        let Some(item) = list.items.iter_mut().find(|i| i.id as u64 == id) else {
            let valid: Vec<u32> = list.items.iter().map(|i| i.id).collect();
            return Ok(ToolOutput::error(format!(
                "no task id {id} on this topic (valid: {valid:?})."
            )));
        };
        item.status = status;
        write_tasks_at(&tasks_dir(ctx), &list).await?;
        Ok(ToolOutput::success(render(&list)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A context pointed at a throwaway topic dir. `current_topic` is set
    /// because `tasks_dir` uses it as the state-dir registry key; the registry
    /// has no entry for it in tests, so the dir falls back to
    /// `<working_dir>/.jyc`.
    fn ctx_for(working_dir: &std::path::Path) -> ToolContext<'_> {
        let mut ctx = ToolContext::new(working_dir);
        ctx.current_topic = Some("tasks-test".to_string());
        ctx
    }

    async fn create(dir: &std::path::Path, items: Vec<&str>) -> ToolOutput {
        TaskCreateTool
            .execute(json!({ "items": items }), &ctx_for(dir))
            .await
            .unwrap()
    }

    async fn update(dir: &std::path::Path, id: u64, status: &str) -> ToolOutput {
        TaskUpdateTool
            .execute(json!({ "id": id, "status": status }), &ctx_for(dir))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_then_update_roundtrips_through_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let out = create(tmp.path(), vec!["first step", "second step"]).await;
        assert!(!out.is_error);
        assert!(out.content.contains("Tasks (0/2):"), "{}", out.content);

        let out = update(tmp.path(), 2, "in_progress").await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("  [~] 2. second step"));

        let out = update(tmp.path(), 2, "completed").await;
        assert!(out.content.contains("Tasks (1/2):"), "{}", out.content);
        assert!(out.content.contains("  [x] 2. second step"));

        // The file — what `/info` and the TUI pane read — carries the same state.
        let stored = read_tasks_at(&tasks_dir(&ctx_for(tmp.path())))
            .await
            .unwrap();
        assert_eq!(stored.progress(), (1, 2));
        assert_eq!(stored.items[0].status, TaskStatus::Pending);
    }

    #[tokio::test]
    async fn create_replaces_the_previous_list() {
        let tmp = tempfile::tempdir().unwrap();
        create(tmp.path(), vec!["a", "b", "c"]).await;
        update(tmp.path(), 1, "completed").await;

        let out = create(tmp.path(), vec!["fresh start"]).await;
        assert!(out.content.contains("Tasks (0/1):"), "{}", out.content);
        assert!(out.content.contains("  [ ] 1. fresh start"));

        // The old ids are gone with the old list.
        let out = update(tmp.path(), 2, "completed").await;
        assert!(out.is_error);
        assert!(out.content.contains("valid: [1]"), "{}", out.content);
    }

    #[tokio::test]
    async fn update_without_a_list_says_create_one() {
        let tmp = tempfile::tempdir().unwrap();
        let out = update(tmp.path(), 1, "completed").await;
        assert!(out.is_error);
        assert!(out.content.contains("task_create"), "{}", out.content);
    }

    #[tokio::test]
    async fn list_tool_reports_an_absent_list() {
        let tmp = tempfile::tempdir().unwrap();
        let out = TaskListTool
            .execute(json!({}), &ctx_for(tmp.path()))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("No task list"), "{}", out.content);

        create(tmp.path(), vec!["only step"]).await;
        let out = TaskListTool
            .execute(json!({}), &ctx_for(tmp.path()))
            .await
            .unwrap();
        assert!(out.content.contains("Tasks (0/1)"), "{}", out.content);
    }

    #[tokio::test]
    async fn validation_rejects_bad_input() {
        let tmp = tempfile::tempdir().unwrap();

        let out = create(tmp.path(), vec![]).await;
        assert!(out.is_error);
        let out = TaskCreateTool
            .execute(json!({ "items": ["   ", ""] }), &ctx_for(tmp.path()))
            .await
            .unwrap();
        assert!(out.is_error, "blank items must not count");

        let too_many = vec!["step"; MAX_ITEMS + 1];
        let out = create(tmp.path(), too_many).await;
        assert!(out.is_error);
        assert!(out.content.contains(&MAX_ITEMS.to_string()));

        let out = create(tmp.path(), vec!["x".repeat(MAX_ITEM_CHARS + 1).as_str()]).await;
        assert!(out.is_error);
        assert!(out.content.contains("chars"), "{}", out.content);

        let out = update(tmp.path(), 1, "done").await;
        assert!(out.is_error);
        assert!(out.content.contains("unknown status"), "{}", out.content);
    }

    #[tokio::test]
    async fn status_names_in_the_schema_are_accepted() {
        // The schema advertises `TaskStatus::NAMES`; anything it lists must
        // parse, or the model can be told a value the tool then rejects.
        let tmp = tempfile::tempdir().unwrap();
        create(tmp.path(), vec!["step"]).await;
        for name in TaskStatus::NAMES {
            let out = update(tmp.path(), 1, name).await;
            assert!(!out.is_error, "{name} should be accepted: {}", out.content);
        }
    }
}
