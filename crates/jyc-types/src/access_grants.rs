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
//! `topic_path::adopt_state_dir` restores it on adoption, so a relaunched
//! parent still finds — and can still reach — the topics it spawned.

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
        .find(|g| g.path == path && g.subagent.is_none())
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
    });
    true
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
                subagent: None
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
}
