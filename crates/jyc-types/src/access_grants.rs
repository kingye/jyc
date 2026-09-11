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
    match grants.iter_mut().find(|g| g.path == path) {
        Some(existing) => {
            if write && !existing.write {
                existing.write = true;
                true
            } else {
                false
            }
        }
        None => {
            grants.push(Grant { path, write });
            true
        }
    }
}

/// Revoke every grant for `path` on `topic` (read and write alike).
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
                write: true
            }]
        );
        assert!(!grant(topic, PathBuf::from("/a"), true)); // already write
        assert!(ungrant(topic, std::path::Path::new("/a")));
        assert!(!ungrant(topic, std::path::Path::new("/a")));
        assert!(grants_for(topic).is_empty());
        clear(topic);
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
