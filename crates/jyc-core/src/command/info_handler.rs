use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use jyc_types::{ChangeKind, TopicInfo, format_amount};

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::topic_manager::TopicManager;

/// /info command — show this topic's info, mirroring the dashboard chat
/// screen's Topic Info pane (name, channel, pattern, model, mode, branch,
/// token usage, cost, changed files).
///
/// Data comes from `TopicManager::list_topics` — the same snapshot the
/// dashboard renders — so channel users see exactly what the TUI shows.
/// Works on every channel: the result is delivered as a normal command
/// reply.
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
    fn name(&self) -> &str {
        "/info"
    }

    fn description(&self) -> &str {
        "Show topic info (mode, model, tokens, cost, files)"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        // Match on (channel, topic_path) rather than the directory basename:
        // a pattern with a `topic_path` override can put the topic in a
        // directory whose name differs from the topic name.
        let topic = self
            .topic_manager
            .list_topics()
            .await
            .into_iter()
            .find(|t| {
                t.channel == context.channel
                    && t.topic_path.as_deref() == Some(context.topic_path.as_path())
            });

        match topic {
            Some(t) => Ok(CommandResult {
                success: true,
                message: format_topic_info(&t),
                ..Default::default()
            }),
            None => Ok(CommandResult {
                success: false,
                message: "/info: topic not found".into(),
                error: Some(format!(
                    "No topic with path {} registered on channel {}",
                    context.topic_path.display(),
                    context.channel
                )),
                ..Default::default()
            }),
        }
    }
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
                // gets a trailing `*` marker instead.
                let marker = if f.uncommitted { " *" } else { "" };
                lines.push(format!("{prefix}{}{marker}", f.path));
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jyc_types::{ChangedFileEntry, TopicCost, TopicStatus};
    use std::path::PathBuf;

    fn full_info() -> TopicInfo {
        TopicInfo {
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
                },
                ChangedFileEntry {
                    path: "src/edited.rs".into(),
                    uncommitted: true,
                    change: ChangeKind::Modified,
                },
            ]),
            cost: Some(TopicCost {
                session: 0.42,
                today: 1.23,
                currency: "USD".into(),
            }),
        }
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
Cost: $0.4200 session · $1.2300 today
Files (2):
+ src/new.rs
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
        t.changed_files = None;
        t.cost = None;
        t.pattern = None;
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
    fn empty_files_list_shows_none() {
        let mut t = full_info();
        t.changed_files = Some(vec![]);
        assert!(format_topic_info(&t).contains("Files: (none)"));
    }
}
