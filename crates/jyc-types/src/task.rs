//! Agent-maintained per-topic task list.
//!
//! The agent creates one list per piece of work — a plan turned into items it
//! then marks done as it implements — so the plan survives context compression
//! and the user can watch progress. Persisted at `.jyc/tasks.json`; creating a
//! new list replaces the old one, and `/reset` / `/new` delete it.

use serde::{Deserialize, Serialize};

/// Where an item is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Not started.
    #[default]
    Pending,
    /// Being worked on right now.
    InProgress,
    /// Finished.
    Completed,
}

impl TaskStatus {
    /// Every status name, in display order. These are the names the agent
    /// passes to `task_update` and the ones [`TaskStatus`] (de)serializes to —
    /// a test pins the two together so the tool schema can't drift.
    pub const NAMES: &'static [&'static str] = &["pending", "in_progress", "completed"];

    /// ASCII marker for the `/info` and TUI renderers, matching the plain
    /// `+ / -` style of the changed-files section (and safe on every channel).
    pub fn marker(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "[ ]",
            TaskStatus::InProgress => "[~]",
            TaskStatus::Completed => "[x]",
        }
    }
}

/// One entry in the list. `id` is what `task_update` references: assigned by
/// `task_create`, stable until the list is created again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskItem {
    pub id: u32,
    pub text: String,
    #[serde(default)]
    pub status: TaskStatus,
}

/// The topic's current task list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskList {
    #[serde(default)]
    pub items: Vec<TaskItem>,
}

impl TaskList {
    /// Whether there is anything to show.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Items completed, out of how many.
    pub fn progress(&self) -> (usize, usize) {
        let done = self
            .items
            .iter()
            .filter(|i| i.status == TaskStatus::Completed)
            .count();
        (done, self.items.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TaskStatus::NAMES` is what the tool schema advertises; serde is what
    /// the file and the inspect API carry. They must be the same strings.
    #[test]
    fn names_match_serde_representation() {
        for name in TaskStatus::NAMES {
            let status: TaskStatus =
                serde_json::from_str(&format!("\"{name}\"")).expect("name should deserialize");
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{name}\""),
                "round-trip should be stable"
            );
        }
    }

    #[test]
    fn progress_counts_completed_only() {
        let list = TaskList {
            items: vec![
                TaskItem {
                    id: 1,
                    text: "a".into(),
                    status: TaskStatus::Completed,
                },
                TaskItem {
                    id: 2,
                    text: "b".into(),
                    status: TaskStatus::InProgress,
                },
                TaskItem {
                    id: 3,
                    text: "c".into(),
                    status: TaskStatus::Pending,
                },
            ],
        };
        assert_eq!(list.progress(), (1, 3));
        assert!(!list.is_empty());
        assert!(TaskList::default().is_empty());
    }

    /// An item written before `status` existed (or written by hand) still
    /// loads, defaulting to pending.
    #[test]
    fn item_status_defaults_to_pending() {
        let list: TaskList = serde_json::from_str(r#"{"items":[{"id":1,"text":"a"}]}"#).unwrap();
        assert_eq!(list.items[0].status, TaskStatus::Pending);
    }
}
