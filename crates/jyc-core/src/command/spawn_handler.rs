use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use jyc_types::state_dir::resolve_state_dir;
use jyc_types::{AppConfig, TopicConfig};
use tracing::instrument;

use super::fork_handler::{
    fail, invalid_name, next_free_name, seed_start_model, seed_state, take_inherit_flag,
};
use super::handler::{CommandContext, CommandHandler, CommandResult};
use crate::topic_manager::TopicManager;
use crate::topic_path::{resolve_topic_path, resolved};

/// How to call it, repeated in every refusal so a typo self-corrects.
const USAGE: &str = "/spawn [--inherit] [--type=<name>] [name] [path]";

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

/// `/spawn` — start a sibling topic that carries this topic's settings into a
/// directory of its own.
///
/// The difference from `/fork` is only *where the new topic works*: a fork
/// keeps this topic's directory, a spawn gets another one — a fresh dir under
/// the agents root, or any dir you name, including one that already holds files
/// (a checkout made elsewhere). What comes along is the state, not the
/// workspace: the same allow-list `/fork` seeds (backlog and settings, never a
/// task list) marked `spawned-from`, so a spawn inherits neither jobs that would
/// double-fire nor a token meter it did not run up. The conversation and the
/// transcript are the one part that stays behind by default — a worker starting
/// on a task of its own does not need the parent's train of thought — and
/// `--inherit` copies them too (see `seed_state`).
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
/// (relative destinations resolve against it), the arguments, and the two
/// things the trigger decides rather than the arguments.
///
/// `/spawn` fills it from its [`CommandContext`]; [`spawn_subagent_under`] —
/// the `jyc_spawn_subagent` tool's entry — fills the same fields from its
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
    /// Copy the conversation and the transcript as well — the opposite of
    /// `seed_state`'s default. `/spawn --inherit`, or the tool's
    /// `inherit_context`.
    inherit: bool,
    /// Work in the parent's own directory instead of a new one. Tool-only as an
    /// *argument*: a typed `/spawn` never asks for it, and a role asks for it
    /// with its own `[spawn]` default (a reviewer of uncommitted work must).
    share_topic_path: bool,
    /// The role the child starts in, already resolved from its name by the
    /// trigger ([`resolve_subagent_type`]) — the request carries the answer, not
    /// the name, so this core never needs a config to read.
    subagent_type: Option<&'a SubagentType>,
    /// The model the parent is running, for the child's own starting point —
    /// resolved by whichever trigger holds a config ([`Self::execute`], or the
    /// tool) and dropped again when the role names a model of its own. `None`
    /// leaves the child on whatever its inherited state resolves to.
    start_model: Option<String>,
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
        agents_dir: &Path,
    ) -> Result<CommandResult> {
        // One name and one path, told apart by shape rather than position, so
        // `/spawn scratch`, `/spawn ../test` and `/spawn scratch/some-dir` all
        // read naturally. `--inherit` is neither: both callers take it out of
        // the arguments before building the request (`take_inherit_flag`), so
        // what arrives here is only ever a name or a path.
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
                None => next_free_name(&self.topic_manager, req.parent_topic, agents_dir).await,
            },
        };
        // A role that works in this topic's directory has no destination of its
        // own to be handed: the path would be dropped (the child is *named* from
        // it and then runs here), and a destination is the one argument that must
        // never be silently ignored — so say so instead of quietly disagreeing
        // with the caller about where the child works.
        if req.share_topic_path && requested.is_some() {
            let who = match req.subagent_type {
                Some(t) => format!("'{}' works in this topic's directory", t.name),
                None => "this spawn works in this topic's directory".to_string(),
            };
            return Ok(fail(format!(
                "/spawn: {who}; give a name, not a path — or spawn it with \
                 `jyc_spawn_subagent(share_topic_path = false)` to give it a directory of its own."
            )));
        }

        // The child's topic_path: the named dir, or the path's last component,
        // or `<agents root>/<name>` — unless the child was asked to share this
        // topic's own dir, which is the same co-pinning `/fork` does with a
        // different starting state. Where its *state* goes is a different
        // question, answered from the name further down: always the child's own
        // `<agents root>/<name>/.jyc`, shared dir or not.
        let topic_path = if req.share_topic_path {
            req.parent_topic_path.to_path_buf()
        } else {
            requested
                .map(|p| normalize_lexically(&p))
                .unwrap_or_else(|| agents_dir.join(&name))
        };

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
        if self.topic_manager.topic_path(&name).await.is_some() || agents_dir.join(&name).exists() {
            return Ok(fail(format!(
                "/spawn: topic '{name}' already exists. Give it another name."
            )));
        }

        // A destination that exists must be a directory (shared or not) — asked
        // first, so the refusals below all speak about ownership rather than
        // about a shape the destination does not have.
        match tokio::fs::metadata(&topic_path).await {
            Ok(meta) if !meta.is_dir() => {
                return Ok(fail(format!(
                    "/spawn: {} exists and is not a directory.",
                    topic_path.display()
                )));
            }
            _ => {}
        }

        // Nothing below this point may create anything until every guard has
        // passed. Comparisons go through `resolved`, which canonicalizes what
        // exists and leaves the rest as written: a path that exists cannot reach
        // two names through `..` or a symlink, and a path that does not exist
        // yet cannot already be somebody else's topic dir.
        let parent_resolved = resolved(req.parent_topic_path);
        let child_resolved = resolved(&topic_path);
        // The three guards below all ask "is this destination somebody's own
        // dir?", and a shared topic_path is *this* topic's own dir by
        // construction — the one case they exist to keep a spawn out of, and
        // the one of its kind that is not refused for being somebody else's too
        // (a repo two agents co-pin is what `/fork` already does). So all three
        // step aside on purpose, the "topic dir of '<owner>'" one included:
        // taking the parent's own dir cannot take anybody else's.
        if !req.share_topic_path {
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
        // the state dir is always `<agents root>/<name>/.jyc` — inside
        // `topic_path` in the default shape, beside the parent's own state when
        // the dir is shared — and never the parent's.
        let state_dir = agents_dir.join(&name).join(".jyc");
        crate::topic_path::adopt_state_dir(&name, &topic_path, &state_dir)?;
        self.topic_manager
            .set_topic_path(&name, topic_path.clone())
            .await?;
        // Seed into the dir the registry settled on, not the one computed above:
        // a name that was already registered (closed, then reopened) keeps that
        // registration, and every later read — the child's own runtime included
        // — goes through `resolve_state_dir`. `set_topic_path` created it.
        let state_dir = jyc_types::state_dir::resolve_state_dir(&name, &topic_path);
        let seeded = seed_state(
            parent_state,
            &state_dir,
            req.parent_topic,
            "spawned-from",
            !req.inherit,
        )
        .await?;

        // The role, on top of what the child just inherited: its settings, its
        // mode, and the name it answers to in the dashboard. `seed_state` has
        // already copied the *parent's* runtime choices (the override files it
        // was running with) — those are exactly what a role replaces, so they
        // go first.
        if let Some(subagent_type) = req.subagent_type {
            apply_subagent_type(&state_dir, subagent_type).with_context(|| {
                format!(
                    "/spawn: could not apply subagent type '{}' to {}",
                    subagent_type.name,
                    state_dir.display()
                )
            })?;
        }

        // The starting model, last: the parent's, unless the role names one of
        // its own. By now the role's removals have run, so what is left in the
        // child's state is exactly what a role with no `[ai]` inherits — and
        // `model-override` is the slot that fills it (`seed_start_model`).
        let role_has_model = req
            .subagent_type
            .is_some_and(|t| declares_a_model(&t.config));
        if !role_has_model {
            seed_start_model(&state_dir, req.start_model.as_deref()).await;
        }

        // The parent keeps its spawn in sight: this one grant — on the child's
        // **topic_path**, so the parent can put the child's input files there,
        // and **naming the child**, since a shared dir cannot name it — is what
        // the prompt's subagent list, the cap check, `/close`'s revocation and
        // the parent's own read+write boundary all read (`subagents_of`, which
        // checks the lineage breadcrumb in the state dir above). `/grant`'s
        // registry, so the same "process-lifetime" semantics apply.
        jyc_types::access_grants::grant_subagent(req.parent_topic, &name, topic_path.clone());

        tracing::info!(
            from = %req.parent_topic,
            to = %name,
            topic_path = %topic_path.display(),
            state = %state_dir.display(),
            seeded,
            inherit = req.inherit,
            shared = req.share_topic_path,
            subagent_type = req.subagent_type.map(|t| t.name.as_str()).unwrap_or("-"),
            "Topic spawned"
        );
        Ok(CommandResult {
            success: true,
            message: format!(
                "✅ Spawned '{}' → '{}'{} — it works in {} with its own state in {} ({} state \
                 file(s) inherited{}, no workspace files copied).\n/spawn does not switch by \
                 itself — pick '{name}' in the topic list.",
                req.parent_topic,
                name,
                match req.subagent_type {
                    Some(t) => format!(" as type '{}'", t.name),
                    None => String::new(),
                },
                topic_path.display(),
                state_dir.display(),
                seeded,
                if req.inherit {
                    ", conversation included"
                } else {
                    ", conversation left behind (--inherit to copy it)"
                }
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
        // Production path only: `resolve_agents_dir()` resolves the real data
        // home, so a test that reached here would create topics inside the
        // developer's own agents tree. Tests call `spawn_under` with every root.
        let agents_dir = self.topic_manager.resolve_agents_dir();
        let parent_state = resolve_state_dir(&context.topic_name, &context.topic_path);
        // `/spawn --inherit …`: the command's only own share is the
        // conversation, read off the arguments here — so a typed `/spawn` asks
        // for neither flag of its own accord, and the role's `[spawn]` defaults
        // fill what the command line left unsaid (below, once the type is read).
        let (inherit, args) = take_inherit_flag(&context.args);
        let (type_name, args) = match take_type_flag(&args) {
            Ok(parsed) => parsed,
            Err(refusal) => return Ok(fail(format!("/spawn: {refusal}. Usage: {USAGE}"))),
        };
        // Which role the child starts in, resolved before anything is created:
        // an unknown or malformed type refuses here, not halfway through a
        // spawn. The config is the one this inbound message was routed with —
        // not a fresh read of the default config path, which would miss the
        // `--config <FILE>` the daemon was started with.
        let subagent_type = match resolve_type_arg(type_name.as_deref(), &context.config) {
            Ok(resolved) => resolved,
            Err(refusal) => return Ok(fail(format!("/spawn: {refusal}"))),
        };
        // The child starts on this topic's model, resolved the way the runtime
        // resolves it and against the same live config the type was — the
        // parent's own state says what it is running (see `seed_start_model`).
        let start_model = crate::session_state::resolve_effective_model(
            &context.topic_name,
            &context.topic_path,
            &context.config,
            &context.channel,
        )
        .await;
        let (inherit, share_topic_path) =
            type_start_flags(subagent_type.as_ref(), inherit.then_some(true), None);
        let req = SpawnRequest {
            parent_topic: &context.topic_name,
            parent_topic_path: &context.topic_path,
            args: &args,
            inherit,
            share_topic_path,
            subagent_type: subagent_type.as_ref(),
            start_model,
        };
        self.spawn_under(req, &parent_state, &agents_dir).await
    }
}

/// Create a subagent topic for `parent`'s agent — the `jyc_spawn_subagent`
/// tool's trigger for [`SpawnCommandHandler::spawn_under`].
///
/// The layout, the inheritance and the grant are `/spawn`'s: a sibling under
/// the agents root carrying a copy of the parent's state (`pattern`,
/// `mode-override`, … — its task list is not among them) with its own `.jyc`,
/// marked `spawned-from` and granted to the parent read+write. What is *not*
/// `/spawn`'s is that the name is resolved
/// here rather than taken from arguments.
///
/// `inherit_context` and `share_topic_path` are the tool's two decisions about
/// the child's *starting point*, both off by default: the conversation and
/// transcript stay with the parent (see `seed_state`), and the child gets a
/// working dir of its own. `share_topic_path` makes it work in the parent's
/// directory instead — the `/fork` shape, for a reviewer that has to read
/// uncommitted work — and its own state still goes to
/// `<agents root>/<name>/.jyc`, so closing it never touches that directory.
/// Returns the new topic's name.
///
/// Seeding the child's directory and sending it the task stay with the caller:
/// `write` for the former, `jyc_send_to_topic` for the latter. Reconfiguring the
/// child is a command away — `jyc_send_to_topic` with a body of `/build`,
/// `/model`, `/context`, … runs it in the child's topic without spending one of
/// its turns (an empty body stops before the model call).
///
/// `subagent_type` is resolved by the caller ([`resolve_type_arg`]), which has
/// the live config: this function only needs the answer. `start_model` is the
/// parent's own model, resolved the same way by the caller that holds a config
/// ([`crate::session_state::resolve_effective_model`]) — the child starts on it
/// unless the role names one.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_subagent(
    tm: &Arc<TopicManager>,
    parent: &str,
    parent_dir: &Path,
    name: Option<&str>,
    inherit_context: bool,
    share_topic_path: bool,
    subagent_type: Option<&SubagentType>,
    start_model: Option<&str>,
) -> Result<String, String> {
    // Production path only, like `/spawn`: `resolve_agents_dir()` resolves
    // the real data home, so a test that reached here would create topics
    // inside the developer's own agents tree. Tests call
    // `spawn_subagent_under` with every root.
    let agents_dir = tm.resolve_agents_dir();
    // The role arrives already resolved, by the caller that has a config to
    // resolve it against ([`resolve_type_arg`]): the tool layer holds the live
    // `AppConfig`, and a config read here would be a *second* answer to "where
    // does config come from" — one that misses `--config`.
    spawn_subagent_under(
        tm,
        parent,
        parent_dir,
        name,
        inherit_context,
        share_topic_path,
        subagent_type,
        start_model,
        &agents_dir,
    )
    .await
}

/// [`spawn_subagent`] with an explicit agents root — the same testable-core
/// pattern as `spawn_under` / `close_topic_under`, and the same deliberate
/// argument list: every root arrives as an argument, which is what keeps a
/// test out of the developer's own agents tree.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_subagent_under(
    tm: &Arc<TopicManager>,
    parent: &str,
    parent_dir: &Path,
    name: Option<&str>,
    inherit_context: bool,
    share_topic_path: bool,
    subagent_type: Option<&SubagentType>,
    start_model: Option<&str>,
    agents_dir: &Path,
) -> Result<String, String> {
    let parent_state = state_dir_of(parent, parent_dir);

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
        None => next_free_name(tm, parent, agents_dir).await,
    };

    let args = vec![name.clone()];
    let req = SpawnRequest {
        parent_topic: parent,
        parent_topic_path: parent_dir,
        args: &args,
        inherit: inherit_context,
        share_topic_path,
        subagent_type,
        start_model: start_model.map(str::to_string),
    };
    let handler = SpawnCommandHandler::new(tm.clone());
    match handler.spawn_under(req, &parent_state, agents_dir).await {
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
/// [`state_dir`] cannot be used here: it panics on an unregistered name, and a
/// grant can name a directory no topic owns (a hand-made `/grant`). This is the
/// single answer to "where is this topic's state", shared by both spawn
/// triggers and both readers below, so no caller has to guess whether the state
/// of a `/spawn <path>` child sits inside the path it was given.
fn state_dir_of(name: &str, topic_path: &Path) -> PathBuf {
    jyc_types::state_dir::registered_state(name).unwrap_or_else(|| topic_path.join(".jyc"))
}

/// The subagent directories `topic` currently holds, as `(name, topic_path)`.
///
/// Read from the grants `topic` holds plus the lineage breadcrumb each child
/// wrote into its own state dir: a grant counts as a subagent while the child it
/// names still says `topic` spawned it.
///
/// A **spawn's** grant carries the child's name, and that is what this reads —
/// the path cannot supply it, since a shared topic_path is the parent's own dir
/// and its last component names the parent's directory rather than the child.
/// For a `/grant` the user made there is no name to read, so the last path
/// component is the fallback: that keeps `/grant` able to re-list an own-dir
/// child after a restart dropped the grants (the documented recovery), and it is
/// simply the wrong guess for a shared dir — where a re-grant lists nobody.
///
/// Like the grants themselves the list is process-lifetime: a restart drops it.
pub fn subagents_of(topic: &str) -> Vec<(String, PathBuf)> {
    jyc_types::access_grants::grants_for(topic)
        .into_iter()
        .filter_map(|g| {
            let child = g
                .subagent
                .or_else(|| g.path.file_name().map(|n| n.to_string_lossy().into_owned()))?;
            (spawned_from(&child, &g.path).as_deref() == Some(topic)).then_some((child, g.path))
        })
        .collect()
}

/// The topic `name` was spawned from, per the `spawned-from` breadcrumb in its
/// state dir, or `None` if it is not a subagent.
///
/// The same lookup [`subagents_of`] and [`forget_subagent`] do, minus their
/// filters: the breadcrumb alone, which is what outlives the in-memory grant —
/// a closed subagent's state dir is gone too, so this is exactly as durable as
/// the topic is.
pub fn spawned_from(name: &str, topic_path: &Path) -> Option<String> {
    let from = std::fs::read_to_string(state_dir_of(name, topic_path).join("spawned-from")).ok()?;
    let from = from.trim();
    (!from.is_empty()).then(|| from.to_string())
}

/// Drop the access `child`'s parent holds on it, called when the child closes.
///
/// The lineage breadcrumb names the parent; grants are keyed by topic name, so
/// the parent's entry for this child — the one grant that names it — is what
/// goes. By name rather than by path: a shared topic_path is the parent's own
/// dir, and another child may be working there. A topic without a breadcrumb —
/// i.e. not a subagent — must stay a no-op rather than an error, which is why a
/// missing state dir and an unregistered name are both fine.
pub fn forget_subagent(child: &str, child_dir: &Path) {
    let Some(parent) = spawned_from(child, child_dir) else {
        return;
    };
    if jyc_types::access_grants::ungrant_subagent(&parent, child) {
        tracing::info!(
            parent = %parent,
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

/// A spawn's `type`, resolved: the name the caller asked for and the settings
/// that name stands for.
///
/// Resolution happens before a spawn creates anything (in the two entry points,
/// [`resolve_subagent_type`]), so the core below carries the answer rather than
/// the name — it needs no config to read, and a typo refuses before a directory
/// exists.
#[derive(Debug)]
pub struct SubagentType {
    /// The name as given (`review`), recorded in the child's state as
    /// `.jyc/subagent-type` so a role is legible after the fact. Nothing reads
    /// it yet — the dashboard is where it is headed — and the spawn does not
    /// depend on it: what the child runs with is the config it was written.
    pub name: String,
    /// What to write into the child's own `.jyc/config.toml`.
    pub config: TopicConfig,
    /// The role's own one-line "when to spawn me", from its installed file —
    /// listed beside the name wherever a spawn lists the types it accepts.
    pub description: Option<String>,
    /// The role's own defaults for the two start flags, from its `[spawn]`
    /// table. See [`type_start_flags`] for how an argument overrides them.
    pub spawn: Option<jyc_types::SpawnPolicy>,
}

/// Resolve a `type` name into the settings a spawn hands its child.
///
/// Two sources, either of which can define a type:
/// - `[subagents.<type>]` in the effective config — what this instance was
///   configured with;
/// - `<config dir>/subagents/<type>/config.toml` — an installed preset, the
///   shape a shipped bundle can carry.
///
/// When both exist, [`TopicConfig::overlay`] combines them key by key with the
/// in-config table (the running instance's own setting) winning. When neither
/// does, the refusal lists what *is* available: a typo should cost one turn,
/// not a debugging session.
pub fn resolve_subagent_type(
    type_name: &str,
    config: Option<&AppConfig>,
    config_dir: Option<&Path>,
) -> Result<SubagentType, String> {
    let name = type_name.trim();
    // Same guard as a topic name: the type is a directory component under the
    // config dir, and the name arrives from a model-authored tool argument.
    if let Some(reason) = invalid_name(name) {
        return Err(format!("'{name}' cannot be a subagent type name: {reason}"));
    }

    let from_config = config.and_then(|c| c.subagents.get(name)).cloned();
    let installed =
        jyc_types::load_subagent_type(name, config_dir).map_err(|e| format!("{e:#}"))?;

    let settings = match (from_config, installed.as_ref()) {
        (Some(from_config), None) => from_config,
        (None, Some(installed)) => installed.topic.clone(),
        (Some(from_config), Some(installed)) => from_config.overlay(installed.topic.clone()),
        (None, None) => return Err(unknown_type_message(name, config, config_dir)),
    };
    // Only the installed file has these two: `[subagents.<type>]` is a topic
    // config, so a config-only type simply has no spawn defaults and no line to
    // be listed by.
    let description = installed.as_ref().and_then(|f| f.description.clone());
    let spawn = installed.as_ref().and_then(|f| f.spawn.clone());

    // A typo in `mode` would otherwise read as "not plan", i.e. build — a
    // reviewer quietly starting able to write. Refuse instead, where the user
    // can still see which name caused it.
    if let Some(mode) = settings.mode.as_deref()
        && !matches!(mode, "plan" | "build")
    {
        return Err(format!(
            "subagent type '{name}': mode = \"{mode}\" is not a mode — use \"plan\" or \"build\""
        ));
    }

    Ok(SubagentType {
        name: name.to_string(),
        config: settings,
        description,
        spawn,
    })
}

/// The two start flags a spawn actually runs with: an argument passed by the
/// trigger (`Some(true)` — `/spawn --inherit`, the tool's `inherit_context`)
/// wins over the type's own `[spawn]` default, which wins over the built-in
/// `false`: an independent child in a directory of its own.
///
/// `None` is "the caller did not decide", which is what lets `--type=review`
/// alone mean "borrow my directory, without my conversation" — the role states
/// how it is spawned, and one command still can override it.
pub fn type_start_flags(
    subagent_type: Option<&SubagentType>,
    inherit: Option<bool>,
    share: Option<bool>,
) -> (bool, bool) {
    let policy = subagent_type
        .and_then(|t| t.spawn.clone())
        .unwrap_or_default();
    (
        inherit.or(policy.inherit_context).unwrap_or(false),
        share.or(policy.share_topic_path).unwrap_or(false),
    )
}

/// The types a spawn accepts on this machine, one `name — description` line
/// each, for the `type` argument's own description in the tool schema.
///
/// Resolved through [`resolve_subagent_type`] — the answer a spawn itself would
/// get — so the list cannot offer what a spawn would refuse: a type whose name
/// is malformed, or whose file does not parse, is shown as unusable instead of
/// being listed bare. That also gives a role's own description exactly one
/// reader, the place that puts it in front of the model.
/// The type names a spawn would accept on this machine, from either source:
/// [`jyc_types::available_subagent_types`] minus the names [`invalid_name`]
/// refuses. Both the menu the model reads and the list an unknown name is
/// answered with go through here, so neither can offer a name a spawn declines.
fn type_names(config: &AppConfig, config_dir: Option<&Path>) -> Vec<String> {
    jyc_types::available_subagent_types(config, config_dir)
        .into_iter()
        .filter(|name| invalid_name(name).is_none())
        .collect()
}

pub fn type_catalog(config: &AppConfig, config_dir: Option<&Path>) -> Vec<String> {
    type_names(config, config_dir)
        .into_iter()
        .map(
            |name| match resolve_subagent_type(&name, Some(config), config_dir) {
                Ok(ty) => match ty.description.filter(|d| !d.trim().is_empty()) {
                    Some(d) => format!("{name} — {}", d.trim()),
                    None => name,
                },
                // A refusal can be a multi-line TOML parse error, and this text
                // ends up in the tool schema a model reads every turn: keep the
                // first line, capped, so a broken file costs one line.
                Err(refusal) => {
                    let first: String = refusal
                        .lines()
                        .next()
                        .unwrap_or("unusable")
                        .trim()
                        .chars()
                        .take(120)
                        .collect();
                    format!("{name} — (unusable: {first})")
                }
            },
        )
        .collect()
}

/// The refusal for a type nobody defines, naming the ones that do exist.
fn unknown_type_message(
    name: &str,
    config: Option<&AppConfig>,
    config_dir: Option<&Path>,
) -> String {
    let available = config
        .map(|c| type_names(c, config_dir))
        .unwrap_or_default();
    let dir = config_dir
        .map(|d| d.join("subagents").display().to_string())
        .unwrap_or_else(|| "<config dir>/subagents".to_string());
    if available.is_empty() {
        format!(
            "unknown subagent type '{name}': no types are defined. Add [subagents.{name}] to the \
             config, or {dir}/{name}/config.toml"
        )
    } else {
        format!(
            "unknown subagent type '{name}'. Available: {} (from [subagents.*] or {dir}/*)",
            available.join(", ")
        )
    }
}

/// Resolve a spawn's `type` argument against the **live** config and the
/// installed type dirs — the one step both entry points run before they call
/// [`spawn_subagent`] / [`SpawnCommandHandler::spawn_under`]: `/spawn`, which
/// holds the config it was dispatched with (`CommandContext::config`), and the
/// `jyc_spawn_subagent` tool, which holds the service's shared `ArcSwap`.
///
/// Both resolve here so an unknown or malformed type refuses before anything is
/// created, and so neither has to read a config off disk: a fresh read would
/// answer with the *default* config path, not the one this instance is running
/// with (`--config <FILE>`), and would be a second answer to "where does config
/// come from". `None` for an absent `type` is not a refusal — it is the default,
/// a child that inherits this topic's settings.
pub fn resolve_type_arg(
    type_name: Option<&str>,
    config: &AppConfig,
) -> Result<Option<SubagentType>, String> {
    match type_name {
        Some(name) => resolve_subagent_type(
            name,
            Some(config),
            jyc_utils::paths::config_dir().as_deref(),
        )
        .map(Some),
        None => Ok(None),
    }
}

/// Peel `--type=<name>` off `/spawn`'s arguments.
///
/// Shaped like `take_inherit_flag`, and for the same reason: `--type=x` is not a
/// topic name, so it has to leave the argument list before [`spawn_under`] tells
/// names from paths. `--type` with no value is a refusal rather than a topic
/// that happens to start with dashes.
fn take_type_flag(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut found: Option<String> = None;
    let mut rest = Vec::new();
    for arg in args {
        let trimmed = arg.trim();
        match trimmed.strip_prefix("--type=") {
            Some(value) if value.trim().is_empty() => {
                return Err("--type needs a name (--type=review)".to_string());
            }
            Some(value) => {
                if found.is_some() {
                    return Err("--type given twice".to_string());
                }
                found = Some(value.trim().to_string());
            }
            None if trimmed == "--type" => {
                return Err("--type needs a name (--type=review)".to_string());
            }
            None => rest.push(arg.clone()),
        }
    }
    Ok((found, rest))
}

/// Whether a role names a model of its own — `[ai]` with any of its three keys.
///
/// The same three keys answer two questions in [`apply_subagent_type`]: which
/// override files the role supersedes ([`superseded_overrides`]), and whether
/// the child still takes the parent's model ([`seed_start_model`]). A role that
/// names one starts on it, not on the parent's.
fn declares_a_model(config: &TopicConfig) -> bool {
    config
        .ai
        .as_ref()
        .is_some_and(|ai| ai.model.is_some() || ai.plan_model.is_some() || ai.build_model.is_some())
}

/// The runtime override files a resolved type supersedes.
///
/// Each one sits *above* a topic config in its resolution chain, so leaving it
/// behind would silently outrank the type's own value. Only the keys the type
/// sets itself are superseded: where it has no opinion the parent's runtime
/// choice stands, which is what makes a role with no `[ai]` inherit the
/// parent's model instead of falling back to the configured default.
fn superseded_overrides(config: &TopicConfig) -> Vec<&'static str> {
    let mut files = Vec::new();
    if config.mode.is_some() {
        files.push("mode-override");
    }
    if let Some(ai) = &config.ai {
        // The legacy file is mode-agnostic and still read ahead of the config,
        // so any model at all has to take it out; a per-mode file only holds a
        // value for the mode it names, and the chain reads the one whose mode it
        // is asked for — so `[ai].model` covers both, and a plan-only model only
        // has to take the plan file out.
        if declares_a_model(config) {
            files.push("model-override");
        }
        if ai.model.is_some() || ai.plan_model.is_some() {
            files.push("plan-model-override");
        }
        if ai.model.is_some() || ai.build_model.is_some() {
            files.push("build-model-override");
        }
    }
    // A skill toggle cancels a config-level exclusion of the same name
    // (`prompt.rs` keeps `on` entries out of the exclude list), so a type that
    // only excludes skills needs the parent's toggles gone just as much; the
    // same goes for a type that replaces the inherited MCP set outright.
    if config.skills.is_some() || config.disabled_skills.is_some() {
        files.push(crate::session_state::SKILL_OVERRIDE_FILE);
    }
    if config.mcps.is_some() || config.mcps_replace {
        files.push(crate::session_state::MCP_OVERRIDE_FILE);
    }
    files
}

/// Write a resolved `type` into the child's state dir.
///
/// Two steps, in this order:
/// 1. **The parent's runtime choices the type answers for itself go.**
///    `seed_state` copied the override files the parent happened to be running
///    with, and [`superseded_overrides`] names the ones whose key the type sets
///    itself. A key it leaves out keeps the parent's choice.
/// 2. **The type's settings are written** to `.jyc/config.toml` — a config
///    layer, so the child's own `/mode`, `/model` and `/skill` still win over it
///    afterwards (the type is where the child *starts*, not a cage) — plus the
///    name in `.jyc/subagent-type`, which nothing reads yet (the dashboard is
///    where it is headed) and which carries no behavior of its own.
fn apply_subagent_type(state_dir: &Path, subagent_type: &SubagentType) -> Result<()> {
    for file in superseded_overrides(&subagent_type.config) {
        let path = state_dir.join(file);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        }
    }

    let path = state_dir.join("config.toml");
    let body = toml::to_string_pretty(&subagent_type.config)
        .context("failed to serialize the subagent type as TOML")?;
    std::fs::write(&path, body).with_context(|| format!("failed to write {}", path.display()))?;

    let type_path = state_dir.join("subagent-type");
    std::fs::write(&type_path, format!("{}\n", subagent_type.name))
        .with_context(|| format!("failed to write {}", type_path.display()))?;
    Ok(())
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

    /// A role file's prose with its line wrapping collapsed, for asserting on a
    /// phrase: the assertion is about the instruction, and a role file that
    /// happens to break a sentence across two lines must not read as a missing
    /// one. (It did: a wrapped sentence turns the test red for nothing.)
    fn flat(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

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
    async fn parent_topic(topic: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(topic);
        let state = dir.join(".jyc");
        // The parent of a spawn is a live topic, so its state dir is
        // registered — a test that resolves the parent's model through
        // `resolve_state_dir` (which has no fallback by design, #825) needs
        // what production always has. `topic` is the caller's: the registry is
        // process-global and these tests run in parallel.
        jyc_types::state_dir::register(topic, &state);
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
            (
                session_state::TASKS_FILE,
                "{\"items\":[{\"id\":1,\"text\":\"the parent's plan\",\"status\":\"pending\"}]}",
            ),
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

    /// The `/spawn` side of a call: the fields `spawn_under` reads, off the
    /// context the production path would have built. `execute` takes the
    /// `--inherit` flag out of the arguments before this point, so the test
    /// that exercises it builds its [`SpawnRequest`] by hand the same way:
    /// `args` here is only ever a name and a path.
    fn spawn_request(ctx: &CommandContext) -> SpawnRequest<'_> {
        SpawnRequest {
            parent_topic: &ctx.topic_name,
            parent_topic_path: &ctx.topic_path,
            args: &ctx.args,
            inherit: false,
            share_topic_path: false,
            subagent_type: None,
            start_model: None,
        }
    }

    /// A type comes from either source: the config's `[subagents.<type>]` table,
    /// or an installed `<config dir>/subagents/<type>/config.toml`. Defined in
    /// both, they merge key by key with the config table winning.
    #[test]
    fn resolve_subagent_type_reads_both_sources() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("subagents/review");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "mode = \"build\"\nskills = [\"installed\"]\ndisabled_tools = [\"bash\"]\n",
        )
        .unwrap();
        let base = "[ai]\nenabled = true\n";

        // Installed only.
        let config = jyc_types::load_config_from_str(base).unwrap();
        let installed = resolve_subagent_type("review", Some(&config), Some(tmp.path())).unwrap();
        assert_eq!(installed.name, "review");
        assert_eq!(installed.config.mode.as_deref(), Some("build"));
        assert_eq!(
            installed.config.skills.as_deref(),
            Some(["installed".to_string()].as_slice())
        );

        // Config table only.
        let config = jyc_types::load_config_from_str(&format!(
            "{base}[subagents.analyst]\nmode = \"plan\"\n"
        ))
        .unwrap();
        let from_config =
            resolve_subagent_type("analyst", Some(&config), Some(tmp.path())).unwrap();
        assert_eq!(from_config.config.mode.as_deref(), Some("plan"));
        assert!(from_config.config.skills.is_none());

        // Both: per key, the config table wins; what only the installed file
        // sets survives.
        let config = jyc_types::load_config_from_str(&format!(
            "{base}[subagents.review]\nmode = \"plan\"\n"
        ))
        .unwrap();
        let merged = resolve_subagent_type("review", Some(&config), Some(tmp.path())).unwrap();
        assert_eq!(
            merged.config.mode.as_deref(),
            Some("plan"),
            "the running instance's own definition wins"
        );
        assert_eq!(
            merged.config.skills.as_deref(),
            Some(["installed".to_string()].as_slice())
        );
        assert_eq!(merged.config.disabled_tools.as_ref().map(Vec::len), Some(1));
    }

    /// A type nobody defines is refused, and the refusal names the ones that do
    /// exist — a typo should cost one turn, not a debugging session.
    #[test]
    fn unknown_subagent_type_lists_what_exists() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("subagents/review");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "mode = \"plan\"\n").unwrap();
        let config = jyc_types::load_config_from_str(
            "[ai]\nenabled = true\n[subagents.analyst]\nmode = \"plan\"\n",
        )
        .unwrap();

        let err = resolve_subagent_type("reveiw", Some(&config), Some(tmp.path())).unwrap_err();
        assert!(err.contains("unknown subagent type 'reveiw'"), "{err}");
        assert!(
            err.contains("analyst") && err.contains("review"),
            "both sources are offered: {err}"
        );

        // Nothing defined anywhere: the message says where a type would go.
        let empty = tempdir().unwrap();
        let bare = jyc_types::load_config_from_str("[ai]\nenabled = true\n").unwrap();
        let err = resolve_subagent_type("review", Some(&bare), Some(empty.path())).unwrap_err();
        assert!(err.contains("no types are defined"), "{err}");
        assert!(err.contains("subagents"), "{err}");
    }

    /// `mode` is checked here rather than tolerated downstream: an unknown mode
    /// resolves to build, which for a reviewer means "able to write files".
    #[test]
    fn subagent_type_mode_is_validated() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("subagents/review");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "mode = \"planning\"\n").unwrap();

        let config = jyc_types::load_config_from_str("[ai]\nenabled = true\n").unwrap();
        let err = resolve_subagent_type("review", Some(&config), Some(tmp.path())).unwrap_err();
        assert!(err.contains("is not a mode"), "{err}");

        std::fs::write(dir.join("config.toml"), "mode = \"build\"\n").unwrap();
        let ok = resolve_subagent_type("review", Some(&config), Some(tmp.path())).unwrap();
        assert_eq!(ok.config.mode.as_deref(), Some("build"));
    }

    /// The type names a directory under the config dir, and the name arrives
    /// from a model-authored tool argument — a traversal must not reach the
    /// lookup at all.
    #[test]
    fn subagent_type_names_are_guarded() {
        let config = jyc_types::load_config_from_str("[ai]\nenabled = true\n").unwrap();
        for bad in ["", "..", "../evil", "sub/type", ".hidden"] {
            let err = resolve_subagent_type(bad, Some(&config), None).unwrap_err();
            assert!(
                err.contains("cannot be a subagent type name"),
                "{bad:?} must be refused: {err}"
            );
        }
    }

    /// An installed type that does not parse is an error naming its file — the
    /// user asked for it by name, so silence would be worse than a refusal
    /// (unlike a topic config, whose absence is normal).
    #[test]
    fn broken_installed_type_is_an_error() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("subagents/review");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "not [valid toml").unwrap();

        let config = jyc_types::load_config_from_str("[ai]\nenabled = true\n").unwrap();
        let err = resolve_subagent_type("review", Some(&config), Some(tmp.path())).unwrap_err();
        assert!(err.contains("config.toml"), "{err}");
    }

    /// The watcher this repository ships — `subagents/ci-watch/config.toml` —
    /// loads the same way. It is checked for the same reason as the reviewer
    /// below, and for one more: the role starts on a model *reference*
    /// (`${ai.small_model}`), which only means the small model as long as the
    /// file keeps it a reference and the resolver keeps resolving it.
    #[test]
    fn the_repos_own_ci_watch_type_loads() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let config = jyc_types::load_config_from_str(
            "[ai]\nenabled = true\nsmall_model = \"provider/small\"\n",
        )
        .unwrap();

        let watch = resolve_subagent_type("ci-watch", Some(&config), Some(&repo)).unwrap();

        assert_eq!(watch.name, "ci-watch");
        assert_eq!(watch.config.mode.as_deref(), Some("plan"));
        assert_eq!(
            watch.config.ai.as_ref().and_then(|ai| ai.model.as_deref()),
            Some("${ai.small_model}"),
            "the watcher runs on the small model"
        );
        // …which is only true while that stays a *reference*: a renamed key or a
        // dropped name would leave the watcher on the configured model with this
        // test still green.
        assert_eq!(
            jyc_types::resolve_model_reference("${ai.small_model}", &config.ai).unwrap(),
            "provider/small",
            "the model reference still resolves to the channel's small model"
        );
        // `bash` stays: `gh` and `sleep` are what watching is. Nothing published
        // to a link either — the report is the only thing it sends back.
        assert_eq!(
            watch.config.disabled_tools.as_deref(),
            Some(
                ["write", "edit", "jyc_publish_file", "jyc_spawn_subagent"]
                    .map(str::to_string)
                    .as_slice()
            )
        );
        assert_eq!(
            watch.spawn.as_ref().and_then(|s| s.share_topic_path),
            Some(true),
            "the caller's directory is what identifies the run being watched"
        );
        assert!(
            watch
                .spawn
                .as_ref()
                .and_then(|s| s.inherit_context)
                .is_none(),
            "a task of its own does not need the caller's conversation"
        );
        assert!(
            watch
                .config
                .instructions
                .as_deref()
                .is_some_and(|i| flat(i).contains("Never poll without an end")),
            "the role carries its own budget and reporting orders"
        );
        assert!(
            watch
                .config
                .instructions
                .as_deref()
                .is_some_and(|i| flat(i).contains("do not interpret what you find")),
            "the watcher reports; the caller interprets"
        );
        // The report is where it went wrong once: a head the caller had mentioned
        // in a message, paired with a run id fetched in the same turn — two
        // sources, one sentence, and the green belonged to neither's head. The
        // rule that answers it has both halves, so both are pinned here.
        let orders = flat(watch.config.instructions.as_deref().unwrap());
        // Both halves of the send-back that went missing once — the orders made
        // it conditional on the caller asking for a reply, and named no outcome
        // for a run that does not exist yet — are pinned, because the sentence
        // that carried the condition was not: reverting either half would
        // otherwise be silent.
        assert!(
            orders.contains("whether or not the caller asked for a reply"),
            "the watcher reports back on its own initiative, not only when asked"
        );
        assert!(
            orders.contains("An empty list for a full, correct SHA is not a verdict"),
            "an empty run list is unread, not a result the watcher may report"
        );
        assert!(
            orders.contains("must be in a tool result from *this* turn"),
            "every fact the watcher states has to come from its own output"
        );
        assert!(
            orders.contains("no evidence at all about what ran"),
            "a head named in the caller's message is not evidence about the run"
        );
        // Two facts that call leaves open. Which workflow ran: the ref can carry
        // several per commit and `--limit 1` takes whichever GitHub lists first.
        // Which head is current: the newest run can belong to the older head
        // while the newer one's run does not exist yet.
        assert!(
            orders.contains("--workflow ci.yml"),
            "the watcher can pin the workflow where the ref carries several"
        );
        assert!(
            orders.contains("is not that head, say so"),
            "a run whose headSha is not the current head is reported as superseded"
        );
        // `--commit` matches the whole value: an abbreviated SHA is an empty
        // list, not an error, so the full one is what the role asks for.
        assert!(
            orders.contains("abbreviated one matches nothing"),
            "the run-against-head command is prescribed with the full SHA"
        );
        // A failure log is fetched through a filter inside the call: the result
        // cap bounds a finished turn, not the turn the log lands in, and a plain
        // byte tail of this repo's own failure misses the failing lines.
        assert!(
            orders.contains("--log-failed | grep -aE 'panicked|FAILED|error: ' | tail -n 5"),
            "the log comes back filtered and tailed inside the call, not whole"
        );
        // Small on purpose: a watcher keeps no tool-call notes and only a few
        // turns, so `gh` output cannot pile up in the context it polls with.
        let strategy = watch
            .config
            .context_strategy
            .as_ref()
            .expect("the watcher carries its own context strategy");
        assert_eq!(strategy.mode, jyc_types::ContextStrategy::SlidingWindow);
        assert_eq!(strategy.window, 3);
        assert_eq!(strategy.note_window, Some(0));
        assert_eq!(strategy.tool_result_cap, Some(512));
    }

    /// The role this repository ships — `subagents/review/config.toml`, the type
    /// the `dev-workflow` skill tells a spawn to name — loads through the same
    /// installed-type path a user's `~/.config/jyc/subagents/review/` does. It is
    /// checked here so the file cannot rot into a role nobody can spawn.
    #[test]
    fn the_repos_own_review_type_loads() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let config = jyc_types::load_config_from_str("[ai]\nenabled = true\n").unwrap();

        let review = resolve_subagent_type("review", Some(&config), Some(&repo)).unwrap();

        assert_eq!(review.name, "review");
        assert_eq!(review.config.mode.as_deref(), Some("plan"));
        assert_eq!(
            review.config.skills.as_deref(),
            Some(
                [
                    "pr-review",
                    "ponytail-review",
                    "coding-principles",
                    "ponytail"
                ]
                .map(str::to_string)
                .as_slice()
            )
        );
        assert_eq!(
            review.config.disabled_skills.as_deref(),
            Some(["dev-workflow".to_string()].as_slice()),
            "a reviewer must not delegate the review onward"
        );
        // `bash` stays: there is no read-only shell, and a review of uncommitted
        // work needs `git diff`. The file-editing tools are the ones that go.
        assert_eq!(
            review.config.disabled_tools.as_deref(),
            Some(
                ["write", "edit", "jyc_spawn_subagent"]
                    .map(str::to_string)
                    .as_slice()
            )
        );
        assert!(
            review
                .config
                .instructions
                .as_deref()
                .is_some_and(|i| flat(i).contains("Change no file")),
            "the role carries its own standing orders"
        );
        // The two keys only an installed type has. `description` reaches the
        // model through the spawn tool's own `type` argument; `[spawn]` is what
        // makes `jyc_spawn_subagent(type = "review")` alone borrow the caller's
        // directory. Asserting both here also catches a mistyped `[spawn]` key
        // (the table denies unknown fields, so it would refuse to load at all).
        assert!(
            review
                .description
                .as_deref()
                .is_some_and(|d| d.contains("push")),
            "the role says when to reach for it, not what tools it has"
        );
        assert_eq!(
            type_start_flags(Some(&review), None, None),
            (false, true),
            "the shipped reviewer declares the share in its own file"
        );
    }

    /// The third role this repository ships — `subagents/develop/config.toml`,
    /// the type the `delegated-dev` skill tells a parent to name — loads through
    /// the same installed-type path. It is the one role that has to keep every
    /// tool (it does the work *and* delegates its own review), so this checks the
    /// absences as much as the values: a `disabled_tools` or a `[spawn]` table
    /// creeping into the file would turn the developer into a role it cannot be.
    #[test]
    fn the_repos_own_develop_type_loads() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let config = jyc_types::load_config_from_str("[ai]\nenabled = true\n").unwrap();

        let develop = resolve_subagent_type("develop", Some(&config), Some(&repo)).unwrap();

        assert_eq!(develop.name, "develop");
        assert_eq!(
            develop.config.mode.as_deref(),
            Some("build"),
            "the developer edits files; plan mode would refuse the work it was spawned for"
        );
        assert_eq!(
            develop.config.skills.as_deref(),
            Some(["dev-workflow".to_string()].as_slice()),
            "the workflow guide — branch, gate tier, delegated review — is its whole allow-list"
        );
        // `write`, `edit`, `bash` and `jyc_spawn_subagent` are all needed here: no
        // read-only or no-delegation rule survives contact with this role.
        assert_eq!(
            develop.config.disabled_tools.as_deref(),
            None,
            "the developer keeps the tools it needs to do the work and delegate its own review"
        );
        // And no `[spawn]` table, because the defaults are what this role wants:
        // a directory of its own (the caller prepares it as a worktree — sharing
        // the parent's tree is the one thing that would break the whole flow) and
        // no inherited conversation.
        assert_eq!(
            type_start_flags(Some(&develop), None, None),
            (false, false),
            "the developer starts with a directory of its own and no inherited conversation"
        );
        assert!(
            develop
                .description
                .as_deref()
                .is_some_and(|d| d.contains("PR")),
            "the role says when to reach for it, not what tools it has"
        );
        assert!(
            develop
                .config
                .instructions
                .as_deref()
                .is_some_and(|i| flat(i).contains("never create or remove that checkout")),
            "the role carries its own standing orders"
        );
    }

    /// What a model chooses from: a role's own line, and a type a spawn would
    /// refuse shown as unusable instead of offered bare — same resolution as the
    /// spawn, so the menu cannot promise what the spawn declines.
    #[tokio::test]
    async fn the_type_catalog_lists_what_a_spawn_would_accept() {
        let tmp = tempfile::tempdir().unwrap();
        let config = jyc_types::load_config_from_str(
            "[ai]\nenabled = true\n[subagents.analyst]\nmode = \"plan\"\n",
        )
        .unwrap();
        for (name, body) in [
            ("review", "description = \"reviews a diff\"\n"),
            // A `[spawn]` typo: the table denies unknown fields, so this file
            // does not load — and a reviewer quietly running in a copy of the
            // tree is exactly what must not happen silently.
            ("broken", "[spawn]\nshare_topic_pth = true\n"),
            // A directory name the spawn rejects, so it must not be offered.
            (".hidden", "mode = \"plan\"\n"),
        ] {
            let dir = tmp.path().join("subagents").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("config.toml"), body).unwrap();
        }

        let lines = type_catalog(&config, Some(tmp.path()));
        assert_eq!(
            lines.len(),
            3,
            "the unnameable one is not offered: {lines:?}"
        );
        assert_eq!(
            lines[0], "analyst",
            "an inline type is listed by name alone"
        );
        assert!(
            lines[1].starts_with("broken — (unusable: "),
            "{:?}",
            lines[1]
        );
        // The mechanism, not just the prefix: the load-time scan of `[spawn]` is
        // what refuses this file, and its message names the key it refused.
        assert!(
            lines[1].contains("unknown key `spawn.share_topic_pth`"),
            "{:?}",
            lines[1]
        );
        assert!(
            !lines[1].contains('\n'),
            "a broken type costs one line in the schema, not a TOML dump"
        );
        assert_eq!(lines[2], "review — reviews a diff");
    }

    /// A type is resolved against the config the caller holds — for `/spawn` the
    /// one it was dispatched with, for the tool the service's live `ArcSwap` —
    /// which is the only way a type declared under `--config <FILE>` or added by
    /// a config reload is visible. Nothing in this path reads config from disk:
    /// `resolve_type_arg` is the single step both entry points run, and an
    /// absent `type` is not a refusal but the documented default.
    #[test]
    fn resolve_type_arg_uses_the_config_it_is_handed() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[ai]\nenabled = true\n\n[subagents.review]\nmode = \"plan\"\n\
             skills = [\"pr-review\"]\n",
        )
        .unwrap();
        // The config as an instance running with `--config <path>` would have it.
        let config = jyc_types::load_config(&path).unwrap();

        let review = resolve_type_arg(Some("review"), &config)
            .expect("a type in the live config resolves")
            .expect("Some for a named type");
        assert_eq!(review.name, "review");
        assert_eq!(review.config.mode.as_deref(), Some("plan"));
        assert_eq!(
            review.config.skills.as_deref(),
            Some(["pr-review".to_string()].as_slice())
        );

        // No `type` at all is the default, not an error.
        assert!(resolve_type_arg(None, &config).unwrap().is_none());
    }

    /// `--type=<name>` leaves the argument list before names and paths are told
    /// apart, and a malformed flag is a refusal rather than a topic named
    /// `--type`.
    #[test]
    fn take_type_flag_parses_and_refuses() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let (name, rest) = take_type_flag(&args(&["--type=review", "worker"])).unwrap();
        assert_eq!(name.as_deref(), Some("review"));
        assert_eq!(rest, args(&["worker"]));

        let (name, rest) = take_type_flag(&args(&["worker", "../out"])).unwrap();
        assert_eq!(name, None, "no flag: nothing to peel");
        assert_eq!(rest, args(&["worker", "../out"]));

        for broken in [args(&["--type"]), args(&["--type="]), args(&["--type=  "])] {
            let err = take_type_flag(&broken).unwrap_err();
            assert!(err.contains("--type needs a name"), "{broken:?}: {err}");
        }
        let err = take_type_flag(&args(&["--type=a", "--type=b"])).unwrap_err();
        assert!(err.contains("twice"), "{err}");
    }

    /// A role's *installed* file can state how it is spawned (`[spawn]`) and
    /// describe itself in one line; an argument passed at spawn time still wins,
    /// in both directions, and a spawn with no type keeps the built-in default.
    #[tokio::test]
    async fn an_installed_type_carries_its_own_spawn_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("subagents/review");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "mode = \"plan\"\ndescription = \"reviews a diff\"\n[spawn]\nshare_topic_path = true\n\
             inherit_context = false\n",
        )
        .unwrap();

        let ty = resolve_subagent_type("review", None, Some(tmp.path())).unwrap();
        assert_eq!(ty.description.as_deref(), Some("reviews a diff"));
        assert_eq!(ty.config.mode.as_deref(), Some("plan"));
        // Nothing passed: the role decides — in the parent's dir, no conversation.
        assert_eq!(
            type_start_flags(Some(&ty), None, None),
            (false, true),
            "the role's own [spawn] fills what the caller left unsaid"
        );
        // Said explicitly: the argument wins, including "off" over the role's "on".
        assert_eq!(
            type_start_flags(Some(&ty), Some(true), Some(false)),
            (true, false)
        );
        // No type at all: the built-in default, unchanged by this feature.
        assert_eq!(type_start_flags(None, None, None), (false, false));
    }

    /// The shipped reviewer's own default, end to end: a role that declares
    /// `[spawn] share_topic_path = true` puts the child in the *caller's*
    /// directory with no flag at the call site (a copy of the tree would review
    /// yesterday's version) — and a path argument, which contradicts that,
    /// refuses instead of being quietly dropped.
    #[tokio::test]
    async fn a_role_that_shares_the_parents_dir_spawns_into_it() {
        let (_tmp, dir) = parent_topic("t-a_role_that_shares_the_parents_dir_spawns_into_it").await;
        let parent_state = dir.join(".jyc");
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_dir = workspace.path().join("agents");
        let handler = SpawnCommandHandler::new(tm.clone());

        let types_root = tempdir().unwrap();
        let type_dir = types_root.path().join("subagents/review");
        std::fs::create_dir_all(&type_dir).unwrap();
        std::fs::write(
            type_dir.join("config.toml"),
            "mode = \"plan\"\n[spawn]\nshare_topic_path = true\n",
        )
        .unwrap();
        let config = jyc_types::load_config_from_str("[ai]\nenabled = true\n").unwrap();
        let subagent_type =
            resolve_subagent_type("review", Some(&config), Some(types_root.path())).unwrap();

        let parent = "t-a_role_that_shares_the_parents_dir_spawns_into_it";
        let name = "worker-a_role_that_shares_the_parents_dir_spawns_into_it";
        let (inherit, share) = type_start_flags(Some(&subagent_type), None, None);
        let args = vec![name.to_string()];
        let result = handler
            .spawn_under(
                SpawnRequest {
                    parent_topic: parent,
                    parent_topic_path: &dir,
                    args: &args,
                    inherit,
                    share_topic_path: share,
                    subagent_type: Some(&subagent_type),
                    start_model: None,
                },
                &parent_state,
                &agents_dir,
            )
            .await
            .unwrap();
        assert!(result.success, "{}", result.message);
        assert!(
            result.message.contains(&dir.display().to_string()),
            "the child works in the parent's dir: {}",
            result.message
        );
        jyc_types::access_grants::clear(parent);

        // A path argument asks for a destination the role has already decided
        // against: refusing says so, dropping it would silently disagree.
        let args = vec!["scratch".to_string(), "/tmp/scratch".to_string()];
        let refused = handler
            .spawn_under(
                SpawnRequest {
                    parent_topic: parent,
                    parent_topic_path: &dir,
                    args: &args,
                    inherit,
                    share_topic_path: share,
                    subagent_type: Some(&subagent_type),
                    start_model: None,
                },
                &parent_state,
                &agents_dir,
            )
            .await
            .unwrap();
        assert!(!refused.success);
        assert!(
            refused.message.contains("give a name, not a path"),
            "{}",
            refused.message
        );
        assert!(
            !agents_dir.join("scratch").exists(),
            "and nothing was created on the way to the refusal"
        );
    }

    /// A typed spawn hands the child the role, dropping only the parent runtime
    /// choices the role answers for itself — each of those outranks a topic
    /// config in its chain. The type's settings are written to
    /// `.jyc/config.toml`, where the child's own `/mode`, `/model` and `/skill`
    /// can still win over them, and the name is recorded for the dashboard.
    #[tokio::test]
    async fn spawn_with_a_type_replaces_the_parents_runtime_choices() {
        let (_tmp, dir) =
            parent_topic("t-spawn_with_a_type_replaces_the_parents_runtime_choices").await;
        let parent_state = dir.join(".jyc");
        // The parent is running with /mode, /model, /skill and /mcp set — the
        // state `seed_state` copies and a role must not inherit.
        for (name, body) in [
            ("plan-model-override", "provider/parent-plan"),
            (
                session_state::SKILL_OVERRIDE_FILE,
                "{\"on\":[\"alpha\"],\"off\":[]}",
            ),
            (session_state::MCP_OVERRIDE_FILE, "{\"on\":[],\"off\":[]}"),
        ] {
            tokio::fs::write(parent_state.join(name), body)
                .await
                .unwrap();
        }

        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_dir = workspace.path().join("agents");
        let handler = SpawnCommandHandler::new(tm.clone());

        let types_root = tempdir().unwrap();
        let type_dir = types_root.path().join("subagents/review");
        std::fs::create_dir_all(&type_dir).unwrap();
        tokio::fs::write(
            type_dir.join("config.toml"),
            "mode = \"plan\"\nskills = [\"pr-review\"]\ndisabled_skills = [\"dev-workflow\"]\n\
             disabled_tools = [\"write\", \"bash\"]\ninstructions = \"read-only review\"\n",
        )
        .await
        .unwrap();
        let config = jyc_types::load_config_from_str("[ai]\nenabled = true\n").unwrap();
        let subagent_type =
            resolve_subagent_type("review", Some(&config), Some(types_root.path())).unwrap();

        let args =
            vec!["worker-spawn_with_a_type_replaces_the_parents_runtime_choices".to_string()];
        let req = SpawnRequest {
            parent_topic: "t-spawn_with_a_type_replaces_the_parents_runtime_choices",
            parent_topic_path: &dir,
            args: &args,
            inherit: false,
            share_topic_path: false,
            subagent_type: Some(&subagent_type),
            start_model: None,
        };
        let result = handler
            .spawn_under(req, &parent_state, &agents_dir)
            .await
            .unwrap();
        assert!(result.success, "{}", result.message);
        assert!(
            result.message.contains("as type 'review'"),
            "{}",
            result.message
        );

        let dest = resolved(
            &agents_dir.join("worker-spawn_with_a_type_replaces_the_parents_runtime_choices"),
        );
        let state = resolve_state_dir(
            "worker-spawn_with_a_type_replaces_the_parents_runtime_choices",
            &dest,
        );
        // The role declares `mode` and `skills`, so those two files go; it says
        // nothing about models or MCPs, so the files carrying those choices stay
        // (which of them the chain reads for this child is the subject of the
        // test below).
        for gone in ["mode-override", session_state::SKILL_OVERRIDE_FILE] {
            assert!(
                !state.join(gone).exists(),
                "{gone} is a key the role sets itself"
            );
        }
        for kept in ["plan-model-override", session_state::MCP_OVERRIDE_FILE] {
            assert!(
                state.join(kept).exists(),
                "{kept} is a choice the role leaves to the parent"
            );
        }

        let cfg = jyc_types::load_topic_config(
            "worker-spawn_with_a_type_replaces_the_parents_runtime_choices",
            &dest,
        )
        .expect("the child starts with a topic config");
        assert_eq!(cfg.mode.as_deref(), Some("plan"));
        assert_eq!(
            cfg.skills.as_deref(),
            Some(["pr-review".to_string()].as_slice())
        );
        assert_eq!(
            cfg.disabled_skills.as_deref(),
            Some(["dev-workflow".to_string()].as_slice())
        );
        assert_eq!(cfg.disabled_tools.as_ref().map(Vec::len), Some(2));
        assert_eq!(cfg.instructions.as_deref(), Some("read-only review"));

        assert_eq!(
            tokio::fs::read_to_string(state.join("subagent-type"))
                .await
                .unwrap()
                .trim(),
            "review",
            "the dashboard reads the role from here"
        );
        // Everything a spawn hands over besides runtime choices still arrives —
        // except the parent's task list, which is the child's own to write.
        assert!(state.join("pattern").exists(), "pattern must be inherited");
        assert!(
            !state.join(session_state::TASKS_FILE).exists(),
            "a child plans its own work"
        );
    }

    /// The other half of the rule: a key the role says nothing about keeps the
    /// parent's runtime choice, and the mapping runs key by key rather than
    /// all-or-nothing.
    #[tokio::test]
    async fn a_type_supersedes_only_the_overrides_it_declares() {
        // (the role's config, the files that must go, the files that must stay)
        let cases: &[(&str, &[&str], &[&str])] = &[
            (
                // No `[ai]`: every model file the parent had stays, and the
                // chain reads the plan file for this child — its mode comes from
                // the type's config (`mode-override`, the file layer, is gone).
                "mode = \"plan\"\n",
                &["mode-override"],
                &[
                    "plan-model-override",
                    "build-model-override",
                    "model-override",
                    "skill-override.json",
                    "mcp-override.json",
                ],
            ),
            (
                // A plan model: the plan file and the mode-agnostic one go, so
                // nothing outranks the type's value in the mode it declares. The
                // build file stays — it is not read in plan mode, and a later
                // `/mode build` still inherits that side from the parent.
                "mode = \"plan\"\n[ai]\nplan_model = \"provider/role\"\n",
                &["mode-override", "plan-model-override", "model-override"],
                &[
                    "build-model-override",
                    "skill-override.json",
                    "mcp-override.json",
                ],
            ),
            (
                // `[ai].model` covers both modes, so all three files go.
                "[ai]\nmodel = \"provider/role\"\n",
                &[
                    "plan-model-override",
                    "build-model-override",
                    "model-override",
                ],
                &["mode-override", "skill-override.json"],
            ),
            (
                // No `mode`: the parent's `mode-override` stands, so this child
                // is in plan mode too and a build model leaves the plan file the
                // chain reads for it.
                "[ai]\nbuild_model = \"provider/role\"\n",
                &["build-model-override", "model-override"],
                &["plan-model-override", "mode-override"],
            ),
            (
                // `disabled_skills` alone still needs the parent's toggles gone:
                // an `on` entry cancels a config-level exclusion of that name.
                "disabled_skills = [\"danger\"]\n",
                &["skill-override.json"],
                &[
                    "mode-override",
                    "plan-model-override",
                    "build-model-override",
                    "model-override",
                    "mcp-override.json",
                ],
            ),
        ];

        for (role, gone, kept) in cases {
            let (_tmp, dir) =
                parent_topic("t-a_type_supersedes_only_the_overrides_it_declares").await;
            let parent_state = dir.join(".jyc");
            for name in [
                "plan-model-override",
                "build-model-override",
                "model-override",
                "skill-override.json",
                "mcp-override.json",
            ] {
                tokio::fs::write(parent_state.join(name), "provider/parent")
                    .await
                    .unwrap();
            }

            let workspace = tempdir().unwrap();
            let tm = make_topic_manager(workspace.path());
            let agents_dir = workspace.path().join("agents");
            let handler = SpawnCommandHandler::new(tm.clone());

            let types_root = tempdir().unwrap();
            let type_dir = types_root.path().join("subagents/role");
            std::fs::create_dir_all(&type_dir).unwrap();
            tokio::fs::write(type_dir.join("config.toml"), role)
                .await
                .unwrap();
            let config = jyc_types::load_config_from_str("[ai]\nenabled = true\n").unwrap();
            let subagent_type =
                resolve_subagent_type("role", Some(&config), Some(types_root.path())).unwrap();

            let args = vec!["worker-a_type_supersedes_only_the_overrides_it_declares".to_string()];
            let req = SpawnRequest {
                parent_topic: "t-a_type_supersedes_only_the_overrides_it_declares",
                parent_topic_path: &dir,
                args: &args,
                inherit: false,
                share_topic_path: false,
                subagent_type: Some(&subagent_type),
                start_model: None,
            };
            let result = handler
                .spawn_under(req, &parent_state, &agents_dir)
                .await
                .unwrap();
            assert!(result.success, "{}", result.message);

            let dest = resolved(
                &agents_dir.join("worker-a_type_supersedes_only_the_overrides_it_declares"),
            );
            let state = resolve_state_dir(
                "worker-a_type_supersedes_only_the_overrides_it_declares",
                &dest,
            );
            for name in *gone {
                assert!(
                    !state.join(name).exists(),
                    "role {role:?} declares that key, so {name} must go"
                );
            }
            for name in *kept {
                assert!(
                    state.join(name).exists(),
                    "role {role:?} has no opinion on {name}, so it stays"
                );
            }
        }
    }

    /// A child begins on the model its parent is running, unless the role names
    /// one of its own.
    ///
    /// Written to `model-override`, the mode-agnostic fallback: the chain reads it
    /// whenever the mode-specific file for the child's own mode is missing, so
    /// the child's mode naming cannot hide it — a mode-only role makes the child
    /// a plan-mode topic (the type's `mode` lands in its config, and the type
    /// expands by removing the `mode-override` that would have said plan). The
    /// last case is the one that made this necessary — a review-shaped role has
    /// no `[ai]`, and without the write it fell through to the configured
    /// default.
    #[tokio::test]
    async fn a_child_starts_on_the_parents_model_unless_the_role_names_one() {
        // (the role, the parent's model, what the child's state should hold)
        let cases: &[(Option<&str>, Option<&str>, Option<&str>)] = &[
            (None, Some("provider/parent"), Some("provider/parent")),
            (
                Some("mode = \"plan\"\n"),
                Some("provider/parent"),
                Some("provider/parent"),
            ),
            // A role with a model of its own starts on it.
            (
                Some("mode = \"plan\"\n[ai]\nplan_model = \"provider/role\"\n"),
                Some("provider/parent"),
                None,
            ),
            // Nothing resolved for the parent: nothing written.
            (None, None, None),
        ];

        for (role, parent_model, expected) in cases {
            let (_tmp, dir) =
                parent_topic("t-a_child_starts_on_the_parents_model_unless_the_role_names_one")
                    .await;
            let parent_state = dir.join(".jyc");
            let workspace = tempdir().unwrap();
            let tm = make_topic_manager(workspace.path());
            let agents_dir = workspace.path().join("agents");
            let handler = SpawnCommandHandler::new(tm.clone());

            let types_root = tempdir().unwrap();
            let subagent_type = role.map(|body| {
                let type_dir = types_root.path().join("subagents/role");
                std::fs::create_dir_all(&type_dir).unwrap();
                std::fs::write(type_dir.join("config.toml"), body).unwrap();
                let config = jyc_types::load_config_from_str("[ai]\nenabled = true\n").unwrap();
                resolve_subagent_type("role", Some(&config), Some(types_root.path())).unwrap()
            });

            let args = vec!["worker-snapshot".to_string()];
            let req = SpawnRequest {
                parent_topic: "t-a_child_starts_on_the_parents_model_unless_the_role_names_one",
                parent_topic_path: &dir,
                args: &args,
                inherit: false,
                share_topic_path: false,
                subagent_type: subagent_type.as_ref(),
                start_model: parent_model.map(str::to_string),
            };
            let result = handler
                .spawn_under(req, &parent_state, &agents_dir)
                .await
                .unwrap();
            assert!(result.success, "{}", result.message);

            let dest = resolved(&agents_dir.join("worker-snapshot"));
            let state = resolve_state_dir("worker-snapshot", &dest);
            let written = tokio::fs::read_to_string(state.join("model-override"))
                .await
                .ok()
                .map(|s| s.trim().to_string());
            assert_eq!(
                written.as_deref(),
                *expected,
                "role {role:?} with parent model {parent_model:?}"
            );
            if let Some(expected) = expected {
                // Not just the file: the chain the child then runs reads it,
                // with no config of its own to fall back on.
                assert_eq!(
                    session_state::resolve_effective_model(
                        "worker-snapshot",
                        &dest,
                        &AppConfig::default(),
                        "x"
                    )
                    .await
                    .as_deref(),
                    Some(*expected),
                    "the child's own chain must read the snapshot"
                );
            }
        }
    }

    /// The parent ran `/model` while it was in build mode, so its state carries
    /// a `build-model-override` its *plan* turn never reads — and the role
    /// declares only `mode`. The child must start on the parent's current model,
    /// not on that leftover file.
    #[tokio::test]
    async fn a_mode_only_role_does_not_inherit_the_parents_other_modes_model() {
        let (_tmp, dir) =
            parent_topic("t-a_mode_only_role_does_not_inherit_the_parents_other_modes_model").await;
        let parent_state = dir.join(".jyc");
        // The parent's current model comes from its topic config, so there is
        // no file to copy; its earlier build-mode choice sits in the
        // mode-specific file that could otherwise shadow the snapshot.
        tokio::fs::write(
            parent_state.join("config.toml"),
            "[ai]\nplan_model = \"provider/plan\"\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            parent_state.join("build-model-override"),
            "provider/stale-build\n",
        )
        .await
        .unwrap();

        let config = AppConfig::default();
        // What `/spawn` resolves for the parent — its own mode's model, not the
        // build-mode choice `/model` left behind a mode switch ago. The parent
        // name is this test's own, so the registry lookup is deterministic.
        let parent_model = session_state::resolve_effective_model(
            "t-a_mode_only_role_does_not_inherit_the_parents_other_modes_model",
            &dir,
            &config,
            "x",
        )
        .await;
        assert_eq!(parent_model.as_deref(), Some("provider/plan"));

        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_dir = workspace.path().join("agents");
        let handler = SpawnCommandHandler::new(tm.clone());
        let types_root = tempdir().unwrap();
        let type_dir = types_root.path().join("subagents/role");
        std::fs::create_dir_all(&type_dir).unwrap();
        std::fs::write(type_dir.join("config.toml"), "mode = \"plan\"\n").unwrap();
        let type_config = jyc_types::load_config_from_str("[ai]\nenabled = true\n").unwrap();
        let subagent_type =
            resolve_subagent_type("role", Some(&type_config), Some(types_root.path())).unwrap();

        // A child name of its own: the state-dir registry is process-global, so
        // a name another test spawns in parallel would resolve to *its* dir.
        let args = vec!["mode-only-child".to_string()];
        let req = SpawnRequest {
            parent_topic: "t-a_mode_only_role_does_not_inherit_the_parents_other_modes_model",
            parent_topic_path: &dir,
            args: &args,
            inherit: false,
            share_topic_path: false,
            subagent_type: Some(&subagent_type),
            start_model: parent_model.clone(),
        };
        let result = handler
            .spawn_under(req, &parent_state, &agents_dir)
            .await
            .unwrap();
        assert!(result.success, "{}", result.message);

        let dest = resolved(&agents_dir.join("mode-only-child"));
        let child_state = resolve_state_dir("mode-only-child", &dest);
        assert_eq!(
            tokio::fs::read_to_string(child_state.join("build-model-override"))
                .await
                .unwrap()
                .trim(),
            "provider/stale-build",
            "the seeded file is still there — the point is that the chain must not read it"
        );
        assert_eq!(
            session_state::resolve_effective_model("mode-only-child", &dest, &config, "x")
                .await
                .as_deref(),
            Some("provider/plan"),
            "the child runs the parent's current model, not its other mode's"
        );
    }

    /// No argument: a fresh dir under the agents root, the parent's *settings* in
    /// it, and none of the workspace content — a spawn hands over state, not files.
    #[tokio::test]
    async fn spawn_defaults_to_a_fresh_dir_under_the_agents_dir() {
        let (_tmp, dir) =
            parent_topic("t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir").await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_dir = workspace.path().join("agents");
        let handler = SpawnCommandHandler::new(tm.clone());

        let ctx = context(
            "t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir",
            &dir,
            "websocket",
            &[],
        );
        let result = handler
            .spawn_under(spawn_request(&ctx), &dir.join(".jyc"), &agents_dir)
            .await
            .unwrap();
        assert!(result.success, "{}", result.message);

        // The handler registers the canonical path, so the asserts compare
        // against it too.
        let dest =
            resolved(&agents_dir.join("t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir-2"));
        assert!(dest.is_dir(), "the default destination is created for it");
        assert_eq!(
            tm.topic_path("t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir-2")
                .await
                .unwrap(),
            dest,
            "the spawn works in its own dir, not the parent's"
        );
        assert!(
            !dest.join("README.md").exists() && !dest.join("src").exists(),
            "a spawn hands over the conversation, not the workspace"
        );
        assert!(dir.join("README.md").exists(), "the parent keeps its files");

        let state = resolve_state_dir(
            "t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir-2",
            &dest,
        );
        assert_eq!(
            state,
            dest.join(".jyc"),
            "in the default shape the state lives inside the topic's own dir"
        );
        assert_eq!(
            tokio::fs::read_to_string(state.join("spawned-from"))
                .await
                .unwrap(),
            "t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir"
        );
        for keep in ["pattern", "mode-override"] {
            assert!(state.join(keep).exists(), "{keep} must be inherited");
        }
        assert!(
            !state.join(session_state::TASKS_FILE).exists(),
            "a child plans its own work: the parent's list stays behind"
        );
        // The default is a child with a head of its own: settings yes, the
        // parent's train of thought no. `--inherit` is the opt-out.
        for drop in [
            "agent-session.json",
            "activity.jsonl",
            "notes.md",
            "agent-context.json",
            "chat_history_2026-09-24.jsonl",
        ] {
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
        let grants = jyc_types::access_grants::grants_for(
            "t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir",
        );
        assert_eq!(grants.len(), 1, "one spawn, one grant: {grants:?}");
        assert_eq!(grants[0].path, dest);
        assert!(
            grants[0].write,
            "the parent seeds its spawn through this grant"
        );
        assert_eq!(
            subagents_of("t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir"),
            vec![(
                "t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir-2".to_string(),
                dest.clone()
            )]
        );

        // Closing the spawn removes the emptied shell with its state — the
        // close-side half of the default shape — and hands the access back.
        tm.close_topic("t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir-2")
            .await
            .unwrap();
        assert!(
            !dest.exists(),
            "an emptied spawn dir goes away with its state"
        );
        assert!(
            subagents_of("t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir").is_empty(),
            "closing the spawn revokes the parent's access"
        );

        jyc_types::state_dir::unregister("t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir-2");
        jyc_types::access_grants::clear("t-spawn_defaults_to_a_fresh_dir_under_the_agents_dir");
    }

    /// `--inherit` is the opt-out of that default, and it is a flag rather than
    /// a name: the topic comes from the argument after it, and the parent's
    /// conversation and transcript come along.
    #[tokio::test]
    async fn spawn_inherit_copies_the_conversation() {
        let (_tmp, dir) = parent_topic("t-spawn_inherit_copies_the_conversation").await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_dir = workspace.path().join("agents");
        let handler = SpawnCommandHandler::new(tm.clone());

        let ctx = context(
            "inherit-parent",
            &dir,
            "websocket",
            &["--inherit", "with-context"],
        );
        // What `execute` does before it builds the request: the flag comes off
        // the arguments, so it can never be mistaken for a name.
        let (inherit, args) = take_inherit_flag(&ctx.args);
        let req = SpawnRequest {
            parent_topic: &ctx.topic_name,
            parent_topic_path: &ctx.topic_path,
            args: &args,
            inherit,
            share_topic_path: false,
            subagent_type: None,
            start_model: None,
        };
        let result = handler
            .spawn_under(req, &dir.join(".jyc"), &agents_dir)
            .await
            .unwrap();
        assert!(result.success, "{}", result.message);

        let dest = resolved(&agents_dir.join("with-context"));
        assert_eq!(
            tm.topic_path("with-context").await.unwrap(),
            dest,
            "'--inherit' must not be read as the name"
        );
        let state = resolve_state_dir("with-context", &dest);
        for kept in [
            "agent-context.json",
            "chat_history_2026-09-24.jsonl",
            "mode-override",
        ] {
            assert!(state.join(kept).exists(), "{kept} must be inherited");
        }
        assert!(
            !state.join(session_state::TASKS_FILE).exists(),
            "even `--inherit` leaves the parent's task list behind"
        );
        assert!(
            result.message.contains("conversation included"),
            "the receipt says what it took: {}",
            result.message
        );

        jyc_types::state_dir::unregister("with-context");
        jyc_types::access_grants::clear("inherit-parent");
    }

    /// A spawn's dir that gained files is user property by then: `/close`
    /// deletes the state and keeps the dir with whatever is in it.
    #[tokio::test]
    async fn close_keeps_a_spawn_dir_that_gained_files() {
        let (_tmp, dir) = parent_topic("t-close_keeps_a_spawn_dir_that_gained_files").await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_dir = workspace.path().join("agents");
        let handler = SpawnCommandHandler::new(tm.clone());

        let ctx = context(
            "t-close_keeps_a_spawn_dir_that_gained_files",
            &dir,
            "websocket",
            &["filed-shell"],
        );
        let result = handler
            .spawn_under(spawn_request(&ctx), &dir.join(".jyc"), &agents_dir)
            .await
            .unwrap();
        assert!(result.success, "{}", result.message);
        let dest = resolved(&agents_dir.join("filed-shell"));
        tokio::fs::write(dest.join("user-note.txt"), "keep me")
            .await
            .unwrap();

        tm.close_topic("filed-shell").await.unwrap();
        assert!(dest.join("user-note.txt").exists(), "the user's file stays");
        assert!(!dest.join(".jyc").exists(), "the state is deleted");
        tokio::fs::remove_dir_all(&dest).await.unwrap();

        jyc_types::state_dir::unregister("filed-shell");
        jyc_types::access_grants::clear("t-close_keeps_a_spawn_dir_that_gained_files");
    }

    /// The reason `/spawn` exists: continue the conversation where the files
    /// already are — a non-empty directory is a destination, not a refusal.
    #[tokio::test]
    async fn spawn_works_in_an_existing_dir_and_touches_nothing_there() {
        let (_tmp, dir) =
            parent_topic("t-spawn_works_in_an_existing_dir_and_touches_nothing_there").await;
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
        let agents_dir = workspace.path().join("agents");
        tm.close_topic("checkout").await.unwrap();
        assert!(
            checkout.join("src/main.rs").exists(),
            "the checkout is untouched"
        );
        assert!(
            !agents_dir.join("checkout").join(".jyc").exists(),
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
        let (_tmp, dir) = parent_topic("t-spawn_resolves_a_relative_path_against_this_topic").await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let handler = SpawnCommandHandler::new(tm.clone());
        let expected = dir.parent().unwrap().join("test");

        let ctx = context(
            "t-spawn_resolves_a_relative_path_against_this_topic",
            &dir,
            "websocket",
            &["../test"],
        );
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
            subagents_of("t-spawn_resolves_a_relative_path_against_this_topic"),
            vec![("test".to_string(), resolved(&expected))],
            "a relative-path spawn is a subagent of its parent"
        );

        jyc_types::state_dir::unregister("test");
        jyc_types::access_grants::clear("t-spawn_resolves_a_relative_path_against_this_topic");
    }

    /// Look-once, refuse-before-creating: none of these may leave a directory,
    /// a topic, or a touched file behind.
    #[tokio::test]
    async fn spawn_refuses_destinations_and_names_it_must_not_take() {
        let (_tmp, dir) =
            parent_topic("t-spawn_refuses_destinations_and_names_it_must_not_take").await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_dir = workspace.path().join("agents");
        let handler = SpawnCommandHandler::new(tm.clone());

        let other = workspace.path().join("other-dir");
        tokio::fs::create_dir_all(&other).await.unwrap();
        tokio::fs::write(other.join("keep.txt"), "mine")
            .await
            .unwrap();
        tm.set_topic_path("other", other.clone()).await.unwrap();
        let a_file = workspace.path().join("file.txt");
        tokio::fs::write(&a_file, "x").await.unwrap();
        tokio::fs::create_dir_all(agents_dir.join("taken"))
            .await
            .unwrap();

        // Reaching the source dir takes an explicit path: `Path::file_name`
        // drops `.`, so `"./."` alone would name itself `src-topic` instead.
        for (args, expected) in [
            (
                vec!["t-spawn_refuses_destinations_and_names_it_must_not_take"],
                "already in",
            ),
            (vec!["taken"], "already exists"),
            (vec![".hidden"], "must not start with '.'"),
            // A mistyped flag is not a name: left allowed it would create a topic
            // called `--inherit=false`. (A bare `~` is *not* here: `/spawn` takes
            // it as the destination it is, `$HOME`, and derives the name from it;
            // it is refused where it is a name — the tool's `name`, `/fork`.)
            (vec!["--inherit=false"], "must not start with '-'"),
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
            let ctx = context(
                "t-spawn_refuses_destinations_and_names_it_must_not_take",
                &dir,
                "websocket",
                &args,
            );
            let result = handler
                .spawn_under(spawn_request(&ctx), &dir.join(".jyc"), &agents_dir)
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
        assert!(!agents_dir.join("spawn-x").exists());
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

        let ctx = context(
            "t-spawn_refuses_a_non_websocket_channel",
            tmp.path(),
            "email",
            &["whatever"],
        );
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

    /// The tool's entry runs `/spawn`'s implementation: the same inheritance
    /// (the parent's settings, `mode-override` included), the same breadcrumb,
    /// the same grant — the guards and the name resolution are what it adds.
    /// `inherit_context` is its one addition to the seeding, so this is the
    /// call that asks for the conversation as well.
    #[tokio::test]
    async fn subagent_spawn_inherits_state_and_grants_access() {
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_dir = workspace.path().join("agents");
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

        let name = spawn_subagent_under(
            &tm,
            parent,
            &parent_dir,
            Some("worker-subagent_spawn_inherits_state_and_grants_access"),
            true,
            false,
            None,
            None,
            &agents_dir,
        )
        .await
        .unwrap();
        assert_eq!(
            name,
            "worker-subagent_spawn_inherits_state_and_grants_access"
        );

        let dest = agents_dir.join("worker-subagent_spawn_inherits_state_and_grants_access");
        assert_eq!(
            std::fs::read_to_string(dest.join(".jyc").join("spawned-from")).unwrap(),
            parent
        );
        assert!(
            tm.topic_path("worker-subagent_spawn_inherits_state_and_grants_access")
                .await
                .is_some()
        );
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
        assert_eq!(
            subagents_of(parent),
            vec![(
                "worker-subagent_spawn_inherits_state_and_grants_access".to_string(),
                dest
            )]
        );

        jyc_types::access_grants::clear(parent);
        jyc_types::state_dir::unregister("worker-subagent_spawn_inherits_state_and_grants_access");
    }

    /// `share_topic_path`: the tool triggers `/spawn`'s co-pinning instead of a
    /// dir of its own — the `/fork` shape, and the only kind of subagent that
    /// can review work that is not committed yet. The shared dir is the
    /// parent's throughout: the child's state stays its own under the agents
    /// root, and closing the child deletes that state and leaves the dir (and
    /// the parent's in-dir `.jyc`) exactly as it was.
    #[tokio::test]
    /// Every topic name in the tests below is this test's own: `state_dir` and
    /// `access_grants` are process-wide and keyed by topic name alone, and
    /// `cargo test` runs this binary's tests in parallel — so a name another
    /// test already uses is a collision waiting to happen, not a detail.
    async fn shared_subagent_works_in_the_parents_dir() {
        let (_tmp, dir) = parent_topic("t-shared_subagent_works_in_the_parents_dir").await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_dir = workspace.path().join("agents");
        let parent = "sh-parent";

        let name = spawn_subagent_under(
            &tm,
            parent,
            &dir,
            Some("shared-review"),
            false,
            true,
            None,
            None,
            &agents_dir,
        )
        .await
        .unwrap();
        assert_eq!(name, "shared-review");

        let shared = resolved(&dir);
        assert_eq!(
            tm.topic_path("shared-review").await.unwrap(),
            shared,
            "the child works where the parent works"
        );
        let state = agents_dir.join("shared-review").join(".jyc");
        assert_eq!(
            state_dir_of("shared-review", &shared),
            state,
            "its state is still its own, under the agents root, never in the shared dir"
        );
        assert_eq!(
            std::fs::read_to_string(state.join("spawned-from")).unwrap(),
            parent
        );
        assert!(
            state.join("pattern").exists() && !state.join("agent-context.json").exists(),
            "settings copied, the parent's conversation left behind"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("README.md")).unwrap(),
            "hello",
            "nothing is copied into the directory the child shares"
        );
        assert_eq!(
            jyc_types::access_grants::grants_for(parent)
                .iter()
                .map(|g| g.path.clone())
                .collect::<Vec<_>>(),
            vec![shared.clone()],
            "the parent is granted the dir it already works in"
        );
        assert_eq!(
            subagents_of(parent),
            vec![("shared-review".to_string(), shared.clone())]
        );

        // Closing the child: its state goes, the shared dir and its files do
        // not — including the parent's own in-dir `.jyc`.
        tm.close_topic("shared-review").await.unwrap();
        assert!(dir.join("README.md").exists() && dir.join("src/main.rs").exists());
        assert!(dir.join(".jyc").join("pattern").exists());
        assert!(!state.exists(), "the child's own state goes with the child");
        assert!(subagents_of(parent).is_empty());

        jyc_types::state_dir::unregister("shared-review");
        jyc_types::access_grants::clear(parent);
    }

    /// A closed parent closes its subagents: they were spawned to serve its
    /// work, the grant is what makes them reachable at all, and a shared one
    /// would otherwise be left pointing into a directory the parent's own close
    /// may have taken.
    #[tokio::test]
    async fn closing_a_parent_closes_its_subagents() {
        let (_tmp, dir) = parent_topic("t-closing_a_parent_closes_its_subagents").await;
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_dir = workspace.path().join("agents");
        let parent = "cascade-parent";

        for (name, share) in [("cascade-review", true), ("cascade-work", false)] {
            spawn_subagent_under(
                &tm,
                parent,
                &dir,
                Some(name),
                false,
                share,
                None,
                None,
                &agents_dir,
            )
            .await
            .unwrap();
        }
        let states = ["cascade-review", "cascade-work"].map(|n| agents_dir.join(n).join(".jyc"));
        assert!(states.iter().all(|s| s.is_dir()));
        assert_eq!(subagents_of(parent).len(), 2);

        tm.close_topic(parent).await.unwrap();

        assert!(
            states.iter().all(|s| !s.exists()),
            "both subagents' state goes with the parent"
        );
        assert!(
            subagents_of(parent).is_empty(),
            "and the access the parent held on them"
        );
        assert!(
            dir.join("README.md").exists() && dir.join(".jyc").join("pattern").exists(),
            "the parent's own dir and state are untouched by its close"
        );

        for name in ["cascade-review", "cascade-work"] {
            jyc_types::state_dir::unregister(name);
        }
        jyc_types::access_grants::clear(parent);
    }

    /// A name is not an argument here: `spawn_under` tells names from paths by
    /// shape, so `../evil` left unchecked would be resolved as a path — off the
    /// agents root, outside everything this module assumes about a subagent.
    #[tokio::test]
    async fn subagent_spawn_refuses_a_path_shaped_name() {
        let workspace = tempdir().unwrap();
        let tm = make_topic_manager(workspace.path());
        let agents_dir = workspace.path().join("agents");
        let parent = "sa-parent-name";
        let parent_dir = workspace.path().join("parent");
        std::fs::create_dir_all(parent_dir.join(".jyc")).unwrap();
        jyc_types::access_grants::clear(parent);

        let err = spawn_subagent_under(
            &tm,
            parent,
            &parent_dir,
            Some("../evil"),
            false,
            false,
            None,
            None,
            &agents_dir,
        )
        .await
        .unwrap_err();
        assert!(err.contains("cannot be a subagent name"), "{err}");
        assert!(
            !workspace.path().join("evil").exists() && !agents_dir.exists(),
            "a refusal creates nothing anywhere"
        );
    }

    /// A spawn's grant is what lists a child, by the name it carries — and a
    /// `/grant`, which names nobody, still lists one whose *directory* is named
    /// after it (the recovery path after a restart). A stale child (its state,
    /// and with it the breadcrumb, gone) and a directory another topic spawned
    /// are both left out.
    #[test]
    fn subagents_of_skips_stale_and_foreign_grants() {
        let workspace = tempdir().unwrap();
        let parent = "sa-parent-list";
        jyc_types::access_grants::clear(parent);

        let mine = workspace.path().join("mine");
        std::fs::create_dir_all(mine.join(".jyc")).unwrap();
        std::fs::write(mine.join(".jyc").join("spawned-from"), parent).unwrap();
        // The same child, after a restart dropped the spawn's own grant and the
        // user re-granted the directory it works in.
        let regranted = workspace.path().join("regranted");
        std::fs::create_dir_all(regranted.join(".jyc")).unwrap();
        std::fs::write(regranted.join(".jyc").join("spawned-from"), parent).unwrap();
        let theirs = workspace.path().join("theirs");
        std::fs::create_dir_all(theirs.join(".jyc")).unwrap();
        std::fs::write(theirs.join(".jyc").join("spawned-from"), "someone-else").unwrap();
        let gone = workspace.path().join("gone"); // never had a state dir

        jyc_types::access_grants::grant_subagent(parent, "mine", mine.clone());
        jyc_types::access_grants::grant(parent, regranted.clone(), true);
        jyc_types::access_grants::grant_subagent(parent, "theirs", theirs.clone());
        jyc_types::access_grants::grant_subagent(parent, "gone", gone.clone());

        assert_eq!(
            subagents_of(parent),
            vec![
                ("mine".to_string(), mine.clone()),
                ("regranted".to_string(), regranted),
            ]
        );

        forget_subagent("mine", &mine);
        assert_eq!(
            subagents_of(parent)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            vec!["regranted"],
            "revoking a child leaves a re-granted sibling listed"
        );
        assert_eq!(format_subagent_names(&[]), "");
        jyc_types::access_grants::clear(parent);
    }
}
