use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use jyc_types::state_dir::jyc_dir;
use tracing::instrument;

use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::session_state;
use crate::topic_manager::TopicManager;

/// State files a forked topic inherits from its parent.
///
/// An allow-list, not a directory walk: whatever is absent here stays in the
/// parent. What is deliberately missing, and why, is worth knowing before
/// adding to it:
///
/// - `agent-session.json` — the token/cost meter. The usage it counts was
///   billed to the parent, and a fork must not inherit a bill it did not run
///   up (`record_token_usage` adds to whatever is on disk). Leaving it out
///   starts the fork un-metered; the first response re-seeds `max_input_tokens`.
/// - `activity.jsonl`, `wire-payload.jsonl` — per-topic logs.
/// - `topic-name`, `topic-path`, `topic-meta.json`, `thread-meta.json` — topic
///   identity and channel routing. `set_topic_path` writes the first two for
///   the new topic; the routing meta must not point the fork at the parent's
///   channel or thread.
/// - `jobs/` — a scheduled job fires by injecting into *its* topic, so a copy
///   would make the fork run the parent's jobs too. Directories are skipped.
/// - Anything else in the state dir belongs to the user, not to the framework.
const SEED_FILES: &[&str] = &[
    "agent-context.json",      // the running context: the conversation
    session_state::TASKS_FILE, // the task list, so the fork continues it (#810)
    "backlog.jsonl",           // the topic's own backlog
    "pattern",                 // which agent pattern runs the topic
    "mode-override",           // plan/build mode
    "plan-model-override",     // per-mode model choice
    "build-model-override",
    session_state::CONTEXT_STRATEGY_FILE, // `/context`
    session_state::MCP_OVERRIDE_FILE,     // `/mcp`
    session_state::SKILL_OVERRIDE_FILE,   // `/skill`
    "skills.json",
];

/// Chat history is one file per day, so it is matched by prefix instead of
/// listed. A fork inherits it: the transcript is what you fork *for*.
const CHAT_HISTORY_PREFIX: &str = "chat_history_";

/// `/fork` — branch a sibling topic off this one.
///
/// The new topic is an ordinary topic: its own directory at
/// `<agents-root>/<agent>/<name>` and its own `.jyc` state dir, exactly where
/// the router puts runtime topics — which is also the boundary
/// [`TopicManager::auto_close_topic`] checks before it dares to delete
/// anything. What makes it a *fork* is where it starts: this topic's context,
/// transcript, task list and settings, so work can continue down a second path
/// while both keep running and never write into each other's history.
///
/// Websocket topics only, like `/pin`: on every other channel the topic name
/// *is* the routing address, so forking one would mean inventing an address.
pub struct ForkCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl ForkCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }

    /// Testable core of [`Self::execute`] with both roots passed in, so no test
    /// can reach the real data home (mirrors
    /// `TopicManager::auto_close_topic_under`).
    async fn fork_under(
        &self,
        context: &CommandContext,
        parent_state: &Path,
        agents_root: &Path,
    ) -> Result<CommandResult> {
        // Which agent the parent runs as decides where the sibling lands. The
        // pattern name can nest (`jyc/plan-197`); the agent is its first
        // segment, so the new topic stays a sibling instead of nesting deeper.
        let Some(agent) = session_state::read_pattern(&context.topic_name, &context.topic_path)
            .await
            .map(|p| p.split('/').next().unwrap_or(p.as_str()).to_string())
        else {
            return Ok(fail(
                "/fork: this topic has no recorded pattern, so there is no agent directory to \
                 fork into."
                    .to_string(),
            ));
        };
        let agent_dir = agents_root.join(agent);

        let requested = context
            .args
            .iter()
            .map(|a| a.trim())
            .find(|a| !a.is_empty());
        let name = match requested {
            Some(name) => name.to_string(),
            None => self.next_free_name(&context.topic_name, &agent_dir).await,
        };
        if let Some(reason) = invalid_name(&name) {
            return Ok(fail(format!("/fork: {reason}. Usage: /fork [name]")));
        }
        if name == context.topic_name {
            return Ok(fail(format!(
                "/fork: '{name}' is the topic you are already in."
            )));
        }

        let new_dir = agent_dir.join(&name);
        if self.topic_manager.topic_path(&name).await.is_some() || new_dir.exists() {
            return Ok(fail(format!(
                "/fork: topic '{name}' already exists. Give it another name."
            )));
        }

        // Creates the topic dir, its `.jyc` state dir and the `topic-name` /
        // `topic-path` breadcrumbs, and registers the routing entry — the same
        // primitive `POST /topics` uses, so the new topic is listed and
        // addressable right away.
        self.topic_manager
            .set_topic_path(&name, new_dir.clone())
            .await?;

        // Resolved *after* that call: `set_topic_path` adopts the state dir,
        // which is where `jyc_dir` points from now on.
        let child_state = jyc_dir(&name, &new_dir);
        let seeded = seed_state(parent_state, &child_state, &context.topic_name).await?;

        tracing::info!(
            from = %context.topic_name,
            to = %name,
            dir = %new_dir.display(),
            seeded,
            "Topic forked"
        );
        let message = format!(
            "✅ Forked '{}' → '{}' at {} ({} state file(s) inherited).\n\
             /fork does not switch by itself — pick '{name}' in the topic list.",
            context.topic_name,
            name,
            new_dir.display(),
            seeded
        );
        Ok(CommandResult {
            success: true,
            message,
            error: None,
            append_body: None,
        })
    }

    /// `<topic>-2`, `<topic>-3`, … — the first candidate that is neither a
    /// registered topic nor a directory that already exists. If all of them
    /// are taken, return `<topic>-2` so the caller reports the duplicate
    /// rather than inventing a name nobody asked for.
    ///
    /// ponytail: the probe and the `create_dir_all` inside `set_topic_path` are
    /// not one transaction, so two topics forking to the same automatic name at
    /// the same instant would share a directory. Unreachable at typing speed; a
    /// lock would cost more than the case it covers.
    async fn next_free_name(&self, topic: &str, agent_dir: &Path) -> String {
        for n in 2..100u32 {
            let candidate = format!("{topic}-{n}");
            if self.topic_manager.topic_path(&candidate).await.is_none()
                && !agent_dir.join(&candidate).exists()
            {
                return candidate;
            }
        }
        format!("{topic}-2")
    }
}

/// Why `name` cannot be a topic name, if it cannot.
///
/// Keeps the shape the router itself produces (`plan-197`, `上海天气`,
/// `英国旅行2026`): unicode is fine, path syntax is not — the name becomes a
/// directory name and a routing key, and `/` or `..` would escape the agent
/// subtree. Stricter than `post_topic`'s check in `jyc-inspect` (path syntax
/// only) on purpose, because here the name also picks a parent directory; if a
/// second creator ever needs the same rule, lift it to `jyc-types` rather than
/// copying it.
fn invalid_name(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("the name is empty");
    }
    if name.len() > 64 {
        return Some("the name is too long (64 bytes max)");
    }
    if name.starts_with('.') {
        return Some("the name must not start with '.'");
    }
    if name.contains('/') || name.contains('\\') {
        return Some("the name must not contain a path separator");
    }
    if name.contains("..") {
        return Some("the name must not contain '..'");
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ' | ':'))
    {
        return Some("the name may only use letters, digits, '-', '_', '.', ':' or spaces");
    }
    None
}

/// Copy the inheritable state of `parent` into `child` and record where the
/// fork came from. Parent files that do not exist are simply not copied — a
/// young topic has few of them.
async fn seed_state(parent: &Path, child: &Path, from: &str) -> Result<usize> {
    let mut copied = 0;
    let mut entries = tokio::fs::read_dir(parent).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        let inherit = SEED_FILES.contains(&file_name)
            || (file_name.starts_with(CHAT_HISTORY_PREFIX) && file_name.ends_with(".jsonl"));
        if inherit && tokio::fs::copy(&path, child.join(file_name)).await.is_ok() {
            copied += 1;
        }
    }
    tokio::fs::write(child.join("forked-from"), from).await?;
    Ok(copied)
}

fn fail(message: String) -> CommandResult {
    CommandResult {
        success: false,
        message,
        error: None,
        append_body: None,
    }
}

#[async_trait]
impl CommandHandler for ForkCommandHandler {
    #[instrument(skip(self, context), fields(topic = %context.topic_name))]
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        if context.channel_type != "websocket" {
            return Ok(fail(
                "/fork only works on websocket topics: elsewhere the topic name is its routing \
                 address, so a fork would need an address of its own."
                    .to_string(),
            ));
        }
        // Production path only: `agents_workspace_root()` resolves the real data
        // home, so a test that reached here would create topics inside the
        // developer's own agents tree. Tests call `fork_under` with both roots.
        let agents_root = self.topic_manager.agents_workspace_root();
        let parent_state = jyc_dir(&context.topic_name, &context.topic_path);
        self.fork_under(&context, &parent_state, &agents_root).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_storage::MessageStorage;
    use crate::metrics::MetricsCollector;
    use crate::static_agent::StaticAgentService;
    use arc_swap::ArcSwap;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    /// A `TopicManager` rooted in `workspace`, so the dirs a fork creates and
    /// the state dir it adopts both stay inside the test's `TempDir`. Same
    /// builder the other command handlers' tests use.
    fn make_topic_manager(workspace: &Path) -> Arc<TopicManager> {
        let storage = Arc::new(MessageStorage::new(workspace));
        let (metrics, _stats, _task) = MetricsCollector::new(CancellationToken::new()).start();
        // `general`, `channels` and `agents` are all `#[serde(default)]` and no
        // fork path touches a channel, so the manager only has to load.
        let config = Arc::new(ArcSwap::from_pointee(
            jyc_types::load_config_from_str("[general]\n[agent]\nenabled = true\n").unwrap(),
        ));
        Arc::new(TopicManager::new_with_options(
            1,
            10,
            storage,
            Arc::new(NoopOutbound),
            Arc::new(StaticAgentService::new("ok")),
            CancellationToken::new(),
            false,
            workspace.join("templates"),
            config,
            "test".to_string(),
            "websocket".to_string(),
            workspace.parent().unwrap_or(workspace).to_path_buf(),
            workspace.to_path_buf(),
            metrics,
            None,
        ))
    }

    struct NoopOutbound;

    #[async_trait::async_trait]
    impl jyc_types::OutboundAdapter for NoopOutbound {
        fn channel_type(&self) -> &str {
            "test"
        }
        async fn connect(&self) -> Result<()> {
            Ok(())
        }
        async fn disconnect(&self) -> Result<()> {
            Ok(())
        }
        fn clean_body(&self, raw_body: &str) -> String {
            raw_body.to_string()
        }
        async fn send_reply(
            &self,
            _original: &jyc_types::InboundMessage,
            _reply_text: &str,
            _topic_path: &Path,
            _message_dir: &str,
            _attachments: Option<&[jyc_types::OutboundAttachment]>,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "noop".to_string(),
            })
        }
        async fn send_message(
            &self,
            _channel: &str,
            _topic: &str,
            _text: &str,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "noop".to_string(),
            })
        }
    }

    /// A parent topic dir whose `.jyc` holds `files`, plus a `jobs/` dir and a
    /// pattern naming `jyc` as the agent.
    async fn parent_topic(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempdir().unwrap();
        let topic_dir = tmp.path().join("src-topic");
        let state = topic_dir.join(".jyc");
        tokio::fs::create_dir_all(state.join("jobs")).await.unwrap();
        let mut all = vec![
            ("pattern", "jyc"),
            ("agent-context.json", "{\"messages\":[]}"),
            ("agent-session.json", "{\"current_input_tokens\":900}"),
            (session_state::TASKS_FILE, "{\"items\":[]}"),
            ("chat_history_2026-09-23.jsonl", "{}"),
            ("chat_history_2026-09-24.jsonl", "{}"),
            ("activity.jsonl", "{}"),
            ("wire-payload.jsonl", "{}"),
            ("topic-name", "src-topic"),
            ("topic-path", "/somewhere"),
            ("topic-meta.json", "{}"),
            ("notes.md", "the user's own file"),
        ];
        all.extend_from_slice(files);
        for (name, body) in all {
            tokio::fs::write(state.join(name), body).await.unwrap();
        }
        tokio::fs::write(state.join("jobs/one.json"), "{}")
            .await
            .unwrap();
        (tmp, topic_dir)
    }

    fn context(topic: &str, topic_dir: &Path, channel_type: &str, args: &[&str]) -> CommandContext {
        CommandContext {
            topic_name: topic.to_string(),
            topic_path: topic_dir.to_path_buf(),
            channel_type: channel_type.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// The happy path: a sibling topic, its own state dir, the inheritable
    /// files and nothing that must stay behind.
    #[tokio::test]
    async fn fork_creates_a_sibling_and_seeds_only_the_inheritable_state() {
        let (_ptmp, topic_dir) = parent_topic(&[]).await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let agent_dir = agents_root.join("jyc");

        let handler = ForkCommandHandler::new(tm.clone());
        let ctx = context("src-topic", &topic_dir, "websocket", &["sibling"]);
        let result = handler
            .fork_under(&ctx, &topic_dir.join(".jyc"), &agents_root)
            .await
            .unwrap();

        assert!(result.success, "{}", result.message);
        let new_dir = tm.topic_path("sibling").await.unwrap();
        assert_eq!(new_dir, agent_dir.join("sibling"), "a sibling, not a nest");

        let child = jyc_dir("sibling", &new_dir);
        assert_eq!(
            tokio::fs::read_to_string(child.join("forked-from"))
                .await
                .unwrap(),
            "src-topic"
        );
        for keep in ["agent-context.json", session_state::TASKS_FILE, "pattern"] {
            assert!(child.join(keep).exists(), "{keep} must be inherited");
        }
        let mut days = 0;
        let mut entries = tokio::fs::read_dir(&child).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(CHAT_HISTORY_PREFIX)
            {
                days += 1;
            }
        }
        assert_eq!(days, 2, "both days of the transcript come along");
        for drop in [
            "agent-session.json",
            "activity.jsonl",
            "wire-payload.jsonl",
            "topic-meta.json",
            "notes.md",
        ] {
            assert!(
                !child.join(drop).exists(),
                "{drop} must stay with the parent"
            );
        }
        assert!(
            !child.join("jobs").exists(),
            "a copy of the parent's scheduled jobs would double-fire them"
        );
        // The parent keeps everything — a fork must not consume its source.
        assert!(topic_dir.join(".jyc/agent-context.json").exists());
    }

    /// No argument means an automatic name, and it must step past siblings
    /// that already exist.
    #[tokio::test]
    async fn fork_without_a_name_picks_the_first_free_suffix() {
        let (_ptmp, topic_dir) = parent_topic(&[]).await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        tokio::fs::create_dir_all(agents_root.join("jyc/src-topic-2"))
            .await
            .unwrap();

        let handler = ForkCommandHandler::new(tm.clone());
        let ctx = context("src-topic", &topic_dir, "websocket", &[]);
        let result = handler
            .fork_under(&ctx, &topic_dir.join(".jyc"), &agents_root)
            .await
            .unwrap();

        assert!(result.success, "{}", result.message);
        assert!(result.message.contains("src-topic-3"), "{}", result.message);
        assert!(tm.topic_path("src-topic-3").await.is_some());
    }

    /// Names become directory names and routing keys, so path syntax is
    /// rejected before anything touches the disk.
    #[tokio::test]
    async fn fork_rejects_names_that_cannot_be_topic_names() {
        let (_ptmp, topic_dir) = parent_topic(&[]).await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let handler = ForkCommandHandler::new(tm);

        for bad in ["../escape", "a/b", ".hidden", "x;y", "src-topic"] {
            let ctx = context("src-topic", &topic_dir, "websocket", &[bad]);
            let result = handler
                .fork_under(&ctx, &topic_dir.join(".jyc"), &agents_root)
                .await
                .unwrap();
            assert!(!result.success, "'{bad}' must be rejected: {result:?}");
        }
        assert!(
            !agents_root.join("jyc/escape").exists(),
            "nothing may be created from a rejected name"
        );
    }

    /// A name that is already a topic cannot be forked over.
    #[tokio::test]
    async fn fork_refuses_an_existing_topic() {
        let (_ptmp, topic_dir) = parent_topic(&[]).await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let taken = agents_root.join("jyc/taken");
        tokio::fs::create_dir_all(&taken).await.unwrap();
        tm.set_topic_path("taken", taken).await.unwrap();

        let handler = ForkCommandHandler::new(tm);
        let ctx = context("src-topic", &topic_dir, "websocket", &["taken"]);
        let result = handler
            .fork_under(&ctx, &topic_dir.join(".jyc"), &agents_root)
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.message.contains("already exists"));
    }

    /// A topic with no pattern has no agent directory to fork into, and
    /// `/fork` is gated to websocket topics like `/pin`.
    #[tokio::test]
    async fn fork_refuses_without_a_pattern_or_channel() {
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let handler = ForkCommandHandler::new(tm.clone());

        let bare = tempdir().unwrap();
        let ctx = context("no-pattern", bare.path(), "websocket", &["x"]);
        let result = handler
            .fork_under(&ctx, &bare.path().join(".jyc"), &agents_root)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.message.contains("pattern"), "{result:?}");

        let (_ptmp, topic_dir) = parent_topic(&[]).await;
        let ctx = context("src-topic", &topic_dir, "email", &["whatever"]);
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("routing address"), "{result:?}");
        assert!(
            tm.topic_path("whatever").await.is_none(),
            "a refused /fork must not leave a topic behind"
        );
    }
}
