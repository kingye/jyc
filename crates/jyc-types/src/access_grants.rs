//! Runtime filesystem access grants (`/grant`, `/ungrant`).
//!
//! Grants extend a topic agent's filesystem boundary at runtime without
//! touching `config.toml`. The registry is process-global, keyed by
//! **topic name**, and consulted by the access-root resolvers in
//! `jyc-agent/src/service/prompt.rs` when each turn builds its tool
//! context — so a grant takes effect on the next turn and disappears on
//! process restart ("temporary"). Persisting across restarts is a separate
//! concern handled by `/grant -p`, which also writes the path into the
//! topic's `[agents.<name>] access` config section.
//!
//! One kind is rebuilt instead of persisted: the grant a spawn records for a
//! child's parent, whose durable half is the child's `spawned-from` breadcrumb.
//! `topic_path::restore_state_registry` rebuilds it at serve startup, so a
//! relaunched parent still finds — and can still reach — the topics it spawned.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

/// One granted path. `write = false` means read-only (the `/grant`
/// default); write access always implies read, matching the resolver
/// convention that `access.write` paths are also readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// Absolute, tilde-expanded path (normalized by the command handler).
    pub path: PathBuf,
    /// Whether write access is granted.
    pub write: bool,
    /// The subagent this grant was made for, when a spawn made it.
    ///
    /// A granted path cannot name a child: a shared `topic_path` is the
    /// parent's own dir, and one parent can have several children working in
    /// it. So a spawn records the name here, and the reader it exists for
    /// (`spawn_handler::subagents_of`) takes the child's identity from the
    /// grant rather than from the last path component. `/grant` makes access
    /// grants, not lineage ones, and names nobody.
    pub subagent: Option<String>,
    /// Whether the grant covers one turn only and is then dropped.
    ///
    /// `/grant -once` is the "just this time" answer to a permission request:
    /// it is in force for the next turn that runs, and [`clear_once`] removes
    /// it when that turn ends, so a later turn that needs the path asks again.
    /// `false` for the ordinary until-restart grant — the two differ in how
    /// long the access lasts, not in what it allows.
    ///
    /// "The next turn that runs", not "the turn this command starts": a bare
    /// `/grant -once <path>` is a command-only message and runs no turn, so the
    /// grant is not dropped at the end of that message. It waits for the first
    /// turn that can actually use it and is dropped when that turn ends — the
    /// meaning the user asked for ("the next thing I do"). Dropping it earlier
    /// would make the grant vanish before any tool could read, which is worse
    /// than the wider lifetime it would be fixing.
    pub once: bool,
}

static REGISTRY: OnceLock<RwLock<HashMap<String, Vec<Grant>>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<String, Vec<Grant>>> {
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Grant `path` to `topic`. Idempotent: an existing grant for the same
/// path is upgraded to `write` if requested, otherwise left unchanged.
/// Returns `true` when the registry actually changed.
pub fn grant(topic: &str, path: PathBuf, write: bool) -> bool {
    let mut map = registry().write().unwrap();
    let grants = map.entry(topic.to_string()).or_default();
    match grants
        .iter_mut()
        // A one-turn grant is not this one: `/grant -once` followed by
        // `/grant` on the same path is a promotion to permanent, not a
        // duplicate.
        .find(|g| g.path == path && g.subagent.is_none() && !g.once)
    {
        Some(existing) => {
            if write && !existing.write {
                existing.write = true;
                true
            } else {
                false
            }
        }
        None => {
            grants.push(Grant {
                path,
                write,
                subagent: None,
                once: false,
            });
            true
        }
    }
}

/// Grant `path` to `parent` as the working dir of subagent `child`.
///
/// Same registry and same process-lifetime semantics as [`grant`]; the one
/// difference is that the entry carries the child's **name**, which is what
/// the subagent reader in `spawn_handler::subagents_of` lists, counts and
/// closes by. The path cannot stand in for it: a shared `topic_path` is the
/// parent's own dir, and two children may work there — they are two grants
/// here, not one. Always write: the parent seeds the child's directory.
pub fn grant_subagent(parent: &str, child: &str, path: PathBuf) -> bool {
    let mut map = registry().write().unwrap();
    let grants = map.entry(parent.to_string()).or_default();
    if grants
        .iter()
        .any(|g| g.path == path && g.subagent.as_deref() == Some(child))
    {
        return false;
    }
    grants.push(Grant {
        path,
        write: true,
        subagent: Some(child.to_string()),
        once: false,
    });
    true
}

/// Grant `path` to `topic` for the next turn that runs, and no longer.
///
/// Same registry and same lookup as [`grant`]; the entry carries
/// [`Grant::once`] and the turn loop drops it through [`clear_once`] when a
/// turn ends, so "just this time" cannot silently become the until-restart
/// grant the user did not ask for. See [`Grant::once`] for why a command-only
/// message does not consume the grant.
pub fn grant_once(topic: &str, path: PathBuf, write: bool) -> bool {
    let mut map = registry().write().unwrap();
    let grants = map.entry(topic.to_string()).or_default();
    match grants.iter_mut().find(|g| g.path == path && g.once) {
        Some(existing) => {
            if write && !existing.write {
                existing.write = true;
                true
            } else {
                false
            }
        }
        None => {
            grants.push(Grant {
                path,
                write,
                subagent: None,
                once: true,
            });
            true
        }
    }
}

/// The one-turn grants in force for `topic` right now.
///
/// Taken by the worker immediately before an AI turn starts and handed back to
/// [`clear_once`] when that turn ends. The pair is what makes "one turn" mean
/// one turn: of the grants that exist, only the ones already there when the
/// turn began are spent by it.
pub fn once_snapshot(topic: &str) -> Vec<(PathBuf, bool)> {
    let map = registry().read().unwrap();
    map.get(topic)
        .map(|grants| {
            grants
                .iter()
                .filter(|g| g.once)
                .map(|g| (g.path.clone(), g.write))
                .collect()
        })
        .unwrap_or_default()
}

/// Drop the one-turn grants in `snapshot`, returning how many were removed.
///
/// Called when an AI turn finishes, with the snapshot taken when it began:
/// that turn is what the user allowed, and leaving the entry behind would make
/// it ordinary access until restart.
///
/// Grants made *after* the snapshot are deliberately kept. A `/grant -once`
/// queued while a turn is already running is executed inside that turn, whose
/// tools were built before the grant existed and therefore cannot use it;
/// dropping it at that turn's end would spend the grant on a turn that never
/// read it. Keeping it costs at most one more turn of access and lets the grant
/// wait for a turn that can use it — the same rule a bare `/grant -once`
/// relies on, since a command-only message runs no turn and never reaches this
/// call (see [`Grant::once`]).
pub fn clear_once(topic: &str, snapshot: &[(PathBuf, bool)]) -> usize {
    if snapshot.is_empty() {
        return 0;
    }
    let mut map = registry().write().unwrap();
    let Some(grants) = map.get_mut(topic) else {
        return 0;
    };
    let before = grants.len();
    grants.retain(|g| {
        !(g.once
            && snapshot
                .iter()
                .any(|(path, write)| *path == g.path && *write == g.write))
    });
    let removed = before - grants.len();
    if grants.is_empty() {
        map.remove(topic);
    }
    removed
}

/// Revoke every grant for `path` on `topic` (read and write alike, a
/// subagent's included: `/ungrant <dir>` means "stop giving me this dir").
/// Returns `true` when something was removed.
pub fn ungrant(topic: &str, path: &std::path::Path) -> bool {
    let mut map = registry().write().unwrap();
    let Some(grants) = map.get_mut(topic) else {
        return false;
    };
    let before = grants.len();
    grants.retain(|g| g.path != path);
    let removed = grants.len() != before;
    if grants.is_empty() {
        map.remove(topic);
    }
    removed
}

/// Revoke the grant `parent` holds for subagent `child`, called when the
/// child closes.
///
/// By name, not by path: the child is what is being forgotten, and the dir it
/// worked in may be one its parent also holds a grant on (a shared
/// `topic_path`), or one another child works in.
pub fn ungrant_subagent(parent: &str, child: &str) -> bool {
    let mut map = registry().write().unwrap();
    let Some(grants) = map.get_mut(parent) else {
        return false;
    };
    let before = grants.len();
    grants.retain(|g| g.subagent.as_deref() != Some(child));
    let removed = grants.len() != before;
    if grants.is_empty() {
        map.remove(parent);
    }
    removed
}

/// Snapshot of all grants for `topic` (empty when none).
pub fn grants_for(topic: &str) -> Vec<Grant> {
    registry()
        .read()
        .unwrap()
        .get(topic)
        .cloned()
        .unwrap_or_default()
}

/// Remove every grant for `topic`. Used by tests.
pub fn clear(topic: &str) {
    registry().write().unwrap().remove(topic);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_ungrant_roundtrip() {
        let topic = "grant-test-roundtrip";
        clear(topic);
        assert!(grants_for(topic).is_empty());
        assert!(grant(topic, PathBuf::from("/a"), false));
        assert!(!grant(topic, PathBuf::from("/a"), false)); // duplicate: no change
        assert!(grant(topic, PathBuf::from("/a"), true)); // upgrade to write
        assert_eq!(
            grants_for(topic),
            vec![Grant {
                path: PathBuf::from("/a"),
                write: true,
                subagent: None,
                once: false,
            }]
        );
        assert!(!grant(topic, PathBuf::from("/a"), true)); // already write
        assert!(ungrant(topic, std::path::Path::new("/a")));
        assert!(!ungrant(topic, std::path::Path::new("/a")));
        assert!(grants_for(topic).is_empty());
        clear(topic);
    }

    /// Two children can work in one directory (a shared `topic_path` is the
    /// parent's own dir), so the name — not the path — is what tells them
    /// apart; a plain `/grant` on the same dir is a third, nameless entry.
    #[test]
    fn subagent_grants_are_named_and_independent_of_plain_grants() {
        let parent = "grant-test-parent";
        clear(parent);
        let dir = PathBuf::from("/shared");

        assert!(grant_subagent(parent, "child-a", dir.clone()));
        assert!(grant_subagent(parent, "child-b", dir.clone()));
        assert!(!grant_subagent(parent, "child-a", dir.clone())); // duplicate
        assert_eq!(grants_for(parent).len(), 2, "one entry per child");
        assert!(
            grants_for(parent).iter().all(|g| g.write),
            "a parent seeds its subagent's dir"
        );

        // The user's own grant on the same dir is its own entry, and revoking
        // the child leaves it alone.
        assert!(grant(parent, dir.clone(), false));
        assert_eq!(grants_for(parent).len(), 3);
        assert!(ungrant_subagent(parent, "child-a"));
        assert!(!ungrant_subagent(parent, "child-a"));
        assert_eq!(grants_for(parent).len(), 2);
        assert_eq!(
            grants_for(parent)
                .iter()
                .map(|g| g.subagent.clone())
                .collect::<Vec<_>>(),
            vec![Some("child-b".to_string()), None],
            "child-b and the user's own grant survive"
        );
        assert!(
            ungrant(parent, &dir),
            "…and /ungrant takes the rest of the dir"
        );
        assert!(grants_for(parent).is_empty());
        clear(parent);
    }

    #[test]
    fn grants_are_topic_scoped() {
        let (a, b) = ("grant-test-a", "grant-test-b");
        clear(a);
        clear(b);
        grant(a, PathBuf::from("/x"), false);
        assert_eq!(grants_for(a).len(), 1);
        assert!(grants_for(b).is_empty());
        clear(a);
    }

    /// The two answers to a permission request differ in duration, so they are
    /// two entries: `/grant -once` then `/grant` on the same dir must leave a
    /// permanent grant, not a one-turn entry that the next turn drops.
    #[test]
    fn once_grants_are_separate_entries_from_permanent_ones() {
        let topic = "grant-test-once";
        clear(topic);
        let dir = PathBuf::from("/data/invoices");

        assert!(grant_once(topic, dir.clone(), false));
        assert!(!grant_once(topic, dir.clone(), false), "duplicate");
        assert!(grant_once(topic, dir.clone(), true), "upgrade to write");
        assert_eq!(grants_for(topic).len(), 1);

        // The permanent grant is its own entry and implies write on its own.
        assert!(grant(topic, dir.clone(), false));
        assert_eq!(grants_for(topic).len(), 2);
        assert_eq!(
            grants_for(topic).iter().filter(|g| g.once).count(),
            1,
            "exactly one of the two is the one-turn grant"
        );

        // Ending the turn drops the one-turn entry and nothing else.
        let spent = once_snapshot(topic);
        assert_eq!(clear_once(topic, &spent), 1);
        assert_eq!(clear_once(topic, &spent), 0, "already dropped");
        assert_eq!(
            grants_for(topic),
            vec![Grant {
                path: dir,
                write: false,
                subagent: None,
                once: false,
            }],
            "the permanent grant survives the turn boundary"
        );
        clear(topic);
    }

    /// A one-turn grant is dropped even when it is the only entry — the topic
    /// key must not be left behind empty.
    #[test]
    fn clear_once_removes_the_topic_when_nothing_remains() {
        let topic = "grant-test-once-only";
        clear(topic);
        assert!(grant_once(topic, PathBuf::from("/tmp-only"), false));
        let spent = once_snapshot(topic);
        assert_eq!(clear_once(topic, &spent), 1);
        assert!(grants_for(topic).is_empty());
        clear(topic);
    }

    /// A bare `/grant -once` runs no turn, so no snapshot ever holds it and it
    /// survives to be spent by the next turn that actually runs. This is the
    /// difference between "just this time" and "silently refused": dropping it
    /// before any tool could read it would turn an authorization into a denial.
    #[test]
    fn a_grant_made_after_the_snapshot_is_not_spent() {
        let topic = "grant-test-once-late";
        clear(topic);
        let dir = PathBuf::from("/data/late");

        // The turn began with nothing in force.
        let snapshot = once_snapshot(topic);
        assert!(snapshot.is_empty());

        // The grant arrives while that turn is running — or, equivalently, a
        // command-only message ran no turn at all. Either way no tools were
        // built against it, so it must not be counted as spent.
        assert!(grant_once(topic, dir.clone(), false));
        assert_eq!(clear_once(topic, &snapshot), 0, "nothing was spent");
        assert!(
            grants_for(topic).iter().any(|g| g.once && g.path == dir),
            "the grant is still in force for the next turn"
        );

        // That next turn spends it.
        let snapshot = once_snapshot(topic);
        assert_eq!(clear_once(topic, &snapshot), 1);
        assert!(grants_for(topic).is_empty());
        clear(topic);
    }
}
