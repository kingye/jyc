use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use jyc_types::state_dir::jyc_dir;
use tracing::instrument;

use super::fork_handler::{fail, invalid_name, next_free_name, seed_state};
use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::topic_manager::TopicManager;
use crate::topic_path::{resolve_topic_path, resolved};

/// How to call it, repeated in every refusal so a typo self-corrects.
const USAGE: &str = "/spawn [name] [path]";

/// `/spawn` — start a sibling topic that carries this topic's conversation into
/// a directory of its own.
///
/// The difference from `/fork` is only *where the new topic works*: a fork
/// keeps this topic's directory, a spawn gets another one — a fresh dir under
/// the agents root, or any dir you name, including one that already holds files
/// (a checkout made elsewhere). What comes along is the state, not the
/// workspace: the same allow-list `/fork` seeds (context, transcript, task list,
/// backlog, settings) marked `spawned-from`, so a spawn inherits neither jobs
/// that would double-fire nor a token meter it did not run up.
///
/// Because a spawn copies nothing **into** the destination, it neither merges
/// with existing data nor needs the guards that would refuse it: a destination
/// may be non-empty, and may even sit inside this topic's own dir. It creates
/// exactly two things — the destination directory when it is missing, and
/// `<agents-root>/<name>/.jyc` with its registration — and leaves whatever the
/// destination holds exactly as it found it.
///
/// Websocket topics only, like `/fork`: on every other channel the topic name
/// *is* the routing address, so spawning one would mean inventing an address.
pub struct SpawnCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl SpawnCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }

    /// Testable core of [`Self::execute`] with every root passed in, so no test
    /// can reach the real data home (mirrors `ForkCommandHandler::fork_under`).
    /// A relative destination resolves against `context.topic_path`, which is
    /// what makes `../test` mean "beside this topic".
    async fn spawn_under(
        &self,
        context: &CommandContext,
        parent_state: &Path,
        agents_root: &Path,
    ) -> Result<CommandResult> {
        // One name and one path, told apart by shape rather than position, so
        // `/spawn scratch`, `/spawn ../test` and `/spawn scratch workdir` all
        // read naturally.
        let mut name = None;
        let mut path = None;
        for arg in context
            .args
            .iter()
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
        {
            let slot = if looks_like_path(arg) {
                &mut path
            } else {
                &mut name
            };
            if slot.is_some() {
                return Ok(fail(format!(
                    "/spawn: one name and one path at most. Usage: {USAGE}"
                )));
            }
            *slot = Some(arg.to_string());
        }

        if !parent_state.is_dir() {
            return Ok(fail(format!(
                "/spawn: this topic's state dir {} is gone — there is nothing to hand over.",
                parent_state.display()
            )));
        }

        // Name and destination are one decision: a path with no name takes the
        // path's last component, and a name with no path lands under the agents
        // root. No arguments at all means `<this>-2` there too.
        let requested = path
            .as_deref()
            .map(|p| resolve_topic_path(p, &context.topic_path));
        let name = match name {
            Some(name) => name,
            None => match requested.as_ref().and_then(|p| p.file_name()) {
                Some(file) => file.to_string_lossy().into_owned(),
                None => next_free_name(&self.topic_manager, &context.topic_name, agents_root).await,
            },
        };
        let dest = requested.unwrap_or_else(|| agents_root.join(&name));

        if let Some(reason) = invalid_name(&name) {
            return Ok(fail(format!("/spawn: {reason}. Usage: {USAGE}")));
        }
        if name == context.topic_name {
            return Ok(fail(format!(
                "/spawn: '{name}' is the topic you are already in."
            )));
        }
        // Same two checks as `/fork`: a taken name would either route to
        // another topic or adopt over another topic's state dir.
        if self.topic_manager.topic_path(&name).await.is_some() || agents_root.join(&name).exists()
        {
            return Ok(fail(format!(
                "/spawn: topic '{name}' already exists. Give it another name."
            )));
        }

        // Nothing below this point may create anything until every guard has
        // passed. Comparisons go through `resolved`, which canonicalizes what
        // exists and leaves the rest as written: a path that exists cannot reach
        // two names through `..` or a symlink, and a path that does not exist
        // yet cannot already be somebody else's topic dir.
        let source_dir = resolved(&context.topic_path);
        let dest_dir = resolved(&dest);
        if dest_dir == source_dir {
            return Ok(fail(format!(
                "/spawn: {} is the directory you are in — pick another path.",
                dest.display()
            )));
        }
        match tokio::fs::metadata(&dest).await {
            Ok(meta) if !meta.is_dir() => {
                return Ok(fail(format!(
                    "/spawn: {} exists and is not a directory.",
                    dest.display()
                )));
            }
            _ => {}
        }
        if let Some(owner) = self
            .topic_manager
            .custom_topic_paths()
            .await
            .into_iter()
            .find(|(name, pin)| {
                name.as_str() != context.topic_name.as_str() && resolved(pin) == dest_dir
            })
            .map(|(name, _)| name)
        {
            return Ok(fail(format!(
                "/spawn: {} is the topic dir of '{owner}'.",
                dest.display()
            )));
        }

        // Created now, so a fresh `../new-dir` is canonical by the time it is
        // registered — a topic's path should not carry `..` in it. An existing
        // directory is left alone.
        tokio::fs::create_dir_all(&dest)
            .await
            .with_context(|| format!("/spawn: could not create {}", dest.display()))?;
        let dest = resolved(&dest);

        // Register the spawn's own state under its name, then pin the dir —
        // `/fork`'s order, so `set_topic_path` reuses this registration instead
        // of deriving one of its own.
        let state_dir = agents_root.join(&name).join(".jyc");
        crate::topic_path::adopt_state_dir(&name, &dest, &state_dir)?;
        self.topic_manager
            .set_topic_path(&name, dest.clone())
            .await?;
        let seeded = seed_state(
            parent_state,
            &state_dir,
            &context.topic_name,
            "spawned-from",
        )
        .await?;

        tracing::info!(
            from = %context.topic_name,
            to = %name,
            dir = %dest.display(),
            state = %state_dir.display(),
            seeded,
            "Topic spawned"
        );
        Ok(CommandResult {
            success: true,
            message: format!(
                "✅ Spawned '{}' → '{}' — it works in {} with its own state in {} ({} state \
                 file(s) inherited, no workspace files copied).\n/spawn does not switch by \
                 itself — pick '{name}' in the topic list.",
                context.topic_name,
                name,
                dest.display(),
                state_dir.display(),
                seeded
            ),
            error: None,
            append_body: None,
        })
    }
}

/// Whether a bare argument is a path rather than a topic name: an explicit `~`,
/// or a separator. Everything else is a name — and every name still has to
/// survive `invalid_name`, so a path-looking name cannot slip through.
fn looks_like_path(arg: &str) -> bool {
    arg.starts_with('~') || arg.contains('/') || arg.contains('\\')
}

#[async_trait]
impl CommandHandler for SpawnCommandHandler {
    #[instrument(skip(self, context), fields(topic = %context.topic_name))]
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        if context.channel_type != "websocket" {
            return Ok(fail(
                "/spawn only works on websocket topics: elsewhere the topic name is its routing \
                 address, so a spawn would need an address of its own."
                    .to_string(),
            ));
        }
        // Production path only: `agents_workspace_root()` resolves the real data
        // home, so a test that reached here would create topics inside the
        // developer's own agents tree. Tests call `spawn_under` with every root.
        let agents_root = self.topic_manager.agents_workspace_root();
        let parent_state = jyc_dir(&context.topic_name, &context.topic_path);
        self.spawn_under(&context, &parent_state, &agents_root)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_storage::MessageStorage;
    use crate::metrics::MetricsCollector;
    use crate::session_state;
    use crate::static_agent::StaticAgentService;
    use arc_swap::ArcSwap;
    use std::path::PathBuf;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    /// A `TopicManager` rooted in `workspace` — same builder the other command
    /// handlers' tests use, so every dir a spawn creates stays in the test's
    /// `TempDir`.
    fn make_topic_manager(workspace: &Path) -> Arc<TopicManager> {
        let storage = Arc::new(MessageStorage::new(workspace));
        let (metrics, _stats, _task) = MetricsCollector::new(CancellationToken::new()).start();
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

    /// A parent topic: a dir with workspace content, and an in-dir `.jyc`
    /// holding both the inheritable state and the files a spawn must not take.
    async fn parent_topic() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("src-topic");
        let state = dir.join(".jyc");
        tokio::fs::create_dir_all(state.join("jobs")).await.unwrap();
        tokio::fs::create_dir_all(dir.join("src")).await.unwrap();
        tokio::fs::write(dir.join("src/main.rs"), "fn main() {}")
            .await
            .unwrap();
        tokio::fs::write(dir.join("README.md"), "hello")
            .await
            .unwrap();
        for (name, body) in [
            ("pattern", "jyc"),
            ("agent-context.json", "{\"messages\":[]}"),
            ("agent-session.json", "{\"current_input_tokens\":900}"),
            (session_state::TASKS_FILE, "{\"items\":[]}"),
            ("chat_history_2026-09-24.jsonl", "{}"),
            ("activity.jsonl", "{}"),
            ("notes.md", "the user's own note"),
        ] {
            tokio::fs::write(state.join(name), body).await.unwrap();
        }
        tokio::fs::write(state.join("jobs/one.json"), "{}")
            .await
            .unwrap();
        (tmp, dir)
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

    /// No argument: a fresh dir under the agents root, the conversation in it,
    /// and none of the workspace content — a spawn hands over state, not files.
    #[tokio::test]
    async fn spawn_defaults_to_a_fresh_dir_under_the_agents_root() {
        let (_tmp, dir) = parent_topic().await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let handler = SpawnCommandHandler::new(tm.clone());

        let ctx = context("src-topic", &dir, "websocket", &[]);
        let result = handler
            .spawn_under(&ctx, &dir.join(".jyc"), &agents_root)
            .await
            .unwrap();
        assert!(result.success, "{}", result.message);

        // The handler registers the canonical path, so the asserts compare
        // against it too.
        let dest = resolved(&agents_root.join("src-topic-2"));
        assert!(dest.is_dir(), "the default destination is created for it");
        assert_eq!(
            tm.topic_path("src-topic-2").await.unwrap(),
            dest,
            "the spawn works in its own dir, not the parent's"
        );
        assert!(
            !dest.join("README.md").exists() && !dest.join("src").exists(),
            "a spawn hands over the conversation, not the workspace"
        );
        assert!(dir.join("README.md").exists(), "the parent keeps its files");

        let state = jyc_dir("src-topic-2", &dest);
        assert_eq!(
            state,
            dest.join(".jyc"),
            "in the default shape the state lives inside the topic's own dir"
        );
        assert_eq!(
            tokio::fs::read_to_string(state.join("spawned-from"))
                .await
                .unwrap(),
            "src-topic"
        );
        for keep in [
            "agent-context.json",
            session_state::TASKS_FILE,
            "pattern",
            "chat_history_2026-09-24.jsonl",
        ] {
            assert!(state.join(keep).exists(), "{keep} must be inherited");
        }
        for drop in ["agent-session.json", "activity.jsonl", "notes.md"] {
            assert!(
                !state.join(drop).exists(),
                "{drop} must stay with the parent"
            );
        }
        assert!(
            !state.join("jobs").exists(),
            "a copy of the parent's scheduled jobs would double-fire them"
        );

        jyc_types::state_dir::unregister("src-topic-2");
    }

    /// The reason `/spawn` exists: continue the conversation where the files
    /// already are — a non-empty directory is a destination, not a refusal.
    #[tokio::test]
    async fn spawn_works_in_an_existing_dir_and_touches_nothing_there() {
        let (_tmp, dir) = parent_topic().await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let handler = SpawnCommandHandler::new(tm.clone());
        // A checkout *elsewhere*: the workspace root is the workspace-topic name
        // space, so a dir there really would be a topic of that name already.
        let checkout = dir.parent().unwrap().join("checkout");
        tokio::fs::create_dir_all(checkout.join("src"))
            .await
            .unwrap();
        tokio::fs::write(checkout.join("src/main.rs"), "fn main() {}")
            .await
            .unwrap();

        let ctx = context(
            "src-topic",
            &dir,
            "websocket",
            &[checkout.to_str().unwrap()],
        );
        let result = handler
            .spawn_under(&ctx, &dir.join(".jyc"), &workspace.path().join("agents"))
            .await
            .unwrap();
        assert!(result.success, "{}", result.message);

        assert_eq!(
            tm.topic_path("checkout").await.unwrap(),
            resolved(&checkout),
            "a path with no name is taken after the dir itself"
        );
        assert_eq!(
            tokio::fs::read_to_string(checkout.join("src/main.rs"))
                .await
                .unwrap(),
            "fn main() {}",
            "the destination's content is exactly as it was found"
        );
        assert!(
            !checkout.join(".jyc").exists(),
            "the conversation goes to the agents root, never into the user's checkout"
        );

        jyc_types::state_dir::unregister("checkout");
    }

    /// A relative destination is taken against this topic's dir, so `../test`
    /// lands beside the parent and `./nested` lands inside it — both allowed,
    /// because nothing is written into either.
    #[tokio::test]
    async fn spawn_resolves_a_relative_path_against_this_topic() {
        for (arg, name, inside_source) in [("../test", "test", false), ("./nested", "nested", true)]
        {
            let (_tmp, dir) = parent_topic().await;
            let workspace = tempdir().unwrap();
            let tm = make_topic_manager(workspace.path());
            let handler = SpawnCommandHandler::new(tm.clone());
            let expected = if inside_source {
                dir.join("nested")
            } else {
                dir.parent().unwrap().join("test")
            };

            let ctx = context("src-topic", &dir, "websocket", &[arg]);
            let result = handler
                .spawn_under(&ctx, &dir.join(".jyc"), &workspace.path().join("agents"))
                .await
                .unwrap();
            assert!(result.success, "'{arg}': {}", result.message);
            assert_eq!(
                tm.topic_path(name).await.unwrap(),
                resolved(&expected),
                "'{arg}' must pin the resolved dir, with no '..' left in it"
            );
            assert!(
                !expected.join(".jyc").exists(),
                "'{arg}' must not put state in the destination"
            );

            jyc_types::state_dir::unregister(name);
        }
    }

    /// Look-once, refuse-before-creating: none of these may leave a directory,
    /// a topic, or a touched file behind.
    #[tokio::test]
    async fn spawn_refuses_destinations_and_names_it_must_not_take() {
        let (_tmp, dir) = parent_topic().await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let handler = SpawnCommandHandler::new(tm.clone());

        let other = workspace.path().join("other-dir");
        tokio::fs::create_dir_all(&other).await.unwrap();
        tokio::fs::write(other.join("keep.txt"), "mine")
            .await
            .unwrap();
        tm.set_topic_path("other", other.clone()).await.unwrap();
        let a_file = workspace.path().join("file.txt");
        tokio::fs::write(&a_file, "x").await.unwrap();
        tokio::fs::create_dir_all(agents_root.join("taken"))
            .await
            .unwrap();

        // Reaching the source dir takes an explicit path: `Path::file_name`
        // drops `.`, so `"./."` alone would name itself `src-topic` instead.
        for (args, expected) in [
            (vec!["src-topic"], "already in"),
            (vec!["taken"], "already exists"),
            (vec![".hidden"], "must not start with '.'"),
            (
                vec!["spawn-x", dir.to_str().unwrap()],
                "the directory you are in",
            ),
            (vec!["spawn-x", a_file.to_str().unwrap()], "not a directory"),
            (
                vec!["spawn-x", other.to_str().unwrap()],
                "topic dir of 'other'",
            ),
            (vec!["a", "b", "c"], "one name and one path at most"),
        ] {
            let ctx = context("src-topic", &dir, "websocket", &args);
            let result = handler
                .spawn_under(&ctx, &dir.join(".jyc"), &agents_root)
                .await
                .unwrap();
            assert!(!result.success, "'{args:?}' must be refused: {result:?}");
            assert!(
                result.message.contains(expected),
                "'{args:?}' should say '{expected}': {}",
                result.message
            );
        }

        assert!(tm.topic_path("spawn-x").await.is_none());
        assert!(!agents_root.join("spawn-x").exists());
        assert!(!dir.join("nested").exists(), "a refusal creates nothing");
        assert_eq!(
            tokio::fs::read_to_string(other.join("keep.txt"))
                .await
                .unwrap(),
            "mine",
            "a refused /spawn must not touch the destination"
        );
        jyc_types::state_dir::unregister("other");
    }

    /// `/spawn` is gated to websocket topics like `/fork`.
    #[tokio::test]
    async fn spawn_refuses_a_non_websocket_channel() {
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let handler = SpawnCommandHandler::new(tm.clone());
        let tmp = tempdir().unwrap();

        let ctx = context("src-topic", tmp.path(), "email", &["whatever"]);
        let result = handler.execute(ctx).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("routing address"), "{result:?}");
        assert!(tm.topic_path("whatever").await.is_none());
    }

    #[test]
    fn path_shaped_arguments_are_paths_and_the_rest_are_names() {
        assert!(looks_like_path("~/tmp/x"));
        assert!(looks_like_path("/tmp/x"));
        assert!(looks_like_path("sub/dir"));
        assert!(looks_like_path("../test"));
        assert!(!looks_like_path("scratch"));
        assert!(!looks_like_path("plan-197"));
    }
}
