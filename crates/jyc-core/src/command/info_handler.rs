use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use jyc_types::{ChangeKind, TopicInfo};

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::duration::format_latency;
use crate::topic_manager::TopicManager;
use crate::topic_manager::topic_tree;

/// The indent of `/info`'s context-composition legend rows under the
/// `Context:` header. The pane indents them to the bar's first cell
/// instead, which has no meaning in plain text — there is no bar here.
const INFO_CONTEXT_LEGEND_INDENT: usize = 2;

/// /info command — show this topic's info, mirroring the dashboard chat
/// screen's Topic Info pane (name, pattern, model, mode, branch, status,
/// token usage, call latency, cost, changed files).
///
/// Data comes from `TopicManager::topic_info` — the same call the
/// dashboard's per-topic endpoint serves
/// (`GET /api/topics/{channel}/{topic}/info`) — so the two surfaces read
/// one source and cannot drift apart. The manager is the channel's own, so
/// the topic is picked by name within that channel; several topics can
/// share one `topic_path` (children that share their parent's directory),
/// and the name is what tells them apart.
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
        match self
            .topic_manager
            .topic_info(&context.topic_name, true)
            .await
        {
            Some(t) => Ok(CommandResult {
                success: true,
                message: format_topic_info(&t),
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

/// Render the pane's rows as plain text, one per line, with the same
/// omission rules as the TUI: missing data skips the row entirely (no
/// `Model:` line on the default model, no `Create:` line at zero).
fn format_topic_info(t: &TopicInfo) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Topic: {}", t.name));
    lines.push(format!("Pattern: {}", t.pattern.as_deref().unwrap_or("-")));
    if let Some(model) = &t.model {
        lines.push(format!("Model: {model}"));
    }
    lines.push(format!("Mode: {}", t.mode.as_deref().unwrap_or("build")));
    if let Some(branch) = &t.branch {
        lines.push(format!("Branch: {branch}"));
    }
    // The topic's live status, with the backlog its queue holds: the
    // figures the pane's status row carries beside the blocks. Omitted
    // when the source reports no capacity — there is no queue to report
    // then.
    if t.queue_capacity > 0 {
        lines.push(format!(
            "Status: {} · queue {} / {}",
            t.status, t.queued, t.queue_capacity
        ));
    } else {
        lines.push(format!("Status: {}", t.status));
    }
    if let (Some(cur), Some(max)) = (t.context_input_tokens, t.max_tokens) {
        // The context size: the last call's input plus its output, against
        // the model's context window.
        let used = jyc_types::context_tokens(Some(cur), t.context_output_tokens).unwrap_or(cur);
        let pct = jyc_types::context_token_pct(Some(cur), t.context_output_tokens, Some(max))
            .unwrap_or(0);
        lines.push(format!(
            "Tokens: {} / {} ({pct}%)",
            jyc_types::format_token_count(used),
            jyc_types::format_token_count(max)
        ));
    }
    if !t.context_composition.is_empty() {
        let total = jyc_types::context_composition_total(&t.context_composition);
        let label_width = jyc_types::context_composition_label_width(&t.context_composition);
        lines.push("Context:".to_string());
        for region in &t.context_composition {
            lines.push(format!(
                "{}{}",
                jyc_types::format_context_region_label(
                    region.region,
                    label_width,
                    INFO_CONTEXT_LEGEND_INDENT
                ),
                jyc_types::format_context_region_share(region, total),
            ));
        }
        // The turn's depth in its own legend: how many LLM calls it has
        // made, one per iteration of the agent loop. The same field the
        // pane reads, from the same `TopicInfo`.
        if let Some(iteration) = t.turn_call_count {
            lines.push(jyc_types::format_iteration_row(
                iteration,
                label_width,
                INFO_CONTEXT_LEGEND_INDENT,
            ));
        }
    }
    if let Some(figures) = jyc_types::format_output_pair(t.output_tokens, t.turn_output_tokens) {
        lines.push(format!("Output: {figures}"));
    }
    // The cache group: the turn's rate, the session's rate and the cache's
    // write volume, under one header — the same grouping the pane draws.
    // Each rate carries its own hits, input and share — the same string
    // the pane's row shows, from the same shared formatter. The header
    // appears only when a row does.
    let mut cache_lines: Vec<String> = Vec::new();
    if let Some(rate) = jyc_types::format_cache_rate(t.turn_cache_hit_tokens, t.turn_input_tokens) {
        cache_lines.push(format!("  Turn: {rate}"));
    }
    if let Some(rate) = jyc_types::format_cache_rate(t.total_cache_hit_tokens, t.total_input_tokens)
    {
        cache_lines.push(format!("  Session: {rate}"));
    }
    if let Some(creation) = t.total_cache_creation_tokens
        && creation > 0
    {
        cache_lines.push(format!(
            "  Create: {}",
            jyc_types::format_token_count(creation)
        ));
    }
    if !cache_lines.is_empty() {
        lines.push("Cache:".to_string());
        lines.extend(cache_lines);
    }
    if let Some(figures) =
        jyc_types::format_output_pair(t.total_reasoning_tokens, t.turn_reasoning_tokens)
    {
        lines.push(format!("Reasoning: {figures}"));
    }
    if let Some(cost) = &t.cost {
        lines.push(format!("Cost: {}", jyc_types::format_cost_pair(cost)));
    }
    // The day's spend across every topic, per currency — the same figure
    // `/bill` reports.
    if let Some(totals) = jyc_types::format_currency_totals(&t.today_all) {
        lines.push(format!("Today (all): {totals}"));
    }
    // Wall-clock time per LLM call: the latest turn's average first, the
    // session's second — same fields, same words and the same shared
    // formatter as the dashboard panes' row (`format_latency`), so the two
    // surfaces cannot drift.
    let turn = t.avg_turn_call_ms.zip(t.turn_call_count);
    let session = t.avg_call_ms.zip(t.call_count);
    if let Some(latency) = format_latency(turn, session) {
        lines.push(format!("Latency: {latency}"));
    }
    // Agent task list, through the same renderer the tools return, so the ids
    // the model prints are the ids the user reads. Omitted when there is none.
    if !t.tasks.is_empty() {
        lines.extend(t.tasks.render_lines());
    }
    // The subagents this topic spawned, as a tree — the same tree the info
    // pane draws, out of the same detail, printed as text.
    if let Some(subagents) = t.subagents.as_deref()
        && !subagents.is_empty()
    {
        lines.push(format!(
            "Subagents ({}):",
            topic_tree::node_count(subagents)
        ));
        lines.extend(subagent_lines(subagents, 1));
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

/// The subagent tree as text: one line per topic, indented by its depth.
fn subagent_lines(nodes: &[jyc_types::TopicNode], depth: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for node in nodes {
        lines.push(format!(
            "{}{} ({})",
            "  ".repeat(depth),
            node.name,
            node.status
        ));
        lines.extend(subagent_lines(&node.children, depth + 1));
    }
    lines
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
            queued: 2,
            queue_capacity: 10,
            parent: None,
            model: Some("kimi/k3-256k".into()),
            mode: Some("plan".into()),
            context_input_tokens: Some(105_432),
            context_output_tokens: Some(2_048),
            max_tokens: Some(256_000),
            context_composition: Vec::new(),
            output_tokens: Some(12_345),
            turn_output_tokens: Some(2_048),
            total_input_tokens: Some(1_234_567),
            total_cache_hit_tokens: Some(987_654),
            total_cache_creation_tokens: Some(111),
            total_reasoning_tokens: Some(4_096),
            turn_reasoning_tokens: Some(512),
            turn_cache_hit_tokens: None,
            turn_input_tokens: None,
            today_all: Vec::new(),
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
            avg_turn_call_ms: Some(1100),
            turn_call_count: Some(3),
            commands: vec![],
            subagents: None,
        }
    }

    #[test]
    fn formats_all_rows_when_data_present() {
        let text = format_topic_info(&full_info());
        let expected = "\
Topic: issue-42
Pattern: gh-issue
Model: kimi/k3-256k
Mode: plan
Branch: fix/issue-42-timeout
Status: Idle · queue 2 / 10
Tokens: 104.96K / 250.00K (41%)
Output: 2.00K turn · 12.06K session
Cache:
  Session: 964.51K / 1.18M (80%)
  Create: 111
Reasoning: 512 turn · 4.00K session
Cost: $1.23 today · $0.42 session
Latency: 1.10s/call (3) · 820ms/call (37)
Tasks (1/3):
  [✓] 1. inspect list_topics
  [~] 2. wire the pane
  [ ] 3. ship it
Files (2):
+ src/new.rs +12 -0
  src/edited.rs *";
        assert_eq!(text, expected);
    }

    /// The turn's own cache row stands next to the session's, so a long
    /// turn's rate is visible without waiting for the session's to move.
    #[test]
    fn formats_the_turn_cache_row_when_the_turn_has_called() {
        let mut t = full_info();
        t.turn_cache_hit_tokens = Some(300);
        t.turn_input_tokens = Some(1_000);
        let text = format_topic_info(&t);
        assert!(text.contains("  Turn: 300 / 1000 (30%)"), "{text}");
        assert!(text.contains("  Session: 964.51K / 1.18M (80%)"), "{text}");
    }

    /// The day's spend across every topic, per currency.
    #[test]
    fn formats_the_days_totals_when_the_ledger_has_entries() {
        let mut t = full_info();
        t.today_all = vec![("USD".to_string(), 12.34), ("CNY".to_string(), 3.2)];
        let text = format_topic_info(&t);
        assert!(text.contains("Today (all): $12.34 · ¥3.20"), "{text}");
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
        // A turn figure alone does not make the row: it is a share of the
        // session's output, and with no session total there is no row.
        t.turn_output_tokens = Some(2_048);
        t.total_input_tokens = None;
        t.total_cache_hit_tokens = None;
        t.total_cache_creation_tokens = None;
        t.total_reasoning_tokens = None;
        t.turn_reasoning_tokens = None;
        t.changed_files = None;
        t.cost = None;
        t.avg_call_ms = None;
        t.call_count = None;
        t.avg_turn_call_ms = None;
        t.turn_call_count = None;
        t.pattern = None;
        t.tasks = TaskList::default();
        t.queue_capacity = 0;
        let text = format_topic_info(&t);
        assert_eq!(
            text,
            "Topic: issue-42\nPattern: -\nMode: build\nStatus: Idle"
        );
    }

    /// The queue's backlog, against the queue's capacity, on the status
    /// row — the same row the pane's status row carries it on.
    #[test]
    fn formats_the_queue_figures_on_the_status_row() {
        let text = format_topic_info(&full_info());
        assert!(text.contains("Status: Idle · queue 2 / 10"), "{text}");
        assert!(!text.contains("Queue:"), "{text}");
    }

    /// A topic whose source reports no capacity has no queue to report:
    /// the status row is the status word alone.
    #[test]
    fn omits_the_queue_without_a_capacity() {
        let mut t = full_info();
        t.queue_capacity = 0;
        let text = format_topic_info(&t);
        assert!(!text.contains("queue"), "{text}");
        assert!(text.contains("Status: Idle\n"), "{text}");
    }

    /// The session-only fallback through the real wiring: a turn that has
    /// measured no call yet (just started, or the session was reset) leaves
    /// one row, with no turn figure in it.
    #[test]
    fn latency_row_falls_back_to_session_with_no_turn_figure() {
        let mut t = full_info();
        t.avg_turn_call_ms = None;
        t.turn_call_count = None;
        let text = format_topic_info(&t);
        assert!(text.contains("Latency: 820ms/call (37)\n"));
    }

    #[test]
    fn cache_create_row_hidden_at_zero() {
        let mut t = full_info();
        t.total_cache_creation_tokens = Some(0);
        assert!(!format_topic_info(&t).contains("Create:"));
    }

    /// The composition sits between the context size and the output, so
    /// the caller reads what filled the context before what it produced.
    #[test]
    fn formats_context_composition_before_output() {
        let mut t = full_info();
        t.context_composition = vec![
            jyc_types::ContextRegionInfo {
                region: jyc_types::ContextRegion::History,
                messages: 3,
                bytes: 512,
                estimated_tokens: 60,
            },
            jyc_types::ContextRegionInfo {
                region: jyc_types::ContextRegion::Current,
                messages: 2,
                bytes: 256,
                estimated_tokens: 40,
            },
        ];
        let text = format_topic_info(&t);
        let section = concat!(
            "Context:\n",
            "  History      ~60 · 60%\n",
            "  Current turn ~40 · 40%\n",
            "  Iteration    3\n",
        );
        let composition = text.find(section).expect("the composition section");
        let output = text.find("Output:").expect("the output row");
        assert!(composition < output, "{text}");
    }

    /// The legend's last row is the turn's depth, and it is `TopicInfo`'s
    /// own figure — the same one the pane reads, so the two cannot report
    /// different counts. Without it the row is absent rather than zero: a
    /// turn that has not called yet has no depth to state.
    #[test]
    fn formats_the_iteration_row_from_the_turn_call_count() {
        let mut t = full_info();
        t.context_composition = vec![jyc_types::ContextRegionInfo {
            region: jyc_types::ContextRegion::History,
            messages: 3,
            bytes: 512,
            estimated_tokens: 60,
        }];
        assert!(format_topic_info(&t).contains("\n  Iteration 3\n"));

        t.turn_call_count = None;
        assert!(!format_topic_info(&t).contains("Iteration"));
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

    /// The subagent tree the detail carries is printed as text — the same
    /// tree the info pane draws, out of the same detail.
    #[test]
    fn prints_the_subagent_tree() {
        let mut t = full_info();
        t.subagents = Some(vec![
            jyc_types::TopicNode {
                name: "ci-watch".to_string(),
                channel: "feishu".to_string(),
                status: TopicStatus::Idle,
                children: vec![jyc_types::TopicNode {
                    name: "review".to_string(),
                    channel: "feishu".to_string(),
                    status: TopicStatus::Processing,
                    children: vec![],
                }],
            },
            jyc_types::TopicNode {
                name: "ci-watch-2".to_string(),
                channel: "feishu".to_string(),
                status: TopicStatus::Idle,
                children: vec![],
            },
        ]);

        let text = format_topic_info(&t);
        assert!(text.contains("Subagents (3):"), "{text}");
        assert!(text.contains("  ci-watch (Idle)"), "{text}");
        assert!(text.contains("    review (Processing)"), "{text}");
        assert!(text.contains("  ci-watch-2 (Idle)"), "{text}");
    }

    /// A topic without subagents leaves no section behind: the neighbours
    /// still render, so the omission is local.
    #[test]
    fn no_subagents_omits_the_section() {
        let t = full_info();
        let text = format_topic_info(&t);
        assert!(!text.contains("Subagents"), "{text}");
        assert!(text.contains("Files (2):"), "{text}");
    }
}
