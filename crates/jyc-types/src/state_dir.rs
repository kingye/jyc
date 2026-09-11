//! Topic state-directory resolution.
//!
//! Each topic keeps its runtime state in a `.jyc` directory. Historically
//! that lived inside the topic's working directory (`<topic_dir>/.jyc`),
//! which pollutes pinned repos and (worse) shares identity with the folder
//! name. This module decouples them: a globally-unique **state dir**
//! (`<agents_root>/<name>/.jyc`) can be registered for a topic *name*, and
//! all state access resolves through [`jyc_dir`].
//!
//! Naming rules for state dirs:
//! - Config-key agents (`[agents.<key>]` pins) → `<agents_root>/<key>/.jyc`.
//!   Config keys are unique by construction, so no collisions are possible.
//! - Ad-hoc/pinned dirs without a config key → `<agents_root>/<derived>/.jyc`
//!   where `derived` encodes the absolute path (see [`derive_state_name`]).
//! - Everything else (generated workspace dirs, dynamic topics) stays
//!   `<topic_dir>/.jyc` via the fallback.
//!
//! The registry is process-global, keyed by **topic name** — the identity
//! that actually distinguishes topics — because several agents may pin the
//! *same* directory (`topic_path`) while each keeping its own state dir.
//! It is populated at startup (config pins + breadcrumb scan) and at
//! runtime pin creation. Lookups fall back to `<topic_dir>/.jyc`, so code
//! paths that never registered a topic behave exactly as before.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{OnceLock, RwLock};

static REGISTRY: OnceLock<RwLock<HashMap<String, PathBuf>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<String, PathBuf>> {
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Lexically normalize a path used as a registry key: drop `.` components
/// and trailing separators. Does NOT resolve `..` or symlinks — topic dirs
/// are expected to be absolute and already tilde-expanded.
fn normalize(dir: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in dir.components() {
        if !matches!(c, Component::CurDir) {
            out.push(c.as_os_str());
        }
    }
    out
}

/// Derive the globally-unique state-dir name for an absolute topic dir.
///
/// `_` is escaped to `__` first, then path separators (and Windows drive
/// colons) become `_`. The leading `/` yields a leading `_`, which keeps
/// derived names out of the config-key namespace (config keys starting
/// with `_` are reserved). Underscores near separator boundaries are
/// unambiguous (`/a/b_c` -> `_a_b__c` differs from `/a_b/c` -> `_a__b_c`);
/// the only residual collision is a path component ending in `_` directly
/// adjacent to one starting with `_` across a separator (`/a_/b` and
/// `/a/_b` both -> `_a___b`), which would make two such sibling pins share
/// one state dir:
///
/// ```text
/// /home/jiny/projects/test  -> _home_jiny_projects_test
/// /home/jiny/my_projects/x  -> _home_jiny_my__projects_x
/// ```
pub fn derive_state_name(topic_dir: &Path) -> String {
    let s = topic_dir.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    // Escape first, then substitute separators: every `_` in the output came
    // from either a doubled `__` or a `/`/`:` — drive names still read
    // cleanly (`C:\x` -> `C_x`). The one residual collision is a component
    // ending in `_` adjacent to a separator (`/a_/b` vs `/a/_b` both give
    // `_a___b`); such sibling pins would share one state dir.
    let s = s.replace('_', "__");
    let s = s.replace(':', "_");
    s.replace('/', "_")
}

/// Register `state_dir` as the `.jyc` location for topic `topic_name`.
///
/// Idempotent; last registration for a *name* wins. Different names may
/// resolve to different state dirs even when their topic dirs coincide
/// (multiple agents pinning one shared directory).
pub fn register(topic_name: &str, state_dir: &Path) {
    let mut map = registry().write().unwrap_or_else(|e| e.into_inner());
    map.insert(topic_name.to_string(), normalize(state_dir));
}

/// Return the registered state dir for `topic_name`, if any.
pub fn registered_state(topic_name: &str) -> Option<PathBuf> {
    let map = registry().read().unwrap_or_else(|e| e.into_inner());
    map.get(topic_name).cloned()
}

/// Snapshot of every registered `(topic_name, state_dir)` pair.
///
/// Used by cross-topic aggregation (e.g. `/bill`) to find state dirs
/// that live outside `data_home` (pinned topics).
pub fn registered_topics() -> Vec<(String, PathBuf)> {
    let map = registry().read().unwrap_or_else(|e| e.into_inner());
    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Remove the registration for `topic_name` (used when a topic's state is
/// destroyed, e.g. `/close` on a pinned topic). No-op if unregistered.
pub fn unregister(topic_name: &str) {
    let mut map = registry().write().unwrap_or_else(|e| e.into_inner());
    map.remove(topic_name);
}

/// Resolve the `.jyc` directory for a topic.
///
/// Returns the state dir registered for `topic_name` if one exists,
/// otherwise the legacy `topic_dir/.jyc` fallback (workspace topics).
pub fn jyc_dir(topic_name: &str, topic_dir: impl AsRef<Path>) -> PathBuf {
    let topic_dir = topic_dir.as_ref();
    let map = registry().read().unwrap_or_else(|e| e.into_inner());
    map.get(topic_name)
        .cloned()
        .unwrap_or_else(|| topic_dir.join(".jyc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn derive_state_name_basic() {
        assert_eq!(
            derive_state_name(Path::new("/home/jiny/projects/test")),
            "_home_jiny_projects_test"
        );
    }

    #[test]
    fn derive_state_name_distinguishes_underscores_from_separators() {
        assert_eq!(derive_state_name(Path::new("/a/b_c")), "_a_b__c", "/a/b_c");
        assert_eq!(derive_state_name(Path::new("/a_b/c")), "_a__b_c", "/a_b/c");
    }

    #[test]
    fn derive_state_name_strips_trailing_slash_and_drives() {
        assert_eq!(
            derive_state_name(Path::new("/a/b/")),
            "_a_b",
            "trailing slash"
        );
        assert_eq!(
            derive_state_name(Path::new("C:\\Users\\x\\proj")),
            "C__Users_x_proj"
        );
    }

    #[test]
    fn jyc_dir_falls_back_when_unregistered() {
        let tmp = tempdir().unwrap();
        // `probe-` prefix keeps this key unique across parallel tests.
        let dir = tmp.path().join("probe-fallback");
        assert_eq!(jyc_dir("probe-fallback-name", &dir), dir.join(".jyc"));
    }

    #[test]
    fn register_redirects_lookup() {
        let tmp = tempdir().unwrap();
        let topic = tmp.path().join("probe-reg-topic");
        let state = tmp.path().join("probe-reg-state");
        register("probe-reg-name", &state);
        assert_eq!(jyc_dir("probe-reg-name", &topic), state);
        assert_eq!(registered_state("probe-reg-name"), Some(state));
    }

    #[test]
    fn co_pinned_topics_get_isolated_state() {
        // Two topic names pinned to the SAME dir must resolve to two
        // different state dirs — the whole point of name keying.
        let tmp = tempdir().unwrap();
        let shared = tmp.path().join("probe-shared-repo");
        let a = tmp.path().join("probe-state-a");
        let b = tmp.path().join("probe-state-b");
        register("probe-a", &a);
        register("probe-b", &b);
        assert_eq!(jyc_dir("probe-a", &shared), a);
        assert_eq!(jyc_dir("probe-b", &shared), b);
    }

    #[test]
    fn unregister_clears_lookup() {
        let tmp = tempdir().unwrap();
        let topic = tmp.path().join("probe-unreg-topic");
        let state = tmp.path().join("probe-unreg-state");
        register("probe-unreg-name", &state);
        unregister("probe-unreg-name");
        assert!(registered_state("probe-unreg-name").is_none());
        assert_eq!(jyc_dir("probe-unreg-name", &topic), topic.join(".jyc"));
        // unregistering twice is a no-op
        unregister("probe-unreg-name");
    }

    #[test]
    fn normalize_absorbs_dot_components() {
        let tmp = tempdir().unwrap();
        let topic = tmp.path().join("probe-dot-topic");
        let state = tmp.path().join("probe-dot-state");
        register("probe-dot-name", &state.join(".").join(""));
        // Registered value is normalized; lookups return the clean path.
        assert_eq!(jyc_dir("probe-dot-name", &topic), state);
    }
}
