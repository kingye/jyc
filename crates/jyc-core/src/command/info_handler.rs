use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use jyc_types::{ChangeKind, TopicInfo, format_amount};

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::duration::{DurationStyle, format_duration_ms};
use crate::topic_manager::TopicManager;

/// /info command — show this topic's info, mirroring the dashboard chat
/// screen's Topic Info pane (name, channel, pattern, model, mode, branch,
/// token usage, call latency, cost, changed files).
///
/// Data comes from `TopicManager::list_topics` — the same snapshot the
/// dashboard renders — so channel users see exactly what the TUI shows, and
/// the topic is picked by name, narrowed by the channel it arrived on, through
/// the shared `jyc_types::find_topic`: several topics can share one
/// `topic_path`. Works on every channel: the result is delivered as a normal
/// command reply.
pub struct InfoCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl InfoCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }
}

#[async_trait]
impl CommandHandler for InfoCommandHandler {
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let topics = self.topic_manager.list_topics().await;
        match topic_for(&topics, &context) {
            Some(t) => Ok(CommandResult {
                success: true,
                message: format_topic_info(t),
                ..Default::default()
            }),
            None => Ok(CommandResult {
                success: false,
                message: "/info: topic not found".into(),
                error: Some(format!(
                    "No topic named '{}' registered on channel {}",
                    context.topic_name, context.channel
                )),
                ..Default::default()
            }),
        }
    }
}

/// The topic this command is about: `context.topic_name` through the shared
/// lookup, narrowed by the channel this command arrived on.
///
/// Never by `topic_path`: children that share their parent's directory
/// (`share_topic_path`, e.g. `ci-watch-*`) carry the same path as the parent,
/// and selecting by path reported whichever of them came first in the list.
fn topic_for<'a>(topics: &'a [TopicInfo], context: &CommandContext) -> Option<&'a TopicInfo> {
    jyc_types::find_topic(topics, &context.topic_name, Some(&context.channel))
}

/// Render the pane's rows as plain text, one per line, with the same
/// omission rules as the TUI: missing data skips the row entirely (no
/// `Model:` line on the default model, no `Cache create:` line at zero).
fn format_topic_info(t: &TopicInfo) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Topic: {}", t.name));
    lines.push(format!("Channel: {}", t.channel));
    lines.push(format!("Pattern: {}", t.pattern.as_deref().unwrap_or("-")));
    if let Some(model) = &t.model {
        lines.push(format!("Model: {model}"));
    }
    lines.push(format!("Mode: {}", t.mode.as_deref().unwrap_or("build")));
    if let Some(branch) = &t.branch {
        lines.push(format!("Branch: {branch}"));
    }
    lines.push(format!("Status: {}", t.status));
    if let (Some(cur), Some(max)) = (t.context_input_tokens, t.max_tokens) {
        let pct = cur
            .checked_mul(100)
            .and_then(|v| v.checked_div(max))
            .unwrap_or(0);
        lines.push(format!("Tokens: {cur} / {max} ({pct}%)"));
    }
    if let Some(output) = t.output_tokens {
        lines.push(format!("Output: {output}"));
    }
    if let Some(total) = t.total_input_tokens {
        lines.push(format!("Total input: {total}"));
    }
    if let Some(hits) = t.total_cache_hit_tokens {
        lines.push(format!("Cache hits: {hits}"));
    }
    if let Some(creation) = t.total_cache_creation_tokens
        && creation > 0
    {
        lines.push(format!("Cache create: {creation}"));
    }
    if let Some(reasoning) = t.total_reasoning_tokens
        && reasoning > 0
    {
        lines.push(format!("Reasoning: {reasoning}"));
    }
    if let Some(cost) = &t.cost {
        lines.push(format!(
            "Cost: {} session · {} today",
            format_amount(cost.session, &cost.currency),
            format_amount(cost.today, &cost.currency)
        ));
    }
    // The session's average wall-clock time per LLM call. Same fields,
    // same words and the same shared duration formatter as the dashboard
    // panes' row — the sentence itself is spelled out in both places, as
    // the cost row already was.
    if let (Some(avg), Some(calls)) = (t.avg_call_ms, t.call_count) {
        lines.push(format!(
            "Latency: {}/call (avg of {calls})",
            format_duration_ms(avg, DurationStyle::Millis),
        ));
    }
    // Agent task list, through the same renderer the tools return, so the ids
    // the model prints are the ids the user reads. Omitted when there is none.
    if !t.tasks.is_empty() {
        lines.extend(t.tasks.render_lines());
    }
    if let Some(files) = t.changed_files.as_deref() {
        if files.is_empty() {
            lines.push("Files: (none)".to_string());
        } else {
            lines.push(format!("Files ({}):", files.len()));
            for f in files {
                let prefix = match f.change {
                    ChangeKind::Added => "+ ",
                    ChangeKind::Deleted => "- ",
                    ChangeKind::Modified => "  ",
                };
                // The pane paints uncommitted entries yellow; plain text
                // gets a trailing `*` marker instead. Counts sit before
                // that marker so the marker still ends the line.
                let marker = if f.uncommitted { " *" } else { "" };
                let counts = match f.lines {
                    Some(lines) => format!(" +{} -{}", lines.insertions, lines.deletions),
                    None => String::new(),
                };
                lines.push(format!("{prefix}{}{counts}{marker}", f.path));
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::task::{TaskItem, TaskList, TaskStatus};
    use jyc_types::{ChangedFileEntry, LineStats, TopicCost, TopicStatus};
    use std::path::PathBuf;

    /// A task list with sequential ids, the way `task_create` assigns them.
    fn task_list(items: &[(&str, TaskStatus)]) -> TaskList {
        TaskList {
            items: items
                .iter()
                .enumerate()
                .map(|(i, (text, status))| TaskItem {
                    id: (i + 1) as u32,
                    text: (*text).to_string(),
                    status: *status,
                })
                .collect(),
        }
    }

    fn full_info() -> TopicInfo {
        TopicInfo {
            tasks: task_list(&[
                ("inspect list_topics", TaskStatus::Completed),
                ("wire the pane", TaskStatus::InProgress),
                ("ship it", TaskStatus::Pending),
            ]),
            name: "issue-42".into(),
            channel: "feishu".into(),
            pattern: Some("gh-issue".into()),
            status: TopicStatus::Idle,
            model: Some("kimi/k3-256k".into()),
            mode: Some("plan".into()),
            context_input_tokens: Some(105_432),
            max_tokens: Some(256_000),
            output_tokens: Some(12_345),
            total_input_tokens: Some(1_234_567),
            total_cache_hit_tokens: Some(987_654),
            total_cache_creation_tokens: Some(111),
            total_reasoning_tokens: Some(4_096),
            last_active_at: None,
            skills: vec![],
            available_skills: vec![],
            recent_messages: vec![],
            thinking_text: None,
            activity: vec![],
            topic_path: Some(PathBuf::from("/tmp/topic")),
            branch: Some("fix/issue-42-timeout".into()),
            changed_files: Some(vec![
                ChangedFileEntry {
                    path: "src/new.rs".into(),
                    uncommitted: false,
                    change: ChangeKind::Added,
                    lines: Some(LineStats {
                        insertions: 12,
                        deletions: 0,
                    }),
                },
                ChangedFileEntry {
                    path: "src/edited.rs".into(),
                    uncommitted: true,
                    change: ChangeKind::Modified,
                    // Binary: git reports no counts.
                    lines: None,
                },
            ]),
            cost: Some(TopicCost {
                session: 0.42,
                today: 1.23,
                currency: "USD".into(),
            }),
            avg_call_ms: Some(820),
            call_count: Some(37),
            commands: vec![],
        }
    }

    /// The reported bug: a parent and two children that shared its directory
    /// (`share_topic_path`) all carried `/home/jiny/projects/jyc`, and `/info`
    /// asked in `jyc` answered with `ci-watch-925` — the first topic whose
    /// path matched, in list order.
    #[test]
    fn selects_by_name_when_topics_share_a_path() {
        let shared = PathBuf::from("/home/jiny/projects/jyc");
        let agent_topic = |name: &str| {
            let mut t = full_info();
            t.name = name.to_string();
            t.channel = "agents".to_string();
            t.topic_path = Some(shared.clone());
            t
        };
        let topics = vec![
            agent_topic("ci-watch-925"),
            agent_topic("jyc"),
            agent_topic("review-prompt-names"),
        ];
        let context = |name: &str| CommandContext {
            channel: "agents".to_string(),
            topic_name: name.to_string(),
            topic_path: shared.clone(),
            ..Default::default()
        };

        assert_eq!(
            topic_for(&topics, &context("jyc")).map(|t| t.name.as_str()),
            Some("jyc")
        );
        assert_eq!(
            topic_for(&topics, &context("ci-watch-925")).map(|t| t.name.as_str()),
            Some("ci-watch-925")
        );
        assert!(topic_for(&topics, &context("nobody")).is_none());
    }

    #[test]
    fn formats_all_rows_when_data_present() {
        let text = format_topic_info(&full_info());
        let expected = "\
Topic: issue-42
Channel: feishu
Pattern: gh-issue
Model: kimi/k3-256k
Mode: plan
Branch: fix/issue-42-timeout
Status: Idle
Tokens: 105432 / 256000 (41%)
Output: 12345
Total input: 1234567
Cache hits: 987654
Cache create: 111
Reasoning: 4096
Cost: $0.4200 session · $1.2300 today
Latency: 820ms/call (avg of 37)
Tasks (1/3):
  [✓] 1. inspect list_topics
  [~] 2. wire the pane
  [ ] 3. ship it
Files (2):
+ src/new.rs +12 -0
  src/edited.rs *";
        assert_eq!(text, expected);
    }

    #[test]
    fn omits_missing_rows_like_the_pane() {
        let mut t = full_info();
        t.model = None;
        t.mode = None;
        t.branch = None;
        t.context_input_tokens = None;
        t.max_tokens = None;
        t.output_tokens = None;
        t.total_input_tokens = None;
        t.total_cache_hit_tokens = None;
        t.total_cache_creation_tokens = None;
        t.total_reasoning_tokens = None;
        t.changed_files = None;
        t.cost = None;
        t.avg_call_ms = None;
        t.call_count = None;
        t.pattern = None;
        t.tasks = TaskList::default();
        let text = format_topic_info(&t);
        assert_eq!(
            text,
            "Topic: issue-42\nChannel: feishu\nPattern: -\nMode: build\nStatus: Idle"
        );
    }

    #[test]
    fn cache_create_row_hidden_at_zero() {
        let mut t = full_info();
        t.total_cache_creation_tokens = Some(0);
        assert!(!format_topic_info(&t).contains("Cache create"));
    }

    #[test]
    fn no_task_list_omits_the_section() {
        let mut t = full_info();
        t.tasks = TaskList::default();
        let text = format_topic_info(&t);
        assert!(
            !text.contains("Tasks"),
            "empty list must not leave a header behind:\n{text}"
        );
        // Still renders the neighbours, so the omission is local.
        assert!(text.contains("Cost:"));
        assert!(text.contains("Files (2):"));
    }

    #[test]
    fn empty_files_list_shows_none() {
        let mut t = full_info();
        t.changed_files = Some(vec![]);
        assert!(format_topic_info(&t).contains("Files: (none)"));
    }
}
