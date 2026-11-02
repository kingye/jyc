//! Tool system for the agent.
//!
//! Defines the `Tool` trait and built-in tool implementations.

pub mod builtin;
pub mod mcp_auth;
pub mod mcp_bridge;
pub mod mcp_client;
pub mod mcp_schema;
pub mod registry;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::types::{ImageSource, ToolDefinition};
use jyc_core::topic_manager::TopicManagers;
use jyc_types::InboundMessage;
use jyc_types::channel::OutboundAdapter;
use jyc_types::config::HookEvent;
use jyc_types::state_dir::resolve_state_dir;
use jyc_utils::hooks::{HookCtx, HookOutcome, HookSet};

/// Everything the reply delivery path needs to send a reply synchronously
/// through the channel's outbound adapter, bypassing the
/// `reply.md`/`reply-sent.flag` file relay. Injected by the agent loop from
/// the message currently being processed; `None` in contexts without a live
/// inbound message (unit tests, sub-agents), where delivery falls back to
/// the file relay.
#[derive(Debug, Clone)]
pub struct ReplyTarget {
    /// The inbound message being replied to.
    pub original: InboundMessage,
    /// Message directory name, used by the outbound adapter for chat-log
    /// storage.
    pub message_dir: String,
}

/// Outcome of a [`deliver_reply`] call.
#[derive(Debug)]
pub struct ReplyDelivery {
    /// Sent synchronously through the channel's outbound adapter.
    pub direct: bool,
    /// Channel message id (direct deliveries only).
    pub message_id: Option<String>,
    /// Chat sequence number the adapter wrote into the chat log row for this
    /// reply (`0` when the channel logs no row). The caller publishes its
    /// `ReplySent` event with it so the live row and the logged row agree.
    pub chat_seq: u64,
}

/// Deliver `text` as a user-visible reply — the single delivery path shared
/// by the final assistant message, pre-question narration, and the
/// cycle-boundary heartbeat.
///
/// With a live reply target the reply is sent synchronously through the
/// pre-warmed outbound adapter, gated by `reply_send` hooks; the caller
/// publishes the `ReplySent` dashboard event when `direct` comes back true.
/// On send failure — or when no target is live (tests, sub-agents) — the
/// reply is queued through the `.jyc/reply.md` + `.jyc/reply-sent.flag`
/// file relay for the worker/watcher to deliver.
pub async fn deliver_reply(
    ctx: &ToolContext<'_>,
    hooks: Option<&HookSet>,
    topic_name: &str,
    text: &str,
) -> Result<ReplyDelivery> {
    if let Some((outbound, target)) = ctx.live_delivery() {
        if let Some(hooks) = hooks {
            let mut hctx = HookCtx {
                topic: topic_name.to_string(),
                cwd: ctx.working_dir.display().to_string(),
                channel: ctx.current_channel.clone(),
                reply_text: Some(text.to_string()),
                ..Default::default()
            };
            hctx.message_content = None; // this is an output, not an intake
            if let HookOutcome::Block(reason) = hooks
                .run(HookEvent::ReplySend, Some(topic_name), &hctx)
                .await
            {
                anyhow::bail!("reply suppressed by reply_send hook: {reason}");
            }
        }
        match outbound
            .send_reply(
                &target.original,
                text,
                ctx.working_dir,
                &target.message_dir,
                None,
            )
            .await
        {
            Ok(result) => {
                tracing::info!(
                    message_len = text.len(),
                    message_id = %result.message_id,
                    "Reply delivered synchronously via outbound adapter"
                );
                return Ok(ReplyDelivery {
                    direct: true,
                    message_id: Some(result.message_id),
                    chat_seq: result.chat_seq,
                });
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Direct reply delivery failed, falling back to file relay"
                );
            }
        }
    }

    // File relay: queue for the background delivery watcher / post-loop
    // worker delivery.
    //
    // A closed topic gets nothing: `/close` deletes the state dir, and a
    // reply queued into a fresh copy of it would bring back the skeleton the
    // topic list reads as an open topic (there is nowhere left to deliver to
    // either — the topic is gone).
    if jyc_types::state_dir::is_closed(topic_name) {
        tracing::info!("Topic closed; reply not queued for delivery");
        return Ok(ReplyDelivery {
            direct: false,
            message_id: None,
            chat_seq: 0,
        });
    }
    let state_dir = resolve_state_dir(topic_name, ctx.working_dir);
    tokio::fs::create_dir_all(&state_dir).await.ok();
    tokio::fs::write(state_dir.join("reply.md"), text)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to write reply.md: {e}"))?;
    let signal = serde_json::json!({
        "sent_at": chrono::Utc::now().to_rfc3339(),
        "message_len": text.len(),
        "attachment_count": 0,
        "attachments": Vec::<String>::new(),
    });
    tokio::fs::write(
        state_dir.join("reply-sent.flag"),
        serde_json::to_string_pretty(&signal)?,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to write signal file: {e}"))?;
    tracing::info!(message_len = text.len(), "Reply signal written");
    Ok(ReplyDelivery {
        direct: false,
        message_id: None,
        chat_seq: 0,
    })
}

/// Shared outbound adapters map keyed by channel name.
/// Used by `jyc_send_message` to support the `channel` parameter for
/// cross-channel proactive messaging.
pub type OutboundsMap = Arc<tokio::sync::Mutex<HashMap<String, Arc<dyn OutboundAdapter>>>>;

/// Context provided to tools during execution.
pub struct ToolContext<'a> {
    /// Working directory for the tool.
    pub working_dir: &'a Path,
    /// Additional absolute paths the agent may legitimately read from
    /// outside `working_dir` (currently: a configured absolute
    /// `[attachments.inbound].save_path`). Tools that enforce a path
    /// boundary (e.g. `read_image`) accept paths under any of these
    /// roots in addition to `working_dir`.
    pub additional_read_roots: Vec<PathBuf>,
    /// Additional absolute paths the agent may write to outside
    /// `working_dir` (e.g. configured per-pattern `write` paths).
    /// Write tools (`write`, `edit`, `bash`) accept paths under any of
    /// these roots in addition to `working_dir`.
    pub additional_write_roots: Vec<PathBuf>,
    /// Side-channel for tools (e.g. `read_image`) that need to inject
    /// additional content blocks into the *next* user turn alongside
    /// the textual tool result. The agent loop drains this after each
    /// batch of tool calls and emits a synthetic user turn carrying the
    /// images. `Mutex<Vec<_>>` to allow tools with `&self` execution to
    /// push without requiring `&mut ToolContext`.
    pub pending_images: Mutex<Vec<ImageSource>>,
    /// Side-channel for tools that must put text in front of the user *during*
    /// the turn (currently: `ask_user` on a channel with no question box). The
    /// agent loop drains this after each tool batch and sends it through the
    /// same reply path as the final message, so the text reaches the channel
    /// the user is actually reading instead of relying on the model to quote
    /// it. `Mutex<Vec<_>>` to match `pending_images`.
    pub pending_texts: Mutex<Vec<String>>,
    /// Whether the active pattern allows image handling. Mirrors the
    /// `inject_inbound_images` flag that `build_user_blocks` checks in
    /// `service.rs`. When `false`, `read_image` should refuse to process
    /// images even if a `VisionClient` is configured, ensuring consistent
    /// behavior between auto-injection and tool-driven image analysis.
    /// Default: `false` (opt-in for safety).
    pub pattern_inject_images: bool,
    /// Optional outbound adapter for proactive messaging tools (e.g.
    /// `jyc_send_message`). Injected by `JycAgentService` when building
    /// the tool registry. `None` when the agent runs in contexts without
    /// a pre-warmed outbound adapter.
    pub outbound: Option<Arc<dyn OutboundAdapter>>,
    /// Cross-channel topic managers keyed by channel name.
    /// Used by `jyc_send_to_topic` tool to inject messages into topics
    /// in other channels. `None` when running in contexts without
    /// cross-channel communication (e.g. unit tests).
    pub topic_managers: Option<Arc<TopicManagers>>,
    /// Current channel name, for tools that need source context (e.g.
    /// `jyc_send_to_topic` sets `source_channel` metadata from this).
    pub current_channel: Option<String>,
    /// Current topic name, for tools that need source context (e.g.
    /// `jyc_send_to_topic` sets `source_topic` metadata from this).
    pub current_topic: Option<String>,
    /// Cross-channel outbound adapters keyed by channel name.
    /// Used by `jyc_send_message` to support the `channel` parameter for
    /// sending proactive messages through any channel's outbound adapter.
    /// `None` when running in contexts without cross-channel support
    /// (e.g. unit tests).
    pub outbounds: Option<OutboundsMap>,
    /// The current turn's messages, injected by the agent loop at the
    /// start of the current tool batch. The turns before it are not in
    /// memory — the agent loop reads only the window the context
    /// strategy asks for, and a tool that needs older turns reads them
    /// back from the topic's `agent-context.jsonl` journal itself (see
    /// `context_browse`). Empty when not injected (unit tests, synthetic
    /// progress replies).
    pub raw_context: Vec<serde_json::Value>,
    /// The assistant's own text for the current tool batch, when that batch
    /// asks the user a question. Injected by the agent loop next to
    /// `raw_context`: a channel with a question box has no other way to show
    /// the user what the question is about, and the same text delivered as a
    /// separate message can go missing. `None` for every other batch.
    pub question_context: Option<String>,
    /// Synchronous delivery target for `jyc_reply_message`. When present
    /// together with `outbound`, the reply tool delivers through the channel
    /// adapter immediately and its result reflects the real delivery outcome.
    pub reply_target: Option<ReplyTarget>,
    /// The topics this turn sent messages to, as `(channel, topic)` pairs.
    ///
    /// `jyc_send_to_topic` records each successful send here, so the service
    /// can tell a turn that already answered a `require_reply` request from
    /// one that did not — the difference between "the requester has its
    /// answer" and "the requester is still waiting".
    pub outbound_targets: Mutex<Vec<(String, String)>>,
    /// The paths this turn was refused, as `(resolved, write)` pairs.
    ///
    /// Recorded by the two boundary checks on refusal, read by
    /// `request_access`. It narrows what the model may ask about: a request
    /// must name a path it was actually refused (or an ancestor of one), so a
    /// model that hit the boundary on `/srv/data/x` cannot turn around and ask
    /// the user to grant `~/.ssh`. The grant itself is still the server's to
    /// make — this only bounds the question.
    ///
    /// `Mutex<Vec<_>>` for the same reason as `pending_images`: the checks take
    /// `&self`, and `Mutex` is poisoned only by an unwind, which cannot happen
    /// under `panic = "abort"`.
    pub denied_paths: Mutex<Vec<(PathBuf, bool)>>,
}

/// Whether `canonical` lies inside the system temp dir `tmp`.
///
/// `tmp` is canonicalized first: on macOS `env::temp_dir()` yields
/// `/var/folders/...` while an already-resolved argument arrives as
/// `/private/var/folders/...`, and a raw comparison would miss the match.
///
/// A `tmp` with no parent (i.e. the filesystem root) is rejected: every
/// absolute path is inside `/`, so honoring it would disable the boundary
/// entirely. `env::temp_dir()` returns `$TMPDIR` unvalidated on Unix, so
/// this is reachable via a stray `export TMPDIR=/` in the service
/// environment rather than being merely theoretical.
///
/// Takes `tmp` as a parameter rather than reading the environment so the
/// degenerate cases stay testable without mutating process state.
fn is_within_temp_dir(canonical: &Path, tmp: &Path) -> bool {
    let tmp_canonical = tmp.canonicalize().unwrap_or_else(|_| tmp.to_path_buf());
    tmp_canonical.parent().is_some() && canonical.starts_with(&tmp_canonical)
}

/// `path` with `.` and `..` folded and its deepest existing ancestor resolved to
/// the real path.
///
/// Folding alone leaves a path under a symlinked prefix (macOS `$TMPDIR`, whose
/// `/var/…` spelling is `/private/var/…` in the filesystem) looking like it sits
/// outside `working_dir`; canonicalizing alone fails for a path that does not
/// exist yet — a file the command is about to create. Resolving the existing
/// prefix and re-appending the rest keeps both sides of every boundary
/// comparison in one namespace, which is why `write`/`edit` of a *new* file
/// still lands inside a working directory reached through a symlink.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut folded = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                folded.pop();
            }
            other => folded.push(other.as_os_str()),
        }
    }

    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = folded.as_path();
    loop {
        if let Ok(canonical) = current.canonicalize() {
            let mut resolved = canonical;
            for name in tail.iter().rev() {
                resolved.push(name);
            }
            return resolved;
        }
        let (Some(name), Some(parent)) = (current.file_name(), current.parent()) else {
            return folded;
        };
        tail.push(name.to_os_string());
        current = parent;
    }
}

impl<'a> ToolContext<'a> {
    /// Construct a context with no extra roots and an empty pending-images queue.
    pub fn new(working_dir: &'a Path) -> Self {
        Self {
            working_dir,
            additional_read_roots: Vec::new(),
            additional_write_roots: Vec::new(),
            pending_images: Mutex::new(Vec::new()),
            pending_texts: Mutex::new(Vec::new()),
            pattern_inject_images: false,
            outbound: None,
            topic_managers: None,
            current_channel: None,
            current_topic: None,
            outbounds: None,
            raw_context: Vec::new(),
            question_context: None,
            reply_target: None,
            outbound_targets: Mutex::new(Vec::new()),
            denied_paths: Mutex::new(Vec::new()),
        }
    }

    /// Construct a context with extra absolute read roots.
    pub fn with_roots(working_dir: &'a Path, additional_read_roots: Vec<PathBuf>) -> Self {
        Self {
            working_dir,
            additional_read_roots,
            additional_write_roots: Vec::new(),
            pending_images: Mutex::new(Vec::new()),
            pending_texts: Mutex::new(Vec::new()),
            pattern_inject_images: false,
            outbound: None,
            topic_managers: None,
            current_channel: None,
            current_topic: None,
            outbounds: None,
            raw_context: Vec::new(),
            question_context: None,
            reply_target: None,
            outbound_targets: Mutex::new(Vec::new()),
            denied_paths: Mutex::new(Vec::new()),
        }
    }

    /// Drain and return any pending image sources accumulated during the
    /// current tool-execution batch. Called by the agent loop after the
    /// batch completes.
    pub fn take_pending_images(&self) -> Vec<ImageSource> {
        std::mem::take(&mut *self.pending_images.lock().expect("pending_images poisoned"))
    }

    /// Drain and return any texts tools queued for the user during the current
    /// tool batch. Called by the agent loop, which delivers them through the
    /// ordinary reply path before continuing the turn.
    pub fn take_pending_texts(&self) -> Vec<String> {
        std::mem::take(&mut *self.pending_texts.lock().expect("pending_texts poisoned"))
    }

    /// The live delivery target, when this turn can put a message in front of
    /// the user right now: an outbound adapter plus the message being answered.
    /// `deliver_reply` sends through it, and a tool deciding "can I deliver, or
    /// is the model's reply text the only route?" must ask here — the two
    /// answers drifting apart is how a mid-turn text lands in `reply.md`, where
    /// the turn's final reply overwrites it.
    pub(crate) fn live_delivery(&self) -> Option<(&Arc<dyn OutboundAdapter>, &ReplyTarget)> {
        Some((self.outbound.as_ref()?, self.reply_target.as_ref()?))
    }

    /// Check that `resolved` is within `working_dir` (or one of the
    /// `additional_read_roots`). Returns `Ok(())` when the path is inside
    /// the boundary, or an `Err` with a user-facing access-denied message.
    ///
    /// The path is judged by what it resolves to: symlinks are followed before
    /// the comparison, so a link inside the working directory that points out of
    /// it is refused at its target, and `..` cannot climb out. A checkout the
    /// operator wants reachable through a link outside the working directory is
    /// granted with `access.read`.
    ///
    /// **Exemptions**: the null device and paths under
    /// `std::env::temp_dir()` are always accepted. The null device carries
    /// nothing across the boundary (writes are discarded, reads are empty) and
    /// is named by nearly every `cmd >/dev/null 2>&1`. The temp dir is scratch
    /// space tools need without per-pattern `access` config; note this makes the
    /// shared system temp dir readable. A degenerate temp dir (the filesystem
    /// root) is ignored — see [`is_within_temp_dir`].
    pub fn check_path_boundary(
        &self,
        display_path: &str,
        resolved: &Path,
    ) -> std::result::Result<(), String> {
        match self.read_boundary_verdict(resolved) {
            Ok(()) => Ok(()),
            Err(canonical) => {
                self.record_denial(&canonical, false);
                Err(Self::boundary_denial(display_path, &canonical, false))
            }
        }
    }

    /// Where `resolved` falls relative to the read boundary, without recording
    /// anything.
    ///
    /// Split from [`Self::check_path_boundary`] so `request_access` can ask
    /// "would this be allowed?" without its own probe counting as a refusal —
    /// otherwise a model could turn any path into a granted ask simply by
    /// asking twice.
    ///
    /// Applies the null-device and temp-dir exemptions described on
    /// [`Self::check_path_boundary`].
    fn read_boundary_verdict(&self, resolved: &Path) -> std::result::Result<(), PathBuf> {
        // A path that does not exist yet (`write` of a new file) cannot be
        // canonicalized whole. Falling back to its raw spelling compares it
        // against a canonicalized `working_dir`, which refuses a new file inside
        // a working directory whose own spelling runs through a symlink —
        // `normalize` resolves the existing prefix instead.
        let canonical = normalize(resolved);
        let working_canonical = normalize(self.working_dir);

        // The null device is always in-boundary: writes are discarded and reads
        // are empty, so nothing crosses the boundary through it, and nearly
        // every `cmd >/dev/null 2>&1` names it.
        if canonical == Path::new("/dev/null") {
            return Ok(());
        }

        if canonical.starts_with(&working_canonical) {
            return Ok(());
        }

        // Also check additional_read_roots (e.g. configured attachment
        // save paths outside working_dir).
        for root in &self.additional_read_roots {
            if canonical.starts_with(normalize(root)) {
                return Ok(());
            }
        }

        // System temp dir is always in-boundary: tools need scratch space.
        if is_within_temp_dir(&canonical, &std::env::temp_dir()) {
            return Ok(());
        }

        Err(canonical)
    }

    /// Where `resolved` falls relative to the write boundary, without
    /// recording anything.
    ///
    /// The write roots are checked after the read ones, on the same canonical
    /// path, so the refusal names the path the read check already resolved.
    fn write_boundary_verdict(&self, resolved: &Path) -> std::result::Result<(), PathBuf> {
        match self.read_boundary_verdict(resolved) {
            Ok(()) => Ok(()),
            Err(canonical) => {
                for root in &self.additional_write_roots {
                    if canonical.starts_with(normalize(root)) {
                        return Ok(());
                    }
                }
                Err(canonical)
            }
        }
    }

    /// Whether `resolved` is inside the boundary this turn would enforce for
    /// `write`, asked without recording a refusal.
    ///
    /// The read-only probe `request_access` uses: a path already reachable
    /// needs no grant, and asking would spend the user's attention on a
    /// decision with no effect.
    pub fn is_within_boundary(&self, resolved: &Path, write: bool) -> bool {
        if write {
            self.write_boundary_verdict(resolved).is_ok()
        } else {
            self.read_boundary_verdict(resolved).is_ok()
        }
    }

    /// Remember a refusal, so `request_access` can tell a path the model was
    /// actually refused from one it made up.
    ///
    /// Called only on the refusal path: an accepted path needs no entry, and a
    /// repeated denial of the same path records once.
    fn record_denial(&self, canonical: &Path, write: bool) {
        let mut denied = self.denied_paths.lock().unwrap();
        if !denied
            .iter()
            .any(|(path, w)| path == canonical && *w == write)
        {
            denied.push((canonical.to_path_buf(), write));
        }
    }

    /// Whether this turn was refused `resolved` (or a path under it) for the
    /// access `write` asks for.
    ///
    /// The relation is `denied.starts_with(canonical)`: the requested path must
    /// contain the refused one, so a request is either the refused path itself
    /// or a directory above it. Asking for a *child* is not covered — a model
    /// refused `/srv/data` may ask for `/srv/data` but not for a directory it
    /// never touched, and asking for a sibling never was refused either.
    ///
    /// `write` requires a write refusal: read access was not what the tool
    /// that failed needed, so a read denial does not justify asking for more.
    pub fn was_denied(&self, resolved: &Path, write: bool) -> bool {
        // The probe must name the path the way `record_denial` stored it, which
        // is `normalize` — the same resolution both boundary verdicts apply.
        // Canonicalizing whole and falling back to the raw spelling compares
        // across two namespaces: a refusal of a not-yet-existing file under a
        // symlinked prefix is stored resolved (`/private/var/…`) and probed raw
        // (`/var/…`), so the ask is told it was never refused.
        let canonical = normalize(resolved);
        self.denied_paths
            .lock()
            .unwrap()
            .iter()
            .any(|(denied, denied_write)| {
                (!write || *denied_write) && denied.starts_with(&canonical)
            })
    }

    /// The refusal both boundary checks return.
    ///
    /// It names the path twice on purpose: the caller's own spelling, so the
    /// user recognises what was asked, and the resolved absolute path, because
    /// that is the form a grant has to quote — `display_path` may be relative
    /// and a relative path cannot be granted. It also names the one tool that
    /// can ask for the grant: an error the model cannot act on is where the
    /// work stops.
    fn boundary_denial(display_path: &str, canonical: &Path, write: bool) -> String {
        let access = if write { "write" } else { "read" };
        format!(
            "Access denied: path '{}' is outside the working directory (resolved: {}). \
             {access} access is not granted for it. If the user might allow it, call \
             load_extended_tools and then request_access with that absolute path.",
            display_path,
            canonical.display()
        )
    }

    /// Check that `resolved` is within `working_dir`, `additional_read_roots`,
    /// or `additional_write_roots`. Write paths imply read access.
    ///
    /// Inherits the null-device and temp-dir exemptions from
    /// [`Self::check_path_boundary`], which this delegates to first.
    ///
    /// Used by write/edit/bash tools to enforce the write boundary.
    pub fn check_write_boundary(
        &self,
        display_path: &str,
        resolved: &Path,
    ) -> std::result::Result<(), String> {
        match self.write_boundary_verdict(resolved) {
            Ok(()) => Ok(()),
            Err(canonical) => {
                self.record_denial(&canonical, true);
                Err(Self::boundary_denial(display_path, &canonical, true))
            }
        }
    }
}

/// Trait for tools that can be invoked by the agent.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (used in LLM tool_use).
    fn name(&self) -> &str;

    /// Which MCP server provided this tool, or `None` for a built-in tool or
    /// the in-process bridge.
    ///
    /// This is load-bearing, not descriptive metadata. Advertisement is decided
    /// from it: `Some(server)` hides the tool until that server is expanded
    /// through `load_mcp_schema`, and `None` leaves it to the extended group
    /// instead. A name therefore has exactly one owner — the live tool that
    /// holds it — so an implementation that gets this wrong misclassifies its
    /// tool silently rather than failing:
    ///
    /// - an MCP-backed tool that leaves this at `None` is treated as a
    ///   built-in, which lets the extended group claim it and keeps it out of
    ///   the MCP index the model reads;
    /// - a built-in that returns `Some(..)` is advertised eagerly, defeating
    ///   the schema budget its group exists to enforce.
    ///
    /// `Some` must name the server exactly as the registry's groups do. Return
    /// a borrow of a field: this is called once per tool on every model
    /// request, so it must stay cheap and must neither allocate nor lock.
    fn source(&self) -> Option<&str> {
        None
    }

    /// Tool description (shown to LLM).
    fn description(&self) -> &str;

    /// JSON Schema for the tool's input parameters.
    fn input_schema(&self) -> Value;

    /// Execute the tool with the given input.
    async fn execute(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput>;

    /// Convert to a ToolDefinition for the LLM.
    fn to_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

/// Output from a tool execution.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// The tool's text output.
    pub content: String,
    /// Whether the execution resulted in an error.
    pub is_error: bool,
    /// End the turn as soon as this output is recorded: the tool has already
    /// put everything the user needs in front of them, so another model call
    /// (and whatever it would say) is one message too many. Set by `ask_user`,
    /// which ends its turn the moment the questions are handed over.
    pub ends_turn: bool,
}

impl ToolOutput {
    /// Create a successful output.
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            ends_turn: false,
        }
    }

    /// Create an error output.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            ends_turn: false,
        }
    }

    /// Mark this output as ending the turn — see [`Self::ends_turn`].
    pub fn ending_turn(mut self) -> Self {
        self.ends_turn = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The system temp dir is in-boundary for both read and write, while an
    /// unrelated absolute path is still denied. Uses `/etc/passwd` as the
    /// negative case because a `TempDir` working dir now sits inside an
    /// always-allowed root.
    #[test]
    fn temp_dir_is_within_both_boundaries() {
        let working = tempfile::tempdir().expect("create working dir");
        let ctx = ToolContext::new(working.path());

        let scratch = std::env::temp_dir().join("jyc-boundary-test.txt");
        assert!(ctx.check_path_boundary("scratch", &scratch).is_ok());
        assert!(ctx.check_write_boundary("scratch", &scratch).is_ok());

        let outside = Path::new("/etc/passwd");
        assert!(ctx.check_path_boundary("passwd", outside).is_err());
        assert!(ctx.check_write_boundary("passwd", outside).is_err());
    }

    /// The null device is in-boundary for both access kinds: writes are
    /// discarded and reads are empty, so it leads nowhere else, and every
    /// `cmd >/dev/null 2>&1` names it.
    #[test]
    fn the_null_device_is_within_both_boundaries() {
        let working = tempfile::tempdir().expect("create working dir");
        let ctx = ToolContext::new(working.path());

        assert!(
            ctx.check_path_boundary("null", Path::new("/dev/null"))
                .is_ok(),
            "reading the null device is always empty"
        );
        assert!(
            ctx.check_write_boundary("null", Path::new("/dev/null"))
                .is_ok(),
            "writing the null device discards"
        );
    }

    /// A symlink is judged at its target. An exemption for any symlinked
    /// ancestor let a link the agent made in its own directory (`ln -s /etc e;
    /// cat e/shadow`) carry a read straight out of the boundary, which is the
    /// same escape the quoted-path scan used to miss.
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_judged_at_its_target() {
        let working = tempfile::tempdir().expect("create working dir");
        let ctx = ToolContext::new(working.path());

        let inside = working.path().join("target.txt");
        std::fs::write(&inside, "inside").expect("write inside target");
        let inner_link = working.path().join("inner");
        std::os::unix::fs::symlink(&inside, &inner_link).expect("create inner symlink");
        assert!(
            ctx.check_path_boundary("inner", &inner_link).is_ok(),
            "a link that stays inside the working directory is fine"
        );

        let outer_link = working.path().join("outer");
        std::os::unix::fs::symlink("/etc", &outer_link).expect("create outer symlink");
        let escape = outer_link.join("passwd");
        assert!(
            ctx.check_path_boundary("outer/passwd", &escape).is_err(),
            "a link out of the working directory is refused at its target"
        );

        // The other direction: the *spelling* is outside, the target inside.
        // Judging the spelling would refuse a path the command can only reach
        // into the working directory with.
        let elsewhere = tempfile::tempdir().expect("create outside dir");
        let inward_link = elsewhere.path().join("inward");
        std::os::unix::fs::symlink(&inside, &inward_link).expect("create inward symlink");
        assert!(
            ctx.check_path_boundary("inward", &inward_link).is_ok(),
            "a link from outside that lands inside names an in-boundary file"
        );
    }

    /// The refusal is recorded under the same name the probe asks with.
    ///
    /// A path that does not exist yet is resolved by [`normalize`] on both
    /// sides, so a refusal reached through a symlinked prefix stays askable.
    /// Canonicalizing whole is not the same question — it fails for a file that
    /// is not there and falls back to the *spelling*, which for a link names a
    /// different path than the one the refusal was recorded against. The model
    /// would then be told it was never refused the path it was just refused.
    #[cfg(unix)]
    #[test]
    fn a_refusal_through_a_symlink_stays_askable() {
        let working = tempfile::tempdir().expect("create working dir");
        let ctx = ToolContext::new(working.path());

        // Points out of the boundary, so the tail is refused; the tail does not
        // exist, so only `normalize` can name the path the refusal is about.
        let link = working.path().join("outward");
        std::os::unix::fs::symlink("/etc", &link).expect("create outward symlink");
        let refused = link.join("jyc-not-here");

        assert!(
            ctx.check_path_boundary("outward/jyc-not-here", &refused)
                .is_err()
        );
        assert!(
            ctx.was_denied(&refused, false),
            "the refused spelling must resolve to the name it was recorded under"
        );
        assert!(
            ctx.was_denied(Path::new("/etc/jyc-not-here"), false),
            "and so must the resolved spelling, which `/grant` quotes"
        );
    }

    /// A refusal is remembered, and it bounds what `request_access` may ask
    /// about: the refused path itself and directories above it, never an
    /// unrelated path and never a read refusal widened into a write ask.
    #[test]
    fn a_refusal_is_recorded_and_narrows_the_ask() {
        let working = tempfile::tempdir().expect("create working dir");
        let ctx = ToolContext::new(working.path());
        let refused = Path::new("/etc/passwd");

        assert!(ctx.check_path_boundary("passwd", refused).is_err());

        assert!(ctx.was_denied(refused, false), "the refused path itself");
        assert!(
            ctx.was_denied(Path::new("/etc"), false),
            "a directory above the refused path"
        );
        assert!(
            !ctx.was_denied(Path::new("/home/nobody/.ssh/id_rsa"), false),
            "a path this turn never touched must not become askable"
        );
        assert!(
            !ctx.was_denied(refused, true),
            "a read refusal does not justify asking for write"
        );
    }

    /// Asking "would this be allowed?" must not count as the refusal that
    /// justifies asking. If the probe recorded, a model could make any path
    /// askable by calling `request_access` twice.
    #[test]
    fn a_probe_records_no_refusal() {
        let working = tempfile::tempdir().expect("create working dir");
        let ctx = ToolContext::new(working.path());
        let outside = Path::new("/etc/passwd");

        assert!(!ctx.is_within_boundary(outside, false));
        assert!(!ctx.is_within_boundary(outside, true));
        assert!(
            !ctx.was_denied(outside, false),
            "is_within_boundary is a query, not a refusal"
        );
    }

    /// A write refusal is recorded as a write one and also answers a read
    /// question, because write access implies read.
    #[test]
    fn a_write_refusal_answers_a_read_question_too() {
        let working = tempfile::tempdir().expect("create working dir");
        let ctx = ToolContext::new(working.path());
        let outside = Path::new("/etc/passwd");

        assert!(ctx.check_write_boundary("passwd", outside).is_err());
        assert!(ctx.was_denied(outside, true));
        assert!(ctx.was_denied(outside, false));
    }

    /// A temp dir of `/` must not be honored: every absolute path is inside
    /// the root, so accepting it would disable the boundary entirely.
    /// `env::temp_dir()` returns `$TMPDIR` unvalidated on Unix, so this is a
    /// reachable misconfiguration.
    #[test]
    fn root_temp_dir_is_rejected() {
        let root = Path::new("/");
        assert!(!is_within_temp_dir(Path::new("/etc/passwd"), root));
        assert!(!is_within_temp_dir(
            Path::new("/home/someone/.ssh/id_rsa"),
            root
        ));

        // A normal temp dir still matches, and only on whole components.
        let tmp = Path::new("/tmp");
        assert!(is_within_temp_dir(Path::new("/tmp/scratch.txt"), tmp));
        assert!(!is_within_temp_dir(Path::new("/tmpfoo/escape.txt"), tmp));
    }

    /// A new file under a working directory whose own spelling runs through a
    /// symlink must still land inside it.
    ///
    /// `write`/`edit` pass the raw joined path, and a file that does not exist
    /// yet cannot be canonicalized. Comparing that raw spelling against a
    /// *canonicalized* `working_dir` loses on any layout where the working dir
    /// is reached through a link — macOS `$TMPDIR` (`/var/…` is really
    /// `/private/var/…`) or a symlinked agents dir — and refuses the write.
    /// [`normalize`] resolves the existing prefix and re-appends the tail, so
    /// both sides of the comparison share one spelling.
    #[cfg(unix)]
    #[test]
    fn a_new_file_under_a_symlinked_working_dir_resolves_inside() {
        let base = tempfile::tempdir().expect("create base");
        let real = base.path().join("real");
        std::fs::create_dir(&real).expect("create real");
        let link = base.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("create link");

        let new_file = link.join("not-yet-there");
        assert!(!new_file.exists(), "the fixture must not exist yet");

        assert_eq!(
            normalize(&new_file),
            normalize(&real).join("not-yet-there"),
            "the missing tail is re-appended to the resolved prefix"
        );
        // The comparison the boundary actually makes.
        assert!(
            normalize(&new_file).starts_with(normalize(&link)),
            "writing a new file inside the working dir is inside the boundary"
        );
        // The raw spelling is what used to lose that comparison.
        assert!(!new_file.starts_with(normalize(&real)));

        let ctx = ToolContext::new(&link);
        assert!(
            ctx.check_write_boundary("not-yet-there", &new_file).is_ok(),
            "and the write is allowed, not just computed correctly"
        );
    }

    /// `..` is folded before the existing prefix is resolved, so a path that
    /// walks up and back down lands on the file it names.
    #[test]
    fn normalize_folds_parent_components() {
        assert_eq!(
            normalize(Path::new("/etc/../etc/passwd")),
            normalize(Path::new("/etc/passwd"))
        );
        // A `..` the filesystem must resolve because the segment is a symlink.
        // `/etc/../etc` is `/etc` either way, so this needs a link to differ —
        // the point here is only that folding does not run off the root.
        assert_eq!(normalize(Path::new("/../..")), Path::new("/"));
    }
}
