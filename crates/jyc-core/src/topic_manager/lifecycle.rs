//! `TopicManager` impl block: lifecycle.rs methods.
//!
//! Extracted from the monolithic `topic_manager.rs`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Per-topic queue stats.
use super::TopicManager;

impl TopicManager {
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        {
            // Cancel all per-topic tokens
            let mut cancels = self.topic_cancels.lock().await;
            for (_, token) in cancels.drain() {
                token.cancel();
            }
        }
        {
            let mut queues = self.topic_queues.lock().await;
            queues.clear();
        }
        {
            // Clear event buses
            let mut event_buses = self.event_buses.lock().await;
            event_buses.clear();
        }
        let mut handles = self.worker_handles.lock().await;
        for handle in handles.drain(..) {
            let _ = handle.await;
        }
        tracing::info!("All workers shut down");
    }

    /// Cancel the AI processing for a topic without deleting its directory.
    ///
    /// Triggers the per-topic cancellation token so the agent loop breaks
    /// at the next iteration. The worker exits, and the next message will
    /// spawn a new worker automatically. Topic directory and queue are preserved.
    ///
    /// Returns `true` if an active token was found and cancelled, `false` if
    /// the topic had no running worker (callers can report this honestly
    /// instead of claiming success).
    pub async fn cancel_topic(&self, topic_name: &str) -> bool {
        let cancels = self.topic_cancels.lock().await;
        if let Some(token) = cancels.get(topic_name) {
            token.cancel();
            tracing::info!(topic = %topic_name, "Topic AI processing cancelled via cancel_topic");
            true
        } else {
            tracing::warn!(topic = %topic_name, "cancel_topic: no cancellation token found (topic may not be processing)");
            false
        }
    }

    /// Close and delete a topic's directory.
    ///
    /// This is channel-agnostic, and so is *what* it deletes: `/close`,
    /// `/close <name>`, `/close all`, a close cascade and an upstream close
    /// event off a channel all end a topic the same way.
    ///
    /// The topic's **home** goes with its state — the directory the
    /// registration hangs off: the home itself when the topic works in
    /// `<home>/.data`, the `<agents dir>/<topic>/` shell a pin left behind, the
    /// default dir a `/spawn` gets (not one a `-d` or a pin handed it).
    /// Contents and all: the working directory, the `.attachments` in it,
    /// whatever else was written there. Two dirs are never taken:
    ///
    /// - one **outside the agents tree** — a checkout a `-d` or a pin points
    ///   at is the user's, and only its state is deleted;
    /// - one **another topic works in or lives inside** (a `-s` child, a fork,
    ///   the topics of the pipe that owns it).
    ///
    /// Nothing *above* the home goes either: a pipe's own dir
    /// (`<agents dir>/<pipe>/`) stays when one of its topics closes. It is the
    /// home only of the pipe topic named after it (`<pipe>` closing `<pipe>`),
    /// and that one takes it with it — the boundary is the home, not the pipe.
    ///
    /// The registration itself stays: a reopened topic reuses the same state dir
    /// name instead of ever falling back to `<topic_dir>/.jyc` (#825).
    /// Unregistered topics keep the legacy behavior: the whole directory (with
    /// its in-dir `.jyc`) is removed. In-memory state is cleaned up in both
    /// cases.
    ///
    /// The topic's subagents close with it
    /// ([`Self::subagent_closure`]).
    pub async fn close_topic(&self, topic_name: &str) -> Result<()> {
        let agents_dir = self.resolve_agents_dir();
        self.close_topic_under(topic_name, &agents_dir).await
    }

    /// [`Self::close_topic`] with an explicit agents root — the same
    /// testable-core pattern as `purge_topic_dir_under` / `auto_close_topic_under`.
    ///
    /// The subagents go first, and through [`Self::close_one_topic_under`]:
    /// this method *is* the cascade, so a child must not run it again.
    pub(crate) async fn close_topic_under(
        &self,
        topic_name: &str,
        agents_dir: &Path,
    ) -> Result<()> {
        for child in self.subagent_closure(topic_name) {
            if let Err(e) = self.close_one_topic_under(&child, agents_dir).await {
                tracing::warn!(
                    parent = %topic_name,
                    subagent = %child,
                    error = %e,
                    "Failed to close a subagent with its parent"
                );
            }
        }
        self.close_one_topic_under(topic_name, agents_dir).await
    }

    /// Every subagent of `topic_name`, deepest first and never including
    /// `topic_name` itself.
    ///
    /// A subagent exists to serve one topic's work and nothing else can find it
    /// again: the parent's grant is the only pointer to it (the `spawned-from`
    /// breadcrumb says who the parent is, but nothing lists a topic's children
    /// from it), so a closed parent would leave it orphaned. A shared
    /// topic_path makes that concrete: the parent's close is what may take that
    /// directory with it (`auto_close_topic`), and a live child pointing into a
    /// deleted dir is nothing anyone asked for.
    ///
    /// Depth-first: a subagent may have subagents of its own, so the whole
    /// subtree goes. A subagent has exactly one parent (the breadcrumb is a
    /// single name), so this terminates.
    ///
    /// ponytail: the grants this reads are process-wide and keyed by the
    /// parent's *name* alone — no channel — so a topic of the same name in
    /// another channel would have its subagents closed here too. The ceiling is
    /// `subagents_of`'s own (the prompt list reads the same registry, by the
    /// same key); the upgrade path is a channel-scoped grant key, which is a
    /// change to `access_grants`, not to this function.
    fn subagent_closure(&self, topic_name: &str) -> Vec<String> {
        let mut stack = vec![topic_name.to_string()];
        let mut doomed = Vec::new();
        while let Some(name) = stack.pop() {
            doomed.push(name.clone());
            stack.extend(
                crate::command::spawn_handler::subagents_of(&name)
                    .into_iter()
                    .map(|(child, _)| child),
            );
        }
        doomed.into_iter().skip(1).rev().collect()
    }

    /// Close one topic: no subagent handling, no recursion — what
    /// [`Self::close_topic_under`] calls, once per topic in the subtree.
    async fn close_one_topic_under(&self, topic_name: &str, agents_dir: &Path) -> Result<()> {
        // Freeze the topic's state *before* deleting: bookkeeping writes that
        // land after the deletion (the close reply's chat-history line,
        // activity-log entries from event subscribers) are dropped instead of
        // recreating the directory. The topic-open path lifts the freeze.
        jyc_types::state_dir::mark_closed(topic_name);
        let topic_path = self
            .topic_path(topic_name)
            .await
            .unwrap_or_else(|| self.storage.workspace().join(topic_name));

        // An agent-spawned subagent hands its parent's filesystem access back
        // here, before anything is deleted: the grant is keyed by this very
        // directory (a no-op for every other topic).
        crate::command::spawn_handler::forget_subagent(topic_name, &topic_path);

        // Pinned topic: relocated state is the topic's data; the dir is
        // user property. Remove the former, preserve the latter. The
        // lookup is by *name*, so topics co-pinning the same dir never
        // destroy each other's state.
        if let Some(state) = jyc_types::state_dir::registered_state(topic_name) {
            // Resolved before the state goes: the dirs a close weighs a home
            // against are read off the workspace, and this topic's own
            // namespace is one of them (see `dir_holder`).
            let own_namespace = state.parent().map(crate::topic_path::resolved);
            if state.exists() {
                tokio::fs::remove_dir_all(&state)
                    .await
                    .context(format!("Failed to remove topic state dir: {:?}", state))?;
            }
            // The dir the state was registered in is the topic's *home* — the
            // one jyc created for it, whether it works in the home itself, in
            // `<home>/.data`, or in a directory it was handed (a pin, a `-d`:
            // then the home is the state namespace beside the state). It goes
            // with the state, contents and all, for every close source alike.
            //
            // Two dirs are not ours to take: one outside the agents tree — the
            // user's checkout, where only the state goes — and one another
            // topic works in or lives inside (a `-s` child, a fork, the topics
            // of the pipe that owns this dir); the guards mirror
            // `purge_topic_dir_under`'s.
            let home = state.parent();
            // Contents are taken only from a home this call can place under an
            // agents root. One it cannot — `/close` resolves the platform root,
            // a test works in a root of its own — is treated as it always was:
            // an emptied dir goes, anything in it stays. No contents on a guess.
            let known = match home {
                Some(home) => {
                    path_is_under(home, agents_dir).await
                        || path_is_under(home, &crate::topic_path::agents_dir(&self.data_dir)).await
                }
                None => false,
            };
            if let Some(home) = home {
                if let Some(holder) = self
                    .dir_holder(topic_name, home, own_namespace.as_deref())
                    .await
                {
                    tracing::info!(
                        topic = %topic_name,
                        dir = %home.display(),
                        holder = %holder,
                        "Topic home kept: another topic works in it or lives inside it"
                    );
                } else if known {
                    // `--purge` runs before a close and takes this same dir for
                    // a topic that works in its home, so it may be gone already.
                    if home.exists() {
                        remove_topic_dir(topic_name, home).await?;
                        tracing::info!(
                            topic = %topic_name,
                            dir = %home.display(),
                            "Topic home deleted with its state"
                        );
                    }
                } else {
                    // A home this call cannot place under a root takes no
                    // contents: an emptied working dir goes with its state, the
                    // way it always did, and anything else stays.
                    if state.parent() == Some(crate::topic_path::topic_home(&topic_path)) {
                        let _ = tokio::fs::remove_dir(&topic_path).await;
                    }
                    if tokio::fs::remove_dir(home).await.is_ok() {
                        tracing::info!(
                            topic = %topic_name,
                            dir = %home.display(),
                            "Emptied topic dir removed with its state"
                        );
                    } else {
                        tracing::info!(
                            topic = %topic_name,
                            dir = %home.display(),
                            "Topic state dir deleted; topic dir kept (not under a root this call resolved)"
                        );
                    }
                }
            }
            self.cleanup_topic_state(topic_name).await;
            return Ok(());
        }

        if topic_path.exists() {
            remove_topic_dir(topic_name, &topic_path).await?;
            tracing::info!(topic = %topic_name, "Topic directory deleted");
        }

        self.cleanup_topic_state(topic_name).await;
        Ok(())
    }

    /// Delete the directory a pinned topic works in — the extra step
    /// behind `/close --force --purge`.
    ///
    /// [`Self::close_topic`] deliberately keeps that directory: for a shared
    /// child or a `/spawn` sibling it is *someone else's* — a checkout, or a
    /// directory the user chose on purpose. Purge is the explicit "and this one too".
    ///
    /// Refuses (deletes nothing) when the dir is not this topic's to remove:
    ///
    /// - another topic works in it — pinned or not: a fork shares its
    ///   parent's dir, and the parent may never have been pinned, so the
    ///   workspace itself is consulted, not just the runtime pin map,
    /// - another topic's dir lives inside it, so deleting it takes them along,
    /// - it *is* one of jyc's own roots, contains one, or sits directly under
    ///   one holding another topic's state (`agents/<name>/.jyc`).
    pub async fn purge_topic_dir(&self, topic_name: &str) -> Result<PurgeOutcome> {
        let agents_dir = self.resolve_agents_dir();
        self.purge_topic_dir_under(topic_name, &agents_dir, &self.data_dir)
            .await
    }

    /// Every other topic's working dir: the runtime pins plus whatever the
    /// workspace itself holds. Pins alone cannot answer "who else works
    /// here?" — an unpinned topic lives at `<workspace>/<name>` and a fork
    /// shares exactly that dir.
    ///
    /// A pin is not evidence on its own, though: nothing removes a pin when its
    /// topic closes, and a shared subagent's pin names a topic that closes with
    /// this one. Both would keep a purged dir alive forever, so the filter asks
    /// [`pin_still_held_by_another`] rather than trusting the map.
    ///
    /// `own_namespace` — this topic's own state dir, resolved — is skipped in
    /// the workspace listing only: a state namespace is not a dir somebody works
    /// in, and on the agents channel the workspace **is** the agents dir, so its
    /// children include a topic's own namespace (`_home_me_repo` for an ad-hoc
    /// topic on `/home/me/repo`). A *pin* on that same dir is another matter and
    /// still counts — that is a topic sharing it on purpose.
    async fn other_topic_dirs(
        &self,
        self_name: &str,
        own_namespace: Option<&Path>,
    ) -> Vec<(String, PathBuf)> {
        let mut dirs: Vec<(String, PathBuf)> = self
            .custom_topic_paths()
            .await
            .into_iter()
            .filter(|(name, pin)| {
                name != self_name && pin_still_held_by_another(name, pin, self_name)
            })
            .collect();
        if let Ok(entries) = std::fs::read_dir(self.storage.workspace()) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == self_name || !entry.path().is_dir() {
                    continue;
                }
                if Some(crate::topic_path::resolved(&entry.path()).as_path()) == own_namespace {
                    continue; // this topic's own state namespace
                }
                dirs.push((name, entry.path()));
            }
        }
        dirs
    }

    /// Name the topic that would go down with `dir` — one that works in it, or
    /// one whose own dir lives inside it. A close takes a dir only when nobody
    /// answers: two guards are [`Self::purge_topic_dir_under`]'s, two read the
    /// disk, because the pin map only knows the topics that are live.
    ///
    /// `own_namespace` is the (resolved) dir this topic's state was registered
    /// in, and the dir being weighed *is* it: the dirs listed for "somebody
    /// works here" are the workspace's children, and on the agents channel the
    /// workspace **is** the agents dir — so a topic whose namespace is a derived
    /// name (`_home_me_repo` for `/home/me/repo`: every ad-hoc and dashboard
    /// topic) would otherwise be listed as its own holder.
    async fn dir_holder(
        &self,
        self_name: &str,
        dir: &Path,
        own_namespace: Option<&Path>,
    ) -> Option<String> {
        let dir = crate::topic_path::resolved(dir);
        for (name, other) in self.other_topic_dirs(self_name, own_namespace).await {
            let other = crate::topic_path::resolved(&other);
            if other == dir || path_is_under(&other, &dir).await {
                return Some(name);
            }
        }
        // One level only: a topic dir jyc makes sits directly in a home, and
        // anything deeper is not something a close should guess about.
        let mut entries = tokio::fs::read_dir(&dir).await.ok()?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let is_dir = entry
                .file_type()
                .await
                .map(|kind| kind.is_dir())
                .unwrap_or(false);
            if !is_dir {
                continue;
            }
            // A `.jyc` with a breadcrumb names the topic that lives there; one
            // that is this topic's own, or a closed topic's leftover, is not
            // somebody to wait for.
            let nested_state = entry.path().join(".jyc");
            let nested_name = tokio::fs::read_to_string(nested_state.join("topic-name"))
                .await
                .map(|name| name.trim().to_string())
                .unwrap_or_default();
            if nested_name == self_name
                || (!nested_name.is_empty() && jyc_types::state_dir::is_closed(&nested_name))
            {
                continue;
            }
            if nested_state.is_dir() {
                return Some(entry.file_name().to_string_lossy().into_owned());
            }
        }
        None
    }

    /// Testable core of [`Self::purge_topic_dir`] with the resolved agents root
    /// and the instance's data dir passed in, so no test can reach the real
    /// data home (mirrors [`Self::auto_close_topic_under`]).
    async fn purge_topic_dir_under(
        &self,
        topic_name: &str,
        agents_dir: &Path,
        data_dir: &Path,
    ) -> Result<PurgeOutcome> {
        // Not the kept-dir case: an unpinned topic's dir is
        // `<workspace>/<name>`, which `close_topic` deletes on its own, and a
        // dir that is already gone has nothing to purge. `topic_path` answers
        // the workspace fallback for such topics, so the distinction has to be
        // made here, not by whether the lookup returned at all.
        let Some(topic_path) = self.topic_path(topic_name).await else {
            return Ok(PurgeOutcome::Nothing);
        };
        if topic_path == self.storage.workspace().join(topic_name) {
            return Ok(PurgeOutcome::Nothing);
        }
        // A topic's own dir is its *home*, and a topic that works in
        // `<home>/.data` has the home created for it — so purging the home is
        // what takes that working directory with it. Only when the home really
        // is this topic's: the state dir beside it is the one registered for
        // this topic. Otherwise `<dir>/.data` is a dir the topic was *given*
        // (`-d`, a pin), whose neighbour is somebody else's and stays.
        let home = crate::topic_path::topic_home(&topic_path);
        let state = jyc_types::state_dir::resolve_state_dir(topic_name, &topic_path);
        let own_home = state.parent() == Some(home);
        let path = crate::topic_path::resolved(if own_home { home } else { &topic_path });
        if !path.exists() {
            return Ok(PurgeOutcome::Nothing);
        }

        let mut shared = Vec::new();
        let mut nested = Vec::new();
        for (name, pin) in self.other_topic_dirs(topic_name, None).await {
            if crate::topic_path::resolved(&pin) == path {
                shared.push(name);
            } else if path_is_under(&pin, &path).await {
                nested.push(name);
            }
        }
        if !shared.is_empty() {
            return Ok(PurgeOutcome::Refused(format!(
                "{} is also the topic dir of {} — deleting it would take that topic with it.",
                path.display(),
                shared.join(", ")
            )));
        }
        if !nested.is_empty() {
            return Ok(PurgeOutcome::Refused(format!(
                "{} contains the topic dir of {} — deleting it would take that topic with it.",
                path.display(),
                nested.join(", ")
            )));
        }
        for root in [agents_dir, &crate::topic_path::agents_dir(data_dir)] {
            let root = crate::topic_path::resolved(root);
            if path.parent().is_some_and(|p| p == root.as_path()) {
                return Ok(PurgeOutcome::Refused(format!(
                    "{} is a state namespace — it holds another topic's `.jyc`, not a workspace.",
                    path.display()
                )));
            }
            if root.starts_with(&path) {
                return Ok(PurgeOutcome::Refused(format!(
                    "{} is jyc's own directory ({} lives in it) — it is not a topic dir.",
                    path.display(),
                    root.display()
                )));
            }
        }

        remove_topic_dir(topic_name, &path).await?;
        tracing::info!(topic = %topic_name, dir = %path.display(), "Topic directory purged");
        Ok(PurgeOutcome::Deleted)
    }

    /// The root that holds every agent's topic subtree (`<data dir>/agents/`).
    ///
    /// A forked topic lands at `<root>/<agent>/<name>` — the same place the
    /// router puts runtime topics, and the same boundary
    /// [`Self::auto_close_topic`] refuses to delete above.
    pub fn resolve_agents_dir(&self) -> std::path::PathBuf {
        crate::topic_path::resolve_agents_dir(&self.data_dir)
    }

    /// Close a topic in response to an upstream close event (issue/PR
    /// closed, chat disbanded, ...).
    ///
    /// What gets deleted is [`Self::close_topic`]'s answer: a close off a
    /// channel takes the topic's home, contents and all, exactly as `/close`
    /// does — the two are one teardown, and the home/pipe boundary is drawn in
    /// one place. All this adds is the rule in front of it: an upstream close
    /// never touches a topic whose working directory is not under the agents
    /// tree (a checkout somebody pinned), which is skipped whole — not even
    /// closed — with an info log.
    ///
    /// Returns `true` when the topic was actually closed.
    pub async fn auto_close_topic(&self, topic_name: &str) -> Result<bool> {
        let agents_dir = self.resolve_agents_dir();
        self.auto_close_topic_under(topic_name, &agents_dir).await
    }

    /// Testable core of `auto_close_topic` with an explicit agents root.
    async fn auto_close_topic_under(&self, topic_name: &str, agents_dir: &Path) -> Result<bool> {
        let topic_path = self
            .topic_path(topic_name)
            .await
            .unwrap_or_else(|| self.storage.workspace().join(topic_name));
        if !path_is_under(&topic_path, agents_dir).await {
            tracing::info!(
                topic = %topic_name,
                path = %topic_path.display(),
                "Auto-close skipped: topic path is not under the agents workspace root"
            );
            return Ok(false);
        }
        // `close_topic_under`, not `close_topic`: the deletion has to weigh the
        // home against the root this close was called with, or an auto-close
        // for one agents dir would take the contents of a dir under another.
        self.close_topic_under(topic_name, agents_dir).await?;
        Ok(true)
    }

    /// Reset the session for a topic with configurable compression.
    ///
    /// Delegates to the agent service's `reset_session` method.
    pub async fn reset_session(
        &self,
        topic_name: &str,
        config: &jyc_types::channel::ResetCompressionConfig,
    ) -> Result<()> {
        let topic_path = self
            .topic_path(topic_name)
            .await
            .unwrap_or_else(|| self.storage.workspace().join(topic_name));
        self.agent
            .reset_session(&topic_path, topic_name, config)
            .await?;

        // Publish SessionStatus event for dashboard visibility
        if self.enable_events {
            let event_bus = self.get_or_create_event_bus(topic_name).await;
            if let Some(bus) = event_bus {
                let mode_str = match config.mode {
                    jyc_types::channel::CompressionMode::None => "none",
                    jyc_types::channel::CompressionMode::Heuristic => "heuristic",
                    jyc_types::channel::CompressionMode::Llm => "llm",
                };
                let _ = bus
                    .publish(crate::topic_event::TopicEvent::SessionStatus {
                        topic_name: topic_name.to_string(),
                        status_type: "session_reset".to_string(),
                        attempt: None,
                        message: Some(format!("mode={mode_str}")),
                        timestamp: chrono::Utc::now(),
                    })
                    .await;
            }
        }

        Ok(())
    }

    /// Clean up in-memory state (queues, event buses) for a closed topic.
    async fn cleanup_topic_state(&self, topic_name: &str) {
        // Cancel the per-topic token so the worker + event listener exit promptly
        {
            let mut cancels = self.topic_cancels.lock().await;
            if let Some(token) = cancels.remove(topic_name) {
                token.cancel();
                tracing::debug!(topic = %topic_name, "Per-topic cancellation token cancelled");
            }
        }

        // Remove from topic_queues
        {
            let mut queues = self.topic_queues.lock().await;
            queues.remove(topic_name);
        }

        // Remove from event_buses
        if self.enable_events {
            let mut event_buses = self.event_buses.lock().await;
            event_buses.remove(topic_name);
        }

        tracing::debug!(topic = %topic_name, "Topic in-memory state cleaned up");
    }
}

/// Whether another topic's pin still stands for a topic that holds its own dir:
/// false for one that has closed, and false for a *dependent*.
///
/// The pin map cannot answer this on its own. Nothing removes a pin when its
/// topic closes, so a closed topic would keep somebody else's directory alive
/// forever — `state_dir`'s closed set is what says the topic is done. (After a
/// restart the stale pin is gone anyway: the map is in-memory, and the state dir
/// whose `topic-path` breadcrumb could restore it has been deleted.)
///
/// A subagent spawned into `self_name`'s own dir is even less of a claim: it is
/// closed with this topic (and its dir with it), so it does not make that dir
/// somebody else's — whichever of the two orders it closes in. A child that
/// shares this topic's dir is not a claim either, and for the same reason: the
/// cascade closes it first (`subagent_closure`), so by the time the directory is
/// weighed it has no holder left.
fn pin_still_held_by_another(name: &str, pin: &Path, self_name: &str) -> bool {
    !jyc_types::state_dir::is_closed(name)
        && crate::command::spawn_handler::spawned_from(name, pin).as_deref() != Some(self_name)
}

/// Canonicalized containment check: `path` must be strictly under `root`.
///
/// Canonicalization resolves symlinks and `..`, so a topic dir symlinked
/// out of the agents tree is still refused. Non-existent paths fall back
/// to a literal comparison (nothing to delete in that case anyway).
async fn path_is_under(path: &Path, root: &Path) -> bool {
    let path = crate::topic_path::resolved(path);
    let root = crate::topic_path::resolved(root);
    path != root && path.starts_with(&root)
}

/// What [`TopicManager::purge_topic_dir`] did, or refused to do.
#[derive(Debug)]
pub enum PurgeOutcome {
    /// Nothing on disk to delete: the topic was never pinned (its dir is the
    /// default `<workspace>/<name>`, which `close_topic` deletes on its own),
    /// or the dir is already gone. `close_topic` still has the state to remove.
    Nothing,
    /// The topic dir was deleted.
    Deleted,
    /// Nothing was deleted; the message says why, for the user.
    Refused(String),
}

/// Delete a topic directory: the `repo` symlink first, then the tree.
///
/// `remove_dir_all` follows symlinks, so a dir holding a link into a shared
/// checkout would delete through it. The link goes first; what remains is a
/// plain tree.
async fn remove_topic_dir(topic_name: &str, path: &Path) -> Result<()> {
    let repo_symlink = path.join("repo");
    if let Ok(meta) = tokio::fs::symlink_metadata(&repo_symlink).await
        && meta.file_type().is_symlink()
    {
        if let Err(e) = tokio::fs::remove_file(&repo_symlink).await {
            tracing::warn!(
                error = %e,
                path = %repo_symlink.display(),
                "Failed to remove repo symlink before topic deletion"
            );
        } else {
            tracing::debug!(
                topic = %topic_name,
                "Removed repo symlink before topic deletion"
            );
        }
    }

    tokio::fs::remove_dir_all(path)
        .await
        .context(format!("Failed to remove topic directory: {:?}", path))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_storage::MessageStorage;
    use crate::metrics::MetricsCollector;
    use crate::static_agent::StaticAgentService;
    use arc_swap::ArcSwap;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

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
            _topic_path: &std::path::Path,
            _message_dir: &str,
            _attachments: Option<&[jyc_types::OutboundAttachment]>,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "noop".to_string(),
            })
        }
        async fn send_message(
            &self,
            _recipient: &str,
            _subject: &str,
            _body: &str,
        ) -> Result<jyc_types::SendResult> {
            Ok(jyc_types::SendResult {
                message_id: "noop".to_string(),
            })
        }
    }

    fn make_tm(workspace: &Path) -> Arc<TopicManager> {
        let storage = Arc::new(MessageStorage::new(workspace));
        let cancel = CancellationToken::new();
        let (metrics, _stats, _task) = MetricsCollector::new(CancellationToken::new()).start();
        let config = Arc::new(ArcSwap::from_pointee(
            jyc_types::load_config_from_str(
                r#"
[general]
[channels.test]
type = "email"
[channels.test.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.test.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
"#,
            )
            .unwrap(),
        ));
        Arc::new(TopicManager::new_with_options(
            1,
            10,
            storage,
            Arc::new(NoopOutbound),
            Arc::new(StaticAgentService::new("ok")),
            cancel,
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

    /// Topic inside the agents root: auto-close deletes it.
    #[tokio::test]
    async fn auto_close_deletes_topic_under_agents_dir() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let workspace = agents_dir.join("agent-x");
        let topic_dir = workspace.join("plan-42");
        std::fs::create_dir_all(&topic_dir).unwrap();

        let tm = make_tm(&workspace);
        let closed = tm
            .auto_close_topic_under("plan-42", &agents_dir)
            .await
            .unwrap();
        assert!(closed);
        assert!(!topic_dir.exists());
    }

    /// Topic pinned to a custom path outside the agents root: never deleted.
    #[tokio::test]
    async fn auto_close_skips_custom_path_outside_agents_dir() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let workspace = agents_dir.join("agent-x");
        std::fs::create_dir_all(&workspace).unwrap();
        let outside = tmp.path().join("projects").join("jyc");
        std::fs::create_dir_all(&outside).unwrap();

        let tm = make_tm(&workspace);
        tm.topic_paths
            .lock()
            .await
            .insert("jyc".to_string(), outside.clone());

        let closed = tm.auto_close_topic_under("jyc", &agents_dir).await.unwrap();
        assert!(!closed);
        assert!(outside.exists());
    }

    /// A symlinked topic dir pointing outside the agents root must not be
    /// deleted — canonicalization resolves the link before the check.
    #[cfg(unix)]
    #[tokio::test]
    async fn auto_close_refuses_symlink_escape() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let workspace = agents_dir.join("agent-x");
        std::fs::create_dir_all(&workspace).unwrap();
        let real = tmp.path().join("real-project");
        std::fs::create_dir_all(&real).unwrap();
        let link = workspace.join("evil");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let tm = make_tm(&workspace);
        let closed = tm
            .auto_close_topic_under("evil", &agents_dir)
            .await
            .unwrap();
        assert!(!closed);
        assert!(real.exists());
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    }

    /// An upstream close takes a pipe topic's dir with its contents, the way
    /// every close source does now.
    #[tokio::test]
    async fn auto_close_deletes_nonempty_dynamic_topic_dir() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let topic_dir = agents_dir.join("agent-x").join("pr-dyn-1");
        let state = topic_dir.join(".jyc");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(topic_dir.join("work.rs"), "fn wip() {}").unwrap();
        std::fs::write(state.join("chat.jsonl"), "{}").unwrap();
        jyc_types::state_dir::register("pr-dyn-1", &state);

        let tm = make_tm(&agents_dir);
        tm.set_topic_path("pr-dyn-1", topic_dir.clone())
            .await
            .unwrap();

        let closed = tm
            .auto_close_topic_under("pr-dyn-1", &agents_dir)
            .await
            .unwrap();

        assert!(closed);
        assert!(
            !topic_dir.exists(),
            "the whole topic dir must go, files and all"
        );
        jyc_types::state_dir::unregister("pr-dyn-1");
    }

    /// A topic working in its own home one level under the agents root — the
    /// `<agents dir>/<name>` a `/spawn` or a self-named pipe topic gets — is a
    /// topic like any other: its home goes, contents and all. The agents root
    /// above it does not.
    #[tokio::test]
    async fn auto_close_deletes_an_agents_own_dir_with_its_home() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let agent_dir = agents_dir.join("agent-own-1");
        let state = agent_dir.join(".jyc");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(agent_dir.join("notes.md"), "# notes").unwrap();
        jyc_types::state_dir::register("agent-own-1", &state);

        let tm = make_tm(&agents_dir);
        tm.set_topic_path("agent-own-1", agent_dir.clone())
            .await
            .unwrap();

        let closed = tm
            .auto_close_topic_under("agent-own-1", &agents_dir)
            .await
            .unwrap();

        assert!(closed);
        assert!(
            !agent_dir.exists(),
            "the topic's home goes with its state, notes.md included"
        );
        assert!(agents_dir.exists(), "the agents root itself survives");
        jyc_types::state_dir::unregister("agent-own-1");
    }

    /// A dir another topic also works in (a shared child works in its parent's)
    /// survives an upstream close of the parent, even in the dynamic shape.
    #[tokio::test]
    async fn auto_close_keeps_dir_shared_with_another_topic() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let shared = agents_dir.join("agent-x").join("pr-share-1");
        let state = shared.join(".jyc");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(shared.join("work.rs"), "fn wip() {}").unwrap();
        jyc_types::state_dir::register("pr-share-1", &state);

        let tm = make_tm(&agents_dir);
        tm.set_topic_path("pr-share-1", shared.clone())
            .await
            .unwrap();
        tm.set_topic_path("pr-share-1-fork", shared.clone())
            .await
            .unwrap();

        let closed = tm
            .auto_close_topic_under("pr-share-1", &agents_dir)
            .await
            .unwrap();

        assert!(closed);
        assert!(
            shared.join("work.rs").exists(),
            "a dir another topic works in must survive"
        );
        jyc_types::state_dir::unregister("pr-share-1");
    }

    /// The reported case, and what `/close` now does for every close source: a
    /// pipe topic's own dir — `<agents dir>/<pipe>/<topic>` with its `.data`
    /// and the files in it — goes with the topic's state, while the pipe's dir
    /// one level up stays. Nothing above a home is ever taken.
    #[tokio::test]
    async fn manual_close_takes_the_home_and_keeps_the_pipe_dir() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let pipe_dir = agents_dir.join("mail-bot");
        let topic_dir = pipe_dir.join("pr-129");
        let state = topic_dir.join(".jyc");
        let work = topic_dir.join(".data");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("work.rs"), "fn wip() {}").unwrap();
        jyc_types::state_dir::register("pr-129", &state);

        let tm = make_tm(&agents_dir);
        tm.set_topic_path("pr-129", work.clone()).await.unwrap();

        tm.close_topic_under("pr-129", &agents_dir).await.unwrap();

        assert!(
            !topic_dir.exists(),
            "the topic's home goes, its working directory and files with it"
        );
        assert!(
            pipe_dir.exists(),
            "the pipe's own dir is not the topic's to delete"
        );
        jyc_types::state_dir::unregister("pr-129");
    }

    /// Pinned topic with a registered state dir: close deletes the state
    /// dir *and* the emptied `agents/<topic>/` shell around it, but keeps
    /// the registration (a reopen reuses the same state dir) and the topic
    /// dir (user-owned repo) survives.
    #[tokio::test]
    async fn close_pinned_topic_removes_state_keeps_dir() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let repo = tmp.path().join("probe-pin-repo");
        let namespace = tmp.path().join("agents/probe-pin-app");
        let state = namespace.join(".jyc");
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("main.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("chat.jsonl"), "{}").unwrap();
        jyc_types::state_dir::register("probe-pin-app", &state);
        // set_topic_path skips adoption when already registered
        tm.set_topic_path("probe-pin-app", repo.clone())
            .await
            .unwrap();

        tm.close_topic_under("probe-pin-app", &agents_dir)
            .await
            .unwrap();

        assert!(
            repo.join("main.rs").exists(),
            "pinned topic dir must survive"
        );
        assert!(
            !repo.join(".jyc").exists(),
            "no state should be recreated in dir"
        );
        assert!(!state.exists(), "relocated state dir must be deleted");
        assert!(
            !namespace.exists(),
            "the state namespace goes with its state, contents and all"
        );
        assert!(agents_dir.exists(), "the agents root itself must survive");
        assert_eq!(
            jyc_types::state_dir::registered_state("probe-pin-app"),
            Some(state.clone()),
            "registration kept so a reopen reuses the same state dir"
        );
        assert_eq!(
            jyc_types::state_dir::resolve_state_dir("probe-pin-app", &repo),
            state
        );
    }

    /// The home a pinned topic's state lives in goes with that state — the
    /// namespace is the topic's own dir, its stray files included. The dir the
    /// topic was *given* is the user's and stays.
    #[tokio::test]
    async fn close_deletes_the_state_namespace_with_its_state() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let repo = tmp.path().join("probe-prune-repo");
        let namespace = tmp.path().join("agents/probe-prune-full");
        let state = namespace.join(".jyc");
        let agents_dir = tmp.path().join("agents");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(namespace.join("notes.txt"), "keep me").unwrap();
        jyc_types::state_dir::register("probe-prune-full", &state);
        tm.set_topic_path("probe-prune-full", repo.clone())
            .await
            .unwrap();

        tm.close_topic_under("probe-prune-full", &agents_dir)
            .await
            .unwrap();

        assert!(
            !namespace.exists(),
            "the topic's own namespace goes, contents and all"
        );
        assert!(
            repo.join("main.rs").exists(),
            "the dir the topic worked in is the user's"
        );
        assert!(agents_dir.exists(), "the agents root itself must survive");
    }

    /// Unregistered topic under the agents root (a legacy layout, state
    /// in-dir): the topic's dir goes, the parent dir does not — that one
    /// belongs to the agent or pipe that owns it.
    #[tokio::test]
    async fn close_keeps_the_parent_dir_of_a_legacy_topic() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let workspace = agents_dir.join("probe-agent-x");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let topic_dir = workspace.join("probe-plan-42");
        std::fs::create_dir_all(topic_dir.join(".jyc")).unwrap();
        std::fs::write(topic_dir.join("f.txt"), "x").unwrap();

        tm.close_topic_under("probe-plan-42", &agents_dir)
            .await
            .unwrap();

        assert!(!topic_dir.exists());
        assert!(
            workspace.exists(),
            "the parent dir is not the closed topic's to take"
        );
        assert!(agents_dir.exists(), "the agents root itself must survive");
    }

    /// A working directory outside the agents tree is the user's: the close
    /// takes the state namespace jyc made for the topic, and leaves whatever
    /// lives in that directory alone.
    #[tokio::test]
    async fn close_keeps_a_working_dir_outside_the_agents_tree() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let outside = tmp.path().join("projects").join("jyc");
        let namespace = agents_dir.join("probe-outside-1");
        let state = namespace.join(".jyc");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("mine.rs"), "fn mine() {}").unwrap();
        jyc_types::state_dir::register("probe-outside-1", &state);

        let tm = make_tm(&agents_dir);
        tm.set_topic_path("probe-outside-1", outside.clone())
            .await
            .unwrap();

        tm.close_topic_under("probe-outside-1", &agents_dir)
            .await
            .unwrap();

        assert!(
            outside.join("mine.rs").exists(),
            "a dir outside the agents tree is never deleted"
        );
        assert!(
            !namespace.exists(),
            "the state namespace jyc made for the topic still goes"
        );
        jyc_types::state_dir::unregister("probe-outside-1");
    }

    /// A topic whose own dir lives inside the home being closed keeps that
    /// home: a self-named topic closing must not take the pipe's other topics
    /// with it. The sibling here is deliberately *not* pinned, so only the
    /// disk can tell the home is shared.
    #[tokio::test]
    async fn close_keeps_a_home_another_topic_lives_in() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let home = agents_dir.join("probe-pipe-1");
        let state = home.join(".jyc");
        let nested = home.join("info");
        let nested_state = nested.join(".jyc");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&nested_state).unwrap();
        std::fs::write(home.join("notes.md"), "# notes").unwrap();
        jyc_types::state_dir::register("probe-pipe-1", &state);
        jyc_types::state_dir::register("info", &nested_state);

        let tm = make_tm(&agents_dir);
        tm.set_topic_path("probe-pipe-1", home.clone())
            .await
            .unwrap();

        tm.close_topic_under("probe-pipe-1", &agents_dir)
            .await
            .unwrap();

        assert!(
            nested_state.exists(),
            "a dir another topic lives in must survive its parent's close"
        );
        assert!(!state.exists(), "the closed topic's own state still goes");
        jyc_types::state_dir::unregister("probe-pipe-1");
        jyc_types::state_dir::unregister("info");
    }

    /// A derived state namespace is this topic's own dir, not somebody else's:
    /// on the agents channel the workspace *is* the agents dir, so the dirs the
    /// guard lists as "somebody works here" are the namespaces themselves — a
    /// topic opened on `/home/me/repo` keeps its state in `_home_me_repo` there,
    /// and the close still has to take that namespace.
    #[tokio::test]
    async fn close_deletes_a_derived_state_namespace() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let namespace = agents_dir.join("_home_me_repo");
        let state = namespace.join(".jyc");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("topic-name"), "adhoc-1").unwrap();
        std::fs::write(namespace.join("notes.txt"), "mine").unwrap();
        jyc_types::state_dir::register("adhoc-1", &state);

        // The workspace is the agents dir, exactly as the agents channel sets it.
        let tm = make_tm(&agents_dir);
        tm.set_topic_path("adhoc-1", tmp.path().join("repo"))
            .await
            .unwrap();

        tm.close_topic_under("adhoc-1", &agents_dir).await.unwrap();

        assert!(
            !namespace.exists(),
            "its own namespace is not another topic's dir"
        );
        jyc_types::state_dir::unregister("adhoc-1");
    }

    /// A leftover `.jyc` of a *closed* topic inside the home is nobody to wait
    /// for: the home still goes with its contents.
    #[tokio::test]
    async fn close_takes_a_home_whose_nested_state_is_a_leftover() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let home = agents_dir.join("probe-leftover");
        let state = home.join(".jyc");
        let stale = home.join("old-topic").join(".jyc");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("topic-name"), "probe-leftover-old").unwrap();
        jyc_types::state_dir::mark_closed("probe-leftover-old");
        jyc_types::state_dir::register("probe-leftover", &state);

        let tm = make_tm(&agents_dir);
        tm.set_topic_path("probe-leftover", home.clone())
            .await
            .unwrap();

        tm.close_topic_under("probe-leftover", &agents_dir)
            .await
            .unwrap();

        assert!(
            !home.exists(),
            "a closed topic's leftover state is not a holder"
        );
        jyc_types::state_dir::unregister("probe-leftover");
    }

    /// `/close --force --purge` purges first and closes second, so for a topic
    /// that works in its home the close finds the dir already gone — it has to
    /// close anyway, not fail on a dir that is not there.
    #[tokio::test]
    async fn close_after_purge_still_closes() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let topic_dir = agents_dir.join("agent-x").join("pr-purged");
        let state = topic_dir.join(".jyc");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(topic_dir.join("work.rs"), "fn wip() {}").unwrap();
        jyc_types::state_dir::register("pr-purged", &state);

        let tm = make_tm(&agents_dir);
        tm.set_topic_path("pr-purged", topic_dir.clone())
            .await
            .unwrap();

        let outcome = tm
            .purge_topic_dir_under("pr-purged", &agents_dir, tmp.path())
            .await
            .unwrap();
        assert!(matches!(outcome, PurgeOutcome::Deleted), "{outcome:?}");
        assert!(!topic_dir.exists(), "the purge took the whole home");

        tm.close_topic_under("pr-purged", &agents_dir)
            .await
            .unwrap();

        assert!(!topic_dir.exists(), "the close is a no-op on a gone home");
        jyc_types::state_dir::unregister("pr-purged");
    }

    /// Two topics pinning the same dir: closing one deletes only its own
    /// name-keyed state; the sibling's state and the shared dir survive.
    #[tokio::test]
    async fn close_co_pinned_topic_only_deletes_own_state() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let repo = tmp.path().join("probe-co-pin");
        std::fs::create_dir_all(&repo).unwrap();
        let state_a = tmp.path().join("agents/probe-co-a/.jyc");
        let state_b = tmp.path().join("agents/probe-co-b/.jyc");
        std::fs::create_dir_all(&state_a).unwrap();
        std::fs::create_dir_all(&state_b).unwrap();
        jyc_types::state_dir::register("probe-co-a", &state_a);
        jyc_types::state_dir::register("probe-co-b", &state_b);
        tm.set_topic_path("probe-co-a", repo.clone()).await.unwrap();

        tm.close_topic("probe-co-a").await.unwrap();

        assert!(!state_a.exists(), "closed topic's state gone");
        assert!(state_b.exists(), "sibling state untouched");
        assert!(repo.exists(), "shared topic dir untouched");
        assert!(jyc_types::state_dir::registered_state("probe-co-b").is_some());
    }

    /// Unregistered topic dirs (workspace/dynamic) keep the legacy
    /// behavior: everything is deleted.
    #[tokio::test]
    async fn close_unregistered_topic_deletes_dir() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let topic_dir = workspace.join("probe-plain-topic");
        std::fs::create_dir_all(topic_dir.join(".jyc")).unwrap();
        std::fs::write(topic_dir.join("f.txt"), "x").unwrap();

        tm.close_topic("probe-plain-topic").await.unwrap();
        assert!(!topic_dir.exists());
    }

    /// `/close --force --purge` on a pinned topic: the state *and* the dir the
    /// topic worked in go, which is the one thing `close_topic` refuses to do.
    #[tokio::test]
    async fn purge_deletes_a_pinned_topic_dir() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let repo = tmp.path().join("probe-purge-repo");
        let state = tmp.path().join("agents/probe-purge/.jyc");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("main.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(&state).unwrap();
        jyc_types::state_dir::register("probe-purge", &state);
        tm.set_topic_path("probe-purge", repo.clone())
            .await
            .unwrap();

        let outcome = tm
            .purge_topic_dir_under(
                "probe-purge",
                &tmp.path().join("agents"),
                &tmp.path().join("state"),
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, PurgeOutcome::Deleted),
            "the pinned dir is what purge deletes"
        );
        assert!(!repo.exists(), "purge is the explicit 'and this dir too'");
    }

    /// Two topics on one dir: purging from either one refuses — the other
    /// topic's workspace is not on the table.
    #[tokio::test]
    async fn purge_refuses_a_dir_another_topic_also_uses() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let shared = tmp.path().join("probe-shared-repo");
        std::fs::create_dir_all(&shared).unwrap();
        let state = tmp.path().join("agents/probe-one/.jyc");
        std::fs::create_dir_all(&state).unwrap();
        jyc_types::state_dir::register("probe-one", &state);
        tm.set_topic_path("probe-one", shared.clone())
            .await
            .unwrap();
        tm.set_topic_path("probe-two", shared.clone())
            .await
            .unwrap();

        let outcome = tm
            .purge_topic_dir_under(
                "probe-one",
                &tmp.path().join("agents"),
                &tmp.path().join("state"),
            )
            .await
            .unwrap();
        let PurgeOutcome::Refused(reason) = outcome else {
            panic!("a co-pinned dir must be refused, got {outcome:?}");
        };
        assert!(reason.contains("probe-two"), "{reason}");
        assert!(shared.exists(), "nothing may be deleted on a refusal");
    }

    /// A pin is not evidence of a live topic: nothing removes one when its topic
    /// closes, and a closed co-pinner must not keep the dir out of reach — while
    /// a *live* one still must, which is the same check's whole point.
    #[tokio::test]
    async fn purge_ignores_a_pin_whose_topic_is_gone() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let shared = tmp.path().join("probe-shared-repo");
        std::fs::create_dir_all(&shared).unwrap();
        tm.set_topic_path("stale-one", shared.clone())
            .await
            .unwrap();
        tm.set_topic_path("stale-two", shared.clone())
            .await
            .unwrap();
        let agents_dir = tmp.path().join("agents");
        let state_root = tmp.path().join("state");

        let outcome = tm
            .purge_topic_dir_under("stale-one", &agents_dir, &state_root)
            .await
            .unwrap();
        let PurgeOutcome::Refused(reason) = outcome else {
            panic!("a live co-pinner owns the dir, got {outcome:?}");
        };
        assert!(reason.contains("stale-two"), "{reason}");

        // Its topic closes — through the real path, which is what marks it
        // closed — and the pin stays in the map: exactly the case that used to
        // keep the dir forever.
        tm.close_topic("stale-two").await.unwrap();
        let outcome = tm
            .purge_topic_dir_under("stale-one", &agents_dir, &state_root)
            .await
            .unwrap();
        assert!(
            matches!(outcome, PurgeOutcome::Deleted),
            "a closed topic's pin must not keep another topic's dir: {outcome:?}"
        );
        assert!(!shared.exists());
        assert!(
            tm.custom_topic_paths().await.contains_key("stale-two"),
            "the guard is what changed, not the pin map"
        );

        for name in ["stale-one", "stale-two"] {
            jyc_types::state_dir::unregister(name);
            jyc_types::state_dir::clear_closed(name);
        }
    }

    /// A subagent that works in its parent's directory is not a co-owner: it
    /// closes with the parent, so its pin must not keep that dir alive — in the
    /// shape a restart leaves behind too, where the grant is gone and only the
    /// breadcrumb is left.
    #[tokio::test]
    async fn purge_ignores_a_subagents_pin_on_its_parents_dir() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let shared = tmp.path().join("probe-shared-repo");
        std::fs::create_dir_all(&shared).unwrap();
        tm.set_topic_path("sub-parent", shared.clone())
            .await
            .unwrap();
        tm.set_topic_path("sub-child", shared.clone())
            .await
            .unwrap();
        let child_state = jyc_types::state_dir::registered_state("sub-child").unwrap();
        std::fs::write(child_state.join("spawned-from"), "sub-parent").unwrap();

        let outcome = tm
            .purge_topic_dir_under(
                "sub-parent",
                &tmp.path().join("agents"),
                &tmp.path().join("state"),
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, PurgeOutcome::Deleted),
            "a subagent's pin is not a claim on the parent's dir: {outcome:?}"
        );
        assert!(!shared.exists());

        for name in ["sub-parent", "sub-child"] {
            jyc_types::state_dir::unregister(name);
        }
    }

    /// An upstream close takes a shared subagent's parent dir with it: the child
    /// closes first (the cascade, so its pin is gone by the time the guards run)
    /// and the dir goes contents and all, exactly as it would without a child.
    ///
    /// The names here are this test's own — `state_dir` and `access_grants` are
    /// process-wide and keyed by topic name alone while this binary's tests run
    /// in parallel, so reusing another test's `agent-x/plan-42` would make each
    /// test hijack the other's state registration.
    #[tokio::test]
    async fn auto_close_deletes_a_shared_subagents_parent_dir() {
        let tmp = tempdir().unwrap();
        let agents_dir = tmp.path().join("agents");
        let workspace = agents_dir.join("agent-share");
        let topic_dir = workspace.join("sh-plan-9");
        std::fs::create_dir_all(topic_dir.join(".jyc")).unwrap();
        std::fs::write(topic_dir.join("notes.md"), "written into it").unwrap();

        // A workspace of its own, *not* the topic's parent: `purge` treats a
        // pinned path that still looks like `<workspace>/<name>` as an
        // unpinned topic's dir and leaves it to `close_topic` (which is what
        // the sibling test above exercises). What is under test here is the
        // shape rule, so the pin has to be a real one.
        let ws = agents_dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let tm = make_tm(&ws);
        // A pipe topic: path and state registered, the in-dir `.jyc` the router
        // adopts when it activates the topic.
        jyc_types::state_dir::register("sh-plan-9", &topic_dir.join(".jyc"));
        tm.topic_paths
            .lock()
            .await
            .insert("sh-plan-9".to_string(), topic_dir.clone());

        crate::command::spawn_handler::spawn_subagent_under(
            &tm,
            "sh-plan-9",
            &topic_dir,
            Some("sh-plan-9-review"),
            false,
            true,
            None,
            None,
            &agents_dir,
        )
        .await
        .unwrap();
        let child_state = agents_dir.join("sh-plan-9-review").join(".jyc");
        assert!(child_state.is_dir(), "the child's state is its own");

        assert!(
            tm.auto_close_topic_under("sh-plan-9", &agents_dir)
                .await
                .unwrap(),
            "a pipe topic's upstream close still closes it"
        );
        assert!(
            !topic_dir.exists(),
            "the shared dir goes with its contents, child or no child"
        );
        assert!(
            !child_state.exists(),
            "the child's state went with the child"
        );
        // Not asserted: the emptied `agents/<child>/` shell. That pruning runs
        // against the instance's own agents root (`resolve_agents_dir`, which
        // prefers the platform data home), while this test's temp `agents_dir`
        // is deliberately not it.

        for name in ["sh-plan-9", "sh-plan-9-review"] {
            jyc_types::state_dir::unregister(name);
            jyc_types::state_dir::clear_closed(name);
        }
        jyc_types::access_grants::clear("sh-plan-9");
    }

    /// A topic dir that contains another topic's dir is refused: deleting it
    /// would take the nested topic with it.
    #[tokio::test]
    async fn purge_refuses_a_dir_that_contains_another_topic() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let outer = tmp.path().join("probe-outer");
        let inner = outer.join("inner-project");
        std::fs::create_dir_all(&inner).unwrap();
        tm.set_topic_path("probe-outer", outer.clone())
            .await
            .unwrap();
        tm.set_topic_path("probe-inner", inner.clone())
            .await
            .unwrap();

        let outcome = tm
            .purge_topic_dir_under(
                "probe-outer",
                &tmp.path().join("agents"),
                &tmp.path().join("state"),
            )
            .await
            .unwrap();
        let PurgeOutcome::Refused(reason) = outcome else {
            panic!("an ancestor of another topic dir must be refused, got {outcome:?}");
        };
        assert!(reason.contains("probe-inner"), "{reason}");
        assert!(outer.exists());
    }

    /// A fork shares its parent's workspace dir, and the parent may never have
    /// been pinned — pins alone cannot see it, so the workspace scan has to.
    #[tokio::test]
    async fn purge_refuses_a_dir_a_workspace_topic_also_uses() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let parent_dir = workspace.join("probe-parent");
        std::fs::create_dir_all(parent_dir.join(".jyc")).unwrap();
        std::fs::write(parent_dir.join("code.rs"), "x").unwrap();
        // The child adopts its own state out of the parent dir — exactly what a
        // shared spawn does — so no `.jyc` remains there to detect by.
        let fork_state = tmp.path().join("agents/probe-fork/.jyc");
        std::fs::create_dir_all(&fork_state).unwrap();
        jyc_types::state_dir::register("probe-fork", &fork_state);
        tm.set_topic_path("probe-fork", parent_dir.clone())
            .await
            .unwrap();

        let outcome = tm
            .purge_topic_dir_under(
                "probe-fork",
                &tmp.path().join("agents"),
                &tmp.path().join("state"),
            )
            .await
            .unwrap();
        let PurgeOutcome::Refused(reason) = outcome else {
            panic!("the parent's workspace dir must be refused, got {outcome:?}");
        };
        assert!(reason.contains("probe-parent"), "{reason}");
        assert!(parent_dir.exists());
        jyc_types::state_dir::unregister("probe-fork");
    }

    /// A direct child of an agents/state root is a state namespace
    /// (`agents/jin/.jyc` is another topic's data), not a topic dir.
    #[tokio::test]
    async fn purge_refuses_a_state_namespace() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let agents_dir = tmp.path().join("agents");
        let namespace = agents_dir.join("jin");
        std::fs::create_dir_all(namespace.join(".jyc")).unwrap();
        tm.set_topic_path("probe-ns", namespace.clone())
            .await
            .unwrap();

        let outcome = tm
            .purge_topic_dir_under("probe-ns", &agents_dir, &tmp.path().join("state"))
            .await
            .unwrap();
        let PurgeOutcome::Refused(reason) = outcome else {
            panic!("a state namespace must be refused, got {outcome:?}");
        };
        assert!(reason.contains("state namespace"), "{reason}");
        assert!(namespace.exists());
        jyc_types::state_dir::unregister("probe-ns");
    }

    /// jyc's own directories are not topic dirs, however a topic got pinned to
    /// one: the agents tree has to survive, and so does the state root.
    #[tokio::test]
    async fn purge_refuses_jycs_own_directories() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let agents_dir = tmp.path().join("agents");
        let data_dir = tmp.path().join("state");
        for (name, pin) in [
            ("probe-agents-root", agents_dir.clone()),
            ("probe-data-home", tmp.path().to_path_buf()),
        ] {
            std::fs::create_dir_all(&pin).unwrap();
            tm.set_topic_path(name, pin.clone()).await.unwrap();

            let outcome = tm
                .purge_topic_dir_under(name, &agents_dir, &data_dir)
                .await
                .unwrap();
            assert!(
                matches!(outcome, PurgeOutcome::Refused(_)),
                "{name} pins {} which holds jyc's own dirs, got {outcome:?}",
                pin.display()
            );
            assert!(pin.exists(), "{name}: nothing may be deleted on a refusal");
        }
    }

    /// A topic that was never pinned keeps its old behavior — the dir lives in
    /// the workspace and `close_topic` deletes it, so purge has nothing to add.
    #[tokio::test]
    async fn purge_leaves_an_unpinned_topic_to_close() {
        let tmp = tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let tm = make_tm(&workspace);

        let topic_dir = workspace.join("probe-plain");
        std::fs::create_dir_all(&topic_dir).unwrap();

        let outcome = tm
            .purge_topic_dir_under(
                "probe-plain",
                &tmp.path().join("agents"),
                &tmp.path().join("state"),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, PurgeOutcome::Nothing), "{outcome:?}");
        assert!(topic_dir.exists(), "purge itself must not guess a dir");
    }

    /// The agents root itself must never match (guard against catastrophic
    /// deletion of the whole tree).
    #[tokio::test]
    async fn path_is_under_rejects_root_itself() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("agents");
        std::fs::create_dir_all(&root).unwrap();
        assert!(!path_is_under(&root, &root).await);
        assert!(path_is_under(&root.join("a").join("t"), &root).await);
        assert!(!path_is_under(&tmp.path().join("other"), &root).await);
    }
}
