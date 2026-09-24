use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use jyc_types::state_dir::jyc_dir;
use tracing::instrument;

use super::fork_handler::{fail, invalid_name, next_free_name, seed_state};
use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::topic_manager::TopicManager;
use crate::topic_path::{copy_dir_all, resolve_topic_path, resolved};

/// How to call it, repeated in every refusal so a typo self-corrects.
const USAGE: &str = "/clone [name] [path]";

/// `/clone` — copy this topic's directory into a directory of its own.
///
/// The difference from `/fork` is what the new topic *sees*: a fork shares the
/// parent's directory (only its state is separate), a clone gets a physical
/// copy and from then on the two edit different files. That is what makes it a
/// clone — and what makes it worth refusing to copy into anything that already
/// holds data.
///
/// What it copies is the workspace, not the state: `.jyc` is seeded from the
/// source topic's state dir with `/fork`'s allow-list (context, transcript,
/// task list, backlog, settings) and marked `cloned-from`, so a clone does not
/// inherit jobs that would double-fire, or a token meter it did not run up.
///
/// Websocket topics only, like `/fork`: on every other channel the topic name
/// *is* the routing address, so cloning one would mean inventing an address.
pub struct CloneCommandHandler {
    topic_manager: Arc<TopicManager>,
}

impl CloneCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }

    /// Testable core of [`Self::execute`] with every root passed in, so no test
    /// can reach the real data home (mirrors `ForkCommandHandler::fork_under`).
    async fn clone_under(
        &self,
        context: &CommandContext,
        source_state: &Path,
        agents_root: &Path,
        workdir: &Path,
    ) -> Result<CommandResult> {
        // One name and one path, told apart by shape rather than position, so
        // `/clone scratch`, `/clone ~/tmp/pcopy` and `/clone scratch ~/tmp/x`
        // all read naturally.
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
                    "/clone: one name and one path at most. Usage: {USAGE}"
                )));
            }
            *slot = Some(arg.to_string());
        }
        // A relative path would resolve against the data root and could climb
        // out of it with `..`; a path here must say where it goes.
        if let Some(p) = path.as_deref()
            && !p.starts_with('~')
            && !Path::new(p).is_absolute()
        {
            return Ok(fail(format!(
                "/clone: '{p}' is not an absolute path — write it out in full or start it with \
                 '~'. Usage: {USAGE}"
            )));
        }

        let source = context.topic_path.clone();
        if !source.is_dir() {
            return Ok(fail(format!(
                "/clone: this topic's directory {} is gone — there is nothing to copy from.",
                source.display()
            )));
        }
        let Some(source_parent) = source.parent() else {
            return Ok(fail(format!(
                "/clone: {:?} has no parent directory to put a copy beside.",
                source
            )));
        };
        // Name and destination are one decision: a path with no name takes the
        // path's last component, a name with no path lands beside the source,
        // and neither means `<this>-2` beside the source.
        let requested = path.as_deref().map(|p| resolve_topic_path(p, workdir));
        let name = match name {
            Some(name) => name,
            None => match requested.as_ref().and_then(|p| p.file_name()) {
                Some(file) => file.to_string_lossy().into_owned(),
                None => {
                    next_free_name(&self.topic_manager, &context.topic_name, source_parent).await
                }
            },
        };
        let dest = requested.unwrap_or_else(|| source_parent.join(&name));

        if let Some(reason) = invalid_name(&name) {
            return Ok(fail(format!("/clone: {reason}. Usage: {USAGE}")));
        }
        if name == context.topic_name {
            return Ok(fail(format!(
                "/clone: '{name}' is the topic you are already in."
            )));
        }
        // Same two checks as `/fork`: a taken name would either route to
        // another topic or adopt over another topic's state dir.
        if self.topic_manager.topic_path(&name).await.is_some() || agents_root.join(&name).exists()
        {
            return Ok(fail(format!(
                "/clone: topic '{name}' already exists. Give it another name."
            )));
        }

        // Nothing below this point may create or overwrite a file until every
        // guard has passed. Comparisons go through `resolved` so a symlinked or
        // `..`-laden argument cannot point one path at two names.
        let source_dir = resolved(&source);
        let dest_dir = resolved(&dest);
        if dest_dir == source_dir {
            return Ok(fail(format!(
                "/clone: {} is the directory you are in — pick another path.",
                dest.display()
            )));
        }
        if dest_dir.starts_with(&source_dir) {
            return Ok(fail(format!(
                "/clone: {} is inside {} — the copy would try to copy itself.",
                dest.display(),
                source.display()
            )));
        }
        match tokio::fs::symlink_metadata(&dest).await {
            Ok(meta) if !meta.is_dir() => {
                return Ok(fail(format!(
                    "/clone: {} exists and is not a directory.",
                    dest.display()
                )));
            }
            Ok(_) => {
                let mut entries = tokio::fs::read_dir(&dest).await?;
                if entries.next_entry().await?.is_some() {
                    return Ok(fail(format!(
                        "/clone: {} is not empty — /clone never merges into existing data.",
                        dest.display()
                    )));
                }
            }
            Err(_) => {}
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
                "/clone: {} is the topic dir of '{owner}'.",
                dest.display()
            )));
        }

        // A recursive copy of a real workspace is minutes of blocking I/O; on
        // the runtime thread it would stall every topic in the process. No
        // progress reporting, so the reply arrives when the copy is done.
        let from = source.clone();
        let to = dest.clone();
        tokio::task::spawn_blocking(move || copy_dir_all(&from, &to))
            .await
            .context("/clone: the copy did not finish")?
            .with_context(|| {
                format!(
                    "/clone: copying into {} failed (partial files may remain there)",
                    dest.display()
                )
            })?;

        // Register the clone's own state under its name, then pin the dir —
        // `/fork`'s order, so `set_topic_path` reuses this registration instead
        // of deriving one of its own.
        let state_dir = agents_root.join(&name).join(".jyc");
        crate::topic_path::adopt_state_dir(&name, &dest, &state_dir)?;
        self.topic_manager
            .set_topic_path(&name, dest.clone())
            .await?;
        let seeded =
            seed_state(source_state, &state_dir, &context.topic_name, "cloned-from").await?;

        tracing::info!(
            from = %context.topic_name,
            to = %name,
            dir = %dest.display(),
            state = %state_dir.display(),
            seeded,
            "Topic cloned"
        );
        let message = format!(
            "✅ Cloned '{}' → '{}' — it works in its own copy at {} with state in {} ({} state \
             file(s) inherited).\n/clone does not switch by itself — pick '{name}' in the topic \
             list.",
            context.topic_name,
            name,
            dest.display(),
            state_dir.display(),
            seeded
        );
        Ok(CommandResult {
            success: true,
            message,
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
impl CommandHandler for CloneCommandHandler {
    #[instrument(skip(self, context), fields(topic = %context.topic_name))]
    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        if context.channel_type != "websocket" {
            return Ok(fail(
                "/clone only works on websocket topics: elsewhere the topic name is its routing \
                 address, so a clone would need an address of its own."
                    .to_string(),
            ));
        }
        // Production path only: `agents_workspace_root()` resolves the real data
        // home, so a test that reached here would create topics inside the
        // developer's own agents tree. Tests call `clone_under` with every root.
        let agents_root = self.topic_manager.agents_workspace_root();
        let source_state = jyc_dir(&context.topic_name, &context.topic_path);
        let workdir = self.topic_manager.data_root().to_path_buf();
        self.clone_under(&context, &source_state, &agents_root, &workdir)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_storage::MessageStorage;
    use crate::metrics::MetricsCollector;
    use crate::static_agent::StaticAgentService;
    use arc_swap::ArcSwap;
    use std::path::PathBuf;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    /// A `TopicManager` rooted in `workspace` — same builder the other command
    /// handlers' tests use, so every dir a clone creates stays in the test's
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

    /// A source topic dir with content, an in-dir state dir (the pinned-topic
    /// shape), plus a `jobs/` dir and files a clone must not inherit.
    async fn source_topic() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("projects").join("src-topic");
        tokio::fs::create_dir_all(dir.join("src")).await.unwrap();
        tokio::fs::create_dir_all(dir.join(".jyc").join("jobs"))
            .await
            .unwrap();
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
            (crate::session_state::TASKS_FILE, "{\"items\":[]}"),
            ("chat_history_2026-09-24.jsonl", "{}"),
            ("activity.jsonl", "{}"),
            ("notes.md", "the user's own note"),
        ] {
            tokio::fs::write(dir.join(".jyc").join(name), body)
                .await
                .unwrap();
        }
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

    /// No argument: the copy lands beside the source, under an automatic name,
    /// with the workspace content and the inheritable state.
    #[tokio::test]
    async fn clone_defaults_to_a_sibling_dir_with_inherited_state() {
        let (_tmp, dir) = source_topic().await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let handler = CloneCommandHandler::new(tm.clone());

        let ctx = context("src-topic", &dir, "websocket", &[]);
        let result = handler
            .clone_under(&ctx, &dir.join(".jyc"), &agents_root, workspace.path())
            .await
            .unwrap();
        assert!(result.success, "{}", result.message);

        let dest = dir.parent().unwrap().join("src-topic-2");
        assert!(dest.is_dir(), "the copy is a sibling of the source");
        assert_eq!(
            tokio::fs::read_to_string(dest.join("src/main.rs"))
                .await
                .unwrap(),
            "fn main() {}",
            "the workspace content comes along"
        );
        assert_eq!(
            tm.topic_path("src-topic-2").await.unwrap(),
            dest,
            "the clone works in its own copy, not the source"
        );
        assert!(
            !dest.join(".jyc").is_dir(),
            "the source's state dir is not copied as content"
        );

        let state = agents_root.join("src-topic-2").join(".jyc");
        assert_eq!(
            jyc_dir("src-topic-2", &dest),
            state,
            "the clone's state is private, one level under the agents root"
        );
        assert_eq!(
            tokio::fs::read_to_string(state.join("cloned-from"))
                .await
                .unwrap(),
            "src-topic"
        );
        for keep in [
            "agent-context.json",
            crate::session_state::TASKS_FILE,
            "pattern",
        ] {
            assert!(state.join(keep).exists(), "{keep} must be inherited");
        }
        for drop in ["agent-session.json", "activity.jsonl", "notes.md"] {
            assert!(
                !state.join(drop).exists(),
                "{drop} must stay with the source"
            );
        }
        assert!(
            !state.join("jobs").exists(),
            "a copy of the source's scheduled jobs would double-fire them"
        );
        // The source keeps everything a clone read.
        assert!(dir.join("src/main.rs").exists());
        assert!(dir.join(".jyc/agent-context.json").exists());

        jyc_types::state_dir::unregister("src-topic-2");
    }

    /// The reason `/clone` exists: after the copy the two sides are unrelated.
    #[tokio::test]
    async fn the_copy_is_independent_of_its_source() {
        let (_tmp, dir) = source_topic().await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let handler = CloneCommandHandler::new(tm.clone());

        let ctx = context("src-topic", &dir, "websocket", &["clone-indep"]);
        handler
            .clone_under(
                &ctx,
                &dir.join(".jyc"),
                &workspace.path().join("agents"),
                workspace.path(),
            )
            .await
            .unwrap();
        let dest = dir.parent().unwrap().join("clone-indep");

        tokio::fs::write(dest.join("README.md"), "edited in the clone")
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.join("README.md"))
                .await
                .unwrap(),
            "hello",
            "editing the copy must not touch the source"
        );
        // The registry is process-global and tests run in parallel.
        jyc_types::state_dir::unregister("clone-indep");
    }

    /// An explicit path wins, and `~` is expanded.
    #[tokio::test]
    async fn clone_takes_a_path_and_names_itself_after_it() {
        let (_tmp, dir) = source_topic().await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let handler = CloneCommandHandler::new(tm.clone());
        let out = tempdir().unwrap();
        let pcopy = out.path().join("pcopy");

        let ctx = context("src-topic", &dir, "websocket", &[pcopy.to_str().unwrap()]);
        let result = handler
            .clone_under(
                &ctx,
                &dir.join(".jyc"),
                &workspace.path().join("agents"),
                workspace.path(),
            )
            .await
            .unwrap();
        assert!(result.success, "{}", result.message);
        assert!(pcopy.join("README.md").exists());
        assert_eq!(tm.topic_path("pcopy").await.unwrap(), pcopy);
        jyc_types::state_dir::unregister("pcopy");
    }

    /// Look-once, refuse-before-writing: every one of these leaves the target
    /// untouched.
    #[tokio::test]
    async fn clone_refuses_targets_it_must_not_write_to() {
        let (_tmp, dir) = source_topic().await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let handler = CloneCommandHandler::new(tm.clone());

        let occupied = dir.parent().unwrap().join("occupied");
        tokio::fs::create_dir_all(&occupied).await.unwrap();
        tokio::fs::write(occupied.join("keep.txt"), "mine")
            .await
            .unwrap();

        let taken = agents_root.join("clone-taken");
        tokio::fs::create_dir_all(&taken).await.unwrap();
        tm.set_topic_path("clone-taken", taken.clone())
            .await
            .unwrap();

        for (args, expected) in [
            (vec!["../escape"], "absolute path"),
            (vec!["src-topic"], "already in"),
            (vec!["clone-taken"], "already exists"),
            (
                vec!["clone-target", occupied.to_str().unwrap()],
                "not empty",
            ),
            (
                vec!["clone-target", dir.to_str().unwrap()],
                "directory you are in",
            ),
        ] {
            let ctx = context("src-topic", &dir, "websocket", &args);
            let result = handler
                .clone_under(&ctx, &dir.join(".jyc"), &agents_root, workspace.path())
                .await
                .unwrap();
            assert!(!result.success, "'{args:?}' must be refused: {result:?}");
            assert!(
                result.message.contains(expected),
                "'{args:?}' should say '{expected}': {}",
                result.message
            );
        }
        // A destination inside the source would recurse into itself.
        let inner = dir.join("nested");
        let ctx = context(
            "src-topic",
            &dir,
            "websocket",
            &["clone-target", inner.to_str().unwrap()],
        );
        let result = handler
            .clone_under(&ctx, &dir.join(".jyc"), &agents_root, workspace.path())
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.message.contains("inside"), "{}", result.message);
        assert!(!inner.exists(), "a refused /clone must not create anything");

        assert_eq!(
            tokio::fs::read_to_string(occupied.join("keep.txt"))
                .await
                .unwrap(),
            "mine"
        );
        jyc_types::state_dir::unregister("clone-taken");
    }

    /// The dir another topic works in is not a destination — that topic's files
    /// are not this command's to overwrite.
    #[tokio::test]
    async fn clone_refuses_a_dir_another_topic_works_in() {
        let (_tmp, dir) = source_topic().await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let handler = CloneCommandHandler::new(tm.clone());

        let other = tempdir().unwrap();
        tm.set_topic_path("other", other.path().to_path_buf())
            .await
            .unwrap();

        let ctx = context(
            "src-topic",
            &dir,
            "websocket",
            &["clone-owned", other.path().to_str().unwrap()],
        );
        let result = handler
            .clone_under(
                &ctx,
                &dir.join(".jyc"),
                &workspace.path().join("agents"),
                workspace.path(),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.message.contains("'other'"), "{}", result.message);
        jyc_types::state_dir::unregister("other");
    }

    /// Two clones of one source must never share a state dir: the source's own
    /// `.jyc` is skipped, so each gets one under its own name.
    #[tokio::test]
    async fn two_clones_of_one_source_keep_their_own_state() {
        let (_tmp, dir) = source_topic().await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let handler = CloneCommandHandler::new(tm.clone());

        let mut states = vec![];
        for n in ["clone-a", "clone-b"] {
            let ctx = context("src-topic", &dir, "websocket", &[n]);
            let result = handler
                .clone_under(&ctx, &dir.join(".jyc"), &agents_root, workspace.path())
                .await
                .unwrap();
            assert!(result.success, "{n}: {}", result.message);
            let dest = dir.parent().unwrap().join(n);
            assert_eq!(
                tokio::fs::read_to_string(dest.join("README.md"))
                    .await
                    .unwrap(),
                "hello",
                "{n} gets its own copy of the content"
            );
            states.push(jyc_dir(n, &dest));
        }
        assert_ne!(states[0], states[1]);
        for n in ["clone-a", "clone-b"] {
            jyc_types::state_dir::unregister(n);
        }
    }

    /// The source dir being gone refuses before anything is created — no
    /// empty destination is left behind.
    #[tokio::test]
    async fn clone_refuses_a_missing_source_dir() {
        let tmp = tempdir().unwrap();
        let gone = tmp.path().join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let handler = CloneCommandHandler::new(tm.clone());

        let ctx = context("src-topic", &gone, "websocket", &["ghost"]);
        let result = handler
            .clone_under(
                &ctx,
                &gone.join(".jyc"),
                &workspace.path().join("agents"),
                workspace.path(),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.message.contains("gone"), "{}", result.message);
        assert!(
            !gone.parent().unwrap().join("ghost").exists(),
            "a refused /clone must not create the destination"
        );
    }

    /// `/clone` is gated to websocket topics like `/fork`.
    #[tokio::test]
    async fn clone_refuses_a_non_websocket_channel() {
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let handler = CloneCommandHandler::new(tm.clone());
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
        assert!(!looks_like_path("scratch"));
        assert!(!looks_like_path("plan-197"));
    }
}
