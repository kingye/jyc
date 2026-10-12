use std::path::{Path, PathBuf};
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

/// Collapse `.` and `..` components lexically — `canonicalize` without
/// touching the filesystem, for destinations that do not exist yet.
///
/// `resolved()` canonicalizes only paths that already exist, so a fresh
/// `../test` would keep its `..` components and break component-wise
/// comparisons below (`..` does not cancel a `starts_with` prefix).
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::ffi::OsStr;
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                // Pop only a real name we pushed; never a root, and never a
                // `..` we kept earlier (double negation must survive).
                if out.file_name().is_some_and(|n| n != OsStr::new("..")) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

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
/// may be non-empty. It may not, however, sit inside this topic's own dir —
/// a nested working dir would inherit the parent's git status, file listings,
/// and close semantics. It creates
/// exactly two things — the destination directory when it is missing, and
/// `<agents-root>/<name>/.jyc` with its registration — and leaves whatever the
/// destination holds exactly as it found it.
///
/// Websocket topics only, like `/fork`: on every other channel the topic name
/// *is* the routing address, so spawning one would mean inventing an address.
pub struct SpawnCommandHandler {
    topic_manager: Arc<TopicManager>,
}

/// What a spawn works from: the parent's identity, the parent's topic_path
/// (relative destinations resolve against it), and the arguments.
///
/// `/spawn` fills it from its [`CommandContext`]; [`spawn_subagent_under`] —
/// the `jyc_spawn_subagent` tool's entry — fills the same three fields from its
/// own context and passes a single name. That is what lets both triggers run
/// [`SpawnCommandHandler::spawn_under`] instead of one implementation apiece:
/// the only difference between them is who decided to spawn.
///
/// Terminology (`DESIGN.md` → Terminology): a topic's **topic_path** is the
/// working directory it runs in — possibly outside the agents root, possibly a
/// pre-existing checkout; its **state dir** is the `.jyc` that always sits
/// under the agents root, located by name.
struct SpawnRequest<'a> {
    parent_topic: &'a str,
    parent_topic_path: &'a Path,
    args: &'a [String],
}

impl SpawnCommandHandler {
    pub fn new(topic_manager: Arc<TopicManager>) -> Self {
        Self { topic_manager }
    }

    /// Testable core of [`Self::execute`] with every root passed in, so no test
    /// can reach the real data home (mirrors `ForkCommandHandler::fork_under`).
    /// A relative destination resolves against `req.topic_path`, which is
    /// what makes `../test` mean "beside this topic".
    ///
    /// The one implementation behind both triggers: `/spawn` and the
    /// `jyc_spawn_subagent` tool ([`spawn_subagent_under`]) both seed the
    /// parent's state and both grant the parent access.
    async fn spawn_under(
        &self,
        req: SpawnRequest<'_>,
        parent_state: &Path,
        agents_root: &Path,
    ) -> Result<CommandResult> {
        // One name and one path, told apart by shape rather than position, so
        // `/spawn scratch`, `/spawn ../test` and `/spawn scratch workdir` all
        // read naturally.
        let mut name = None;
        let mut path = None;
        for arg in req.args.iter().map(|a| a.trim()).filter(|a| !a.is_empty()) {
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
            .map(|p| resolve_topic_path(p, req.parent_topic_path));
        let name = match name {
            Some(name) => name,
            None => match requested.as_ref().and_then(|p| p.file_name()) {
                Some(file) => file.to_string_lossy().into_owned(),
                None => next_free_name(&self.topic_manager, req.parent_topic, agents_root).await,
            },
        };
        // The child's topic_path: the named dir, or the path's last component,
        // or `<agents root>/<name>`. Where its *state* goes is a different
        // question, answered from the name further down.
        let topic_path = requested
            .map(|p| normalize_lexically(&p))
            .unwrap_or_else(|| agents_root.join(&name));

        if let Some(reason) = invalid_name(&name) {
            return Ok(fail(format!("/spawn: {reason}. Usage: {USAGE}")));
        }
        if name == req.parent_topic {
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
        let parent_resolved = resolved(req.parent_topic_path);
        let child_resolved = resolved(&topic_path);
        if child_resolved == parent_resolved {
            return Ok(fail(format!(
                "/spawn: {} is the directory you are in — pick another path.",
                topic_path.display()
            )));
        }
        // A nested working dir would inherit the parent's git status, file
        // listings, and close semantics, so the destination must stay
        // strictly outside the source topic's own dir.
        if child_resolved.starts_with(&parent_resolved) {
            return Ok(fail(format!(
                "/spawn: {} is inside the current topic's directory — pick a path outside it.",
                topic_path.display()
            )));
        }
        match tokio::fs::metadata(&topic_path).await {
            Ok(meta) if !meta.is_dir() => {
                return Ok(fail(format!(
                    "/spawn: {} exists and is not a directory.",
                    topic_path.display()
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
                name.as_str() != req.parent_topic && resolved(pin) == child_resolved
            })
            .map(|(name, _)| name)
        {
            return Ok(fail(format!(
                "/spawn: {} is the topic dir of '{owner}'.",
                topic_path.display()
            )));
        }

        // Created now, so a fresh `../new-dir` is canonical by the time it is
        // registered — a topic's path should not carry `..` in it. An existing
        // directory is left alone.
        tokio::fs::create_dir_all(&topic_path)
            .await
            .with_context(|| format!("/spawn: could not create {}", topic_path.display()))?;
        let topic_path = resolved(&topic_path);

        // Register the spawn's own state under its name, then pin the dir —
        // `/fork`'s order, so `set_topic_path` reuses this registration instead
        // of deriving one of its own. Two different things, never conflated:
        // the state dir is always `<agents root>/<name>/.jyc` and is never
        // inside `topic_path`.
        let state_dir = agents_root.join(&name).join(".jyc");
        crate::topic_path::adopt_state_dir(&name, &topic_path, &state_dir)?;
        self.topic_manager
            .set_topic_path(&name, topic_path.clone())
            .await?;
        let seeded = seed_state(parent_state, &state_dir, req.parent_topic, "spawned-from").await?;

        // The parent keeps its spawn in sight: this one grant — on the child's
        // **topic_path**, so the parent can put the child's input files there —
        // is what the prompt's subagent list, the cap check, `/close`'s
        // revocation and the parent's own read+write boundary all read
        // (`subagents_of`, which finds the lineage in the state dir above).
        // `/grant`'s registry, so the same "process-lifetime" semantics apply.
        jyc_types::access_grants::grant(req.parent_topic, topic_path.clone(), true);

        tracing::info!(
            from = %req.parent_topic,
            to = %name,
            topic_path = %topic_path.display(),
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
                req.parent_topic,
                name,
                topic_path.display(),
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
        let req = SpawnRequest {
            parent_topic: &context.topic_name,
            parent_topic_path: &context.topic_path,
            args: &context.args,
        };
        self.spawn_under(req, &parent_state, &agents_root).await
    }
}

/// How many subagents one topic may hold at a time.
pub const MAX_SUBAGENTS: usize = 3;

/// Create a subagent topic for `parent`'s agent — the `jyc_spawn_subagent`
/// tool's trigger for [`SpawnCommandHandler::spawn_under`].
///
/// The layout, the inheritance and the grant are `/spawn`'s: a sibling under
/// the agents root carrying a copy of the parent's state (`pattern`,
/// conversation, task list, `mode-override`, …) with its own `.jyc`, marked
/// `spawned-from` and granted to the parent read+write. What is *not* `/spawn`'s
/// is the guard set — one level deep, [`MAX_SUBAGENTS`] children at most — and
/// the fact that the name is resolved here rather than taken from arguments.
/// Returns the new topic's name.
///
/// Seeding the child's directory and sending it the task stay with the caller:
/// `write` for the former, `jyc_send_to_topic` for the latter. Reconfiguring the
/// child is a command away — `jyc_send_to_topic` with a body of `/build`,
/// `/model`, `/context`, … runs it in the child's topic without spending one of
/// its turns (an empty body stops before the model call).
pub async fn spawn_subagent(
    tm: &Arc<TopicManager>,
    parent: &str,
    parent_dir: &Path,
    name: Option<&str>,
) -> Result<String, String> {
    // Production path only, like `/spawn`: `agents_workspace_root()` resolves
    // the real data home, so a test that reached here would create topics
    // inside the developer's own agents tree. Tests call
    // `spawn_subagent_under` with every root.
    let agents_root = tm.agents_workspace_root();
    spawn_subagent_under(tm, parent, parent_dir, name, &agents_root).await
}

/// [`spawn_subagent`] with an explicit agents root — the same testable-core
/// pattern as `spawn_under` / `close_topic_under`.
pub async fn spawn_subagent_under(
    tm: &Arc<TopicManager>,
    parent: &str,
    parent_dir: &Path,
    name: Option<&str>,
    agents_root: &Path,
) -> Result<String, String> {
    let parent_state = state_dir_of(parent, parent_dir);
    if parent_state.join("spawned-from").exists() {
        return Err(
            "this topic is itself a subagent: subagents cannot spawn subagents".to_string(),
        );
    }
    let existing = subagents_of(parent);
    if existing.len() >= MAX_SUBAGENTS {
        return Err(format!(
            "already at {MAX_SUBAGENTS} subagents ({}): continue one with \
             jyc_send_to_topic, or close one first",
            format_subagent_names(&existing)
        ));
    }

    // Resolved here because the caller reports the name back, and validated
    // because `spawn_under` tells a name from a path by shape: a `../evil` that
    // reached it unchecked would be resolved as a path, off the agents root and
    // outside everything this module assumes about a subagent's directory.
    let name = match name {
        Some(n) => {
            if let Some(why) = invalid_name(n) {
                return Err(format!("'{n}' cannot be a subagent name: {why}"));
            }
            n.to_string()
        }
        None => next_free_name(tm, parent, agents_root).await,
    };

    let args = vec![name.clone()];
    let req = SpawnRequest {
        parent_topic: parent,
        parent_topic_path: parent_dir,
        args: &args,
    };
    let handler = SpawnCommandHandler::new(tm.clone());
    match handler.spawn_under(req, &parent_state, agents_root).await {
        Ok(result) if result.success => Ok(name),
        // `/spawn`'s refusals are worded for the command; this caller is a tool.
        Ok(refusal) => Err(refusal
            .message
            .strip_prefix("/spawn: ")
            .unwrap_or(&refusal.message)
            .to_string()),
        Err(e) => Err(format!("failed to create subagent: {e:#}")),
    }
}

/// A topic's **state dir** — its `.jyc`, which always sits under the agents
/// root and is found by name; `topic_path` is only the fallback for a topic
/// that predates registration.
///
/// [`jyc_dir`] cannot be used here: it panics on an unregistered name, and a
/// grant can name a directory no topic owns (a hand-made `/grant`). This is the
/// single answer to "where is this topic's state", shared by both spawn
/// triggers and both readers below, so no caller has to guess whether the state
/// of a `/spawn <path>` child sits inside the path it was given.
fn state_dir_of(name: &str, topic_path: &Path) -> PathBuf {
    jyc_types::state_dir::registered_state(name).unwrap_or_else(|| topic_path.join(".jyc"))
}

/// The subagent directories `topic` currently holds, as `(name, topic_path)`.
///
/// A granted directory counts as a subagent while it still exists and its state
/// dir's `spawned-from` names `topic` — the lineage breadcrumb
/// [`SpawnCommandHandler::spawn_under`] writes for every spawn, whether `/spawn`
/// (with or without a path) or the `jyc_spawn_subagent` tool made it. The
/// breadcrumb is read from the **state dir**, located by the granted dir's name
/// and never assumed to be `<granted dir>/.jyc`: a `/spawn <path>` child runs
/// in the path it was given while its state stays under the agents root. A
/// stale grant (the child was closed) and a directory later reused by another
/// topic are both left out, so the count, the cap check and the prompt's
/// subagent list cannot drift from reality.
pub fn subagents_of(topic: &str) -> Vec<(String, PathBuf)> {
    jyc_types::access_grants::grants_for(topic)
        .into_iter()
        .filter_map(|g| {
            let name = g.path.file_name()?.to_string_lossy().into_owned();
            let lineage = state_dir_of(&name, &g.path).join("spawned-from");
            std::fs::read_to_string(lineage)
                .is_ok_and(|from| from.trim() == topic)
                .then_some((name, g.path))
        })
        .collect()
}

/// Drop the access `child`'s parent holds on it, called when the child closes.
///
/// The lineage breadcrumb names the parent; grants are keyed by topic name and
/// the parent granted exactly the child's topic_path, so its own close revokes
/// it. Read from the child's **state dir** (`topic_path` is only the fallback),
/// so a child spawned into a named path revokes too. A topic without a
/// breadcrumb — i.e. not a subagent — must stay a no-op rather than an error,
/// which is why a missing state dir and an unregistered name are both fine.
pub fn forget_subagent(child: &str, child_dir: &Path) {
    let Ok(parent) = std::fs::read_to_string(state_dir_of(child, child_dir).join("spawned-from"))
    else {
        return;
    };
    if jyc_types::access_grants::ungrant(parent.trim(), child_dir) {
        tracing::info!(
            parent = parent.trim(),
            subagent = %child,
            "Subagent closed; parent access revoked"
        );
    }
}

/// `a, b, c` — names only, for refusal messages and the prompt's subagent list.
pub fn format_subagent_names(subagents: &[(String, PathBuf)]) -> String {
    subagents
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
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
            ("mode-override", "plan"),
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

    /// The `/spawn` side of a call: the three fields `spawn_under` reads, off
    /// the context the production path would have built.
    fn spawn_request(ctx: &CommandContext) -> SpawnRequest<'_> {
        SpawnRequest {
            parent_topic: &ctx.topic_name,
            parent_topic_path: &ctx.topic_path,
            args: &ctx.args,
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
            .spawn_under(spawn_request(&ctx), &dir.join(".jyc"), &agents_root)
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
            "mode-override",
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

        // A spawn is one of the parent's subagents: this grant is what the
        // parent's read+write boundary, the prompt's subagent list and the cap
        // check all read — the same for `/spawn` and for the tool.
        let grants = jyc_types::access_grants::grants_for("src-topic");
        assert_eq!(grants.len(), 1, "one spawn, one grant: {grants:?}");
        assert_eq!(grants[0].path, dest);
        assert!(
            grants[0].write,
            "the parent seeds its spawn through this grant"
        );
        assert_eq!(
            subagents_of("src-topic"),
            vec![("src-topic-2".to_string(), dest.clone())]
        );

        // Closing the spawn removes the emptied shell with its state — the
        // close-side half of the default shape — and hands the access back.
        tm.close_topic("src-topic-2").await.unwrap();
        assert!(
            !dest.exists(),
            "an emptied spawn dir goes away with its state"
        );
        assert!(
            subagents_of("src-topic").is_empty(),
            "closing the spawn revokes the parent's access"
        );

        jyc_types::state_dir::unregister("src-topic-2");
        jyc_types::access_grants::clear("src-topic");
    }

    /// A spawn's dir that gained files is user property by then: `/close`
    /// deletes the state and keeps the dir with whatever is in it.
    #[tokio::test]
    async fn close_keeps_a_spawn_dir_that_gained_files() {
        let (_tmp, dir) = parent_topic().await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let handler = SpawnCommandHandler::new(tm.clone());

        let ctx = context("filed-parent", &dir, "websocket", &["filed-shell"]);
        let result = handler
            .spawn_under(spawn_request(&ctx), &dir.join(".jyc"), &agents_root)
            .await
            .unwrap();
        assert!(result.success, "{}", result.message);
        let dest = resolved(&agents_root.join("filed-shell"));
        tokio::fs::write(dest.join("user-note.txt"), "keep me")
            .await
            .unwrap();

        tm.close_topic("filed-shell").await.unwrap();
        assert!(dest.join("user-note.txt").exists(), "the user's file stays");
        assert!(!dest.join(".jyc").exists(), "the state is deleted");
        tokio::fs::remove_dir_all(&dest).await.unwrap();

        jyc_types::state_dir::unregister("filed-shell");
        jyc_types::access_grants::clear("filed-parent");
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
            "checkout-parent",
            &dir,
            "websocket",
            &[checkout.to_str().unwrap()],
        );
        let result = handler
            .spawn_under(
                spawn_request(&ctx),
                &dir.join(".jyc"),
                &workspace.path().join("agents"),
            )
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
        // A path-form spawn is a subagent too: the dir it was given is the dir
        // the parent may now read and write.
        assert_eq!(
            jyc_types::access_grants::grants_for("checkout-parent")
                .iter()
                .map(|g| g.path.clone())
                .collect::<Vec<_>>(),
            vec![resolved(&checkout)],
        );
        assert_eq!(
            subagents_of("checkout-parent"),
            vec![("checkout".to_string(), resolved(&checkout))],
            "awareness reads the state dir by name, not the topic_path it granted"
        );

        // Closing it deletes the conversation and leaves the checkout: the
        // state lives outside the dir, so the emptied-dir cleanup never fires.
        let agents_root = workspace.path().join("agents");
        tm.close_topic("checkout").await.unwrap();
        assert!(
            checkout.join("src/main.rs").exists(),
            "the checkout is untouched"
        );
        assert!(
            !agents_root.join("checkout").join(".jyc").exists(),
            "the state is deleted"
        );
        assert!(
            jyc_types::access_grants::grants_for("checkout-parent").is_empty(),
            "closing the spawn revokes the parent's access"
        );
        assert!(
            subagents_of("checkout-parent").is_empty(),
            "and drops it from the parent's subagent list"
        );

        jyc_types::state_dir::unregister("checkout");
        jyc_types::access_grants::clear("checkout-parent");
    }

    /// A relative destination is taken against this topic's dir, so `../test`
    /// lands beside the parent — allowed, because nothing is written into it.
    #[tokio::test]
    async fn spawn_resolves_a_relative_path_against_this_topic() {
        let (_tmp, dir) = parent_topic().await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let handler = SpawnCommandHandler::new(tm.clone());
        let expected = dir.parent().unwrap().join("test");

        let ctx = context("relative-parent", &dir, "websocket", &["../test"]);
        let result = handler
            .spawn_under(
                spawn_request(&ctx),
                &dir.join(".jyc"),
                &workspace.path().join("agents"),
            )
            .await
            .unwrap();
        assert!(result.success, "../test: {}", result.message);
        assert_eq!(
            tm.topic_path("test").await.unwrap(),
            resolved(&expected),
            "'../test' must pin the resolved dir, with no '..' left in it"
        );
        assert!(
            !expected.join(".jyc").exists(),
            "'../test' must not put state in the destination"
        );
        // State and awareness ride on the name, not on the path: the lineage
        // lives in the state dir under the agents root, and the parent sees the
        // child even though its topic_path is beside the parent's.
        assert!(
            state_dir_of("test", &expected)
                .join("spawned-from")
                .exists(),
            "the lineage belongs to the state dir, never to the topic_path"
        );
        assert_eq!(
            subagents_of("relative-parent"),
            vec![("test".to_string(), resolved(&expected))],
            "a relative-path spawn is a subagent of its parent"
        );

        jyc_types::state_dir::unregister("test");
        jyc_types::access_grants::clear("relative-parent");
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
            (vec!["./sub"], "inside the current topic"),
            (vec!["nested", "./deep"], "inside the current topic"),
            (
                vec!["nested", dir.join("deep").to_str().unwrap()],
                "inside the current topic",
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
                .spawn_under(spawn_request(&ctx), &dir.join(".jyc"), &agents_root)
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

    /// The tool's entry runs `/spawn`'s implementation: the same inheritance (the
    /// parent's state, `mode-override` included), the same breadcrumb, the same
    /// grant — the guards and the name resolution are what it adds.
    #[tokio::test]
    async fn subagent_spawn_inherits_state_and_grants_access() {
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let parent = "sa-parent-create";
        let parent_dir = workspace.path().join("parent");
        let parent_state = parent_dir.join(".jyc");
        std::fs::create_dir_all(&parent_state).unwrap();
        for (name, body) in [
            ("pattern", "jyc"),
            ("agent-context.json", "{\"messages\":[]}"),
            ("chat_history_2026-09-24.jsonl", "{}"),
            ("mode-override", "plan"),
            ("agent-session.json", "{\"current_input_tokens\":900}"),
            ("activity.jsonl", "{}"),
        ] {
            std::fs::write(parent_state.join(name), body).unwrap();
        }
        jyc_types::access_grants::clear(parent);

        let name = spawn_subagent_under(&tm, parent, &parent_dir, Some("worker"), &agents_root)
            .await
            .unwrap();
        assert_eq!(name, "worker");

        let dest = agents_root.join("worker");
        assert_eq!(
            std::fs::read_to_string(dest.join(".jyc").join("spawned-from")).unwrap(),
            parent
        );
        assert!(tm.topic_path("worker").await.is_some());
        // The parent's own `.jyc` (a pinned topic's, say) keeps its name, and
        // the child's state is the child's: its dir is its own, not the
        // parent's working tree (that is `/fork`).
        for keep in [
            "pattern",
            "agent-context.json",
            "chat_history_2026-09-24.jsonl",
            "mode-override",
        ] {
            assert!(
                dest.join(".jyc").join(keep).exists(),
                "{keep} must be inherited, exactly as `/spawn` does it"
            );
        }
        for drop in ["agent-session.json", "activity.jsonl"] {
            assert!(
                !dest.join(".jyc").join(drop).exists(),
                "{drop} must stay with the parent"
            );
        }
        // Only the state: a spawn hands over the conversation, never workspace
        // files (the parent dir holds no files here, so `.jyc` is all there is).
        let entries: Vec<_> = std::fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from(".jyc")]);

        let grants = jyc_types::access_grants::grants_for(parent);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].path, dest);
        assert!(grants[0].write, "the parent must be able to seed the dir");
        assert_eq!(subagents_of(parent), vec![("worker".to_string(), dest)]);

        jyc_types::access_grants::clear(parent);
        jyc_types::state_dir::unregister("worker");
    }

    /// A name is not an argument here: `spawn_under` tells names from paths by
    /// shape, so `../evil` left unchecked would be resolved as a path — off the
    /// agents root, outside everything this module assumes about a subagent.
    #[tokio::test]
    async fn subagent_spawn_refuses_a_path_shaped_name() {
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let parent = "sa-parent-name";
        let parent_dir = workspace.path().join("parent");
        std::fs::create_dir_all(parent_dir.join(".jyc")).unwrap();
        jyc_types::access_grants::clear(parent);

        let err = spawn_subagent_under(&tm, parent, &parent_dir, Some("../evil"), &agents_root)
            .await
            .unwrap_err();
        assert!(err.contains("cannot be a subagent name"), "{err}");
        assert!(
            !workspace.path().join("evil").exists() && !agents_root.exists(),
            "a refusal creates nothing anywhere"
        );
    }

    /// One level only: a subagent cannot spawn its own subagent.
    #[tokio::test]
    async fn subagent_spawn_refuses_a_second_level() {
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let parent_dir = workspace.path().join("nested");
        std::fs::create_dir_all(parent_dir.join(".jyc")).unwrap();
        std::fs::write(parent_dir.join(".jyc").join("spawned-from"), "grandparent").unwrap();

        let err = spawn_subagent_under(
            &tm,
            "sa-parent-nested",
            &parent_dir,
            Some("worker"),
            &agents_root,
        )
        .await
        .unwrap_err();
        assert!(err.contains("cannot spawn subagents"), "{err}");
        assert!(!agents_root.join("worker").exists());
    }

    #[tokio::test]
    async fn subagent_spawn_refuses_past_the_cap() {
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_root = workspace.path().join("agents");
        let parent = "sa-parent-cap";
        let parent_dir = workspace.path().join("parent");
        std::fs::create_dir_all(&parent_dir).unwrap();
        jyc_types::access_grants::clear(parent);
        for i in 0..MAX_SUBAGENTS {
            let dir = agents_root.join(format!("filled-{i}"));
            std::fs::create_dir_all(dir.join(".jyc")).unwrap();
            std::fs::write(dir.join(".jyc").join("spawned-from"), parent).unwrap();
            jyc_types::access_grants::grant(parent, dir, true);
        }

        let err = spawn_subagent_under(&tm, parent, &parent_dir, Some("one-more"), &agents_root)
            .await
            .unwrap_err();
        assert!(err.contains("already at 3 subagents"), "{err}");
        assert!(
            err.contains("filled-0"),
            "the refusal lists what is already there: {err}"
        );
        jyc_types::access_grants::clear(parent);
    }

    /// A stale grant (child closed) and a directory another topic owns are
    /// neither counted nor listed.
    #[test]
    fn subagents_of_skips_stale_and_foreign_grants() {
        let workspace = tempdir().unwrap();
        let parent = "sa-parent-list";
        jyc_types::access_grants::clear(parent);

        let mine = workspace.path().join("mine");
        std::fs::create_dir_all(mine.join(".jyc")).unwrap();
        std::fs::write(mine.join(".jyc").join("spawned-from"), parent).unwrap();
        let theirs = workspace.path().join("theirs");
        std::fs::create_dir_all(theirs.join(".jyc")).unwrap();
        std::fs::write(theirs.join(".jyc").join("spawned-from"), "someone-else").unwrap();
        let gone = workspace.path().join("gone");
        for p in [&mine, &theirs, &gone] {
            jyc_types::access_grants::grant(parent, p.clone(), true);
        }

        assert_eq!(
            subagents_of(parent),
            vec![("mine".to_string(), mine.clone())]
        );

        forget_subagent("mine", &mine);
        assert!(subagents_of(parent).is_empty());
        assert_eq!(format_subagent_names(&[]), "");
        jyc_types::access_grants::clear(parent);
    }
}
