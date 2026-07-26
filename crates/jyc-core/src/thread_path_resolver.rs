//! Shared utility for resolving the on-disk path of a thread.
//!
//! Several inspect-server endpoints need to find a thread directory on
//! disk — `chat_history_*.jsonl` for history, `activity.jsonl` for activity,
//! session files for reset, etc. The lookup is non-trivial because a thread
//! may be:
//!
//! 1. A live thread in a registered channel's `ThreadManager` (with or without
//!    a custom `thread_path` override).
//! 2. An offline thread in another channel's workspace directory.
//! 3. An offline thread in a workspace directory with no registered
//!    `ThreadManager` (e.g., during testing).
//!
//! This module centralises the lookup so every endpoint uses the same
//! resolution order: try the named channel's `ThreadManager` first, then
//! fall back to scanning all known workspace directories.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::thread_manager::ThreadManager;

/// Resolve the on-disk path for a thread, looking in the named channel's
/// `ThreadManager` first and falling back to all known workspace dirs.
///
/// Returns `None` if the thread doesn't exist in any known location.
///
/// # Lookup order
///
/// 1. **`thread_managers`**: the named `channel`'s `ThreadManager`. Its own
///    `thread_path()` already handles the `<workspace>/<thread>` fallback
///    for live threads and any registered custom `thread_path` overrides.
/// 2. **`workspace_dirs`**: every workspace directory, looking for a
///    directory literally named `<thread_name>`.
///
/// This consolidates the lookup that was previously inlined in
/// `reset_session_impl` and `handle_thread_history` (and is similar to
/// `ThreadManager::thread_path` but spans all channels/workspaces).
pub async fn resolve_thread_path(
    thread_managers: &Arc<ArcSwap<Vec<Arc<ThreadManager>>>>,
    workspace_dirs: &Arc<ArcSwap<Vec<PathBuf>>>,
    channel: &str,
    thread_name: &str,
) -> Option<PathBuf> {
    // 1) Try the named channel's ThreadManager. Cloning the Arc drops the
    //    ArcSwap guard so we can `await thread_path` without holding the lock.
    {
        let tms = thread_managers.load();
        if let Some(tm) = tms.iter().find(|tm| tm.channel_name() == channel).cloned() {
            drop(tms);
            if let Some(path) = tm.thread_path(thread_name).await {
                return Some(path);
            }
        }
    }

    // 2) Fall back to scanning all known workspace directories. This
    //    covers threads in channels without a registered ThreadManager
    //    (e.g., during testing) and threads whose channel was removed
    //    but whose directory still exists on disk.
    let dirs = workspace_dirs.load();
    dirs.iter().find_map(|d| {
        let p = d.join(thread_name);
        if p.exists() { Some(p) } else { None }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    fn empty_thread_managers() -> Arc<ArcSwap<Vec<Arc<ThreadManager>>>> {
        Arc::new(ArcSwap::from_pointee(vec![]))
    }

    fn empty_workspace_dirs() -> Arc<ArcSwap<Vec<PathBuf>>> {
        Arc::new(ArcSwap::from_pointee(vec![]))
    }

    #[tokio::test]
    async fn returns_none_when_thread_managers_empty_and_no_workspaces() {
        let result = resolve_thread_path(
            &empty_thread_managers(),
            &empty_workspace_dirs(),
            "emf",
            "issue-42",
        )
        .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn finds_thread_in_workspace_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let thread_dir = tmp.path().join("issue-42");
        std::fs::create_dir(&thread_dir).unwrap();

        let dirs: Arc<ArcSwap<Vec<PathBuf>>> =
            Arc::new(ArcSwap::from_pointee(vec![tmp.path().to_path_buf()]));

        let result = resolve_thread_path(&empty_thread_managers(), &dirs, "emf", "issue-42").await;
        assert_eq!(result, Some(thread_dir));
    }

    #[tokio::test]
    async fn returns_none_when_thread_not_in_any_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs: Arc<ArcSwap<Vec<PathBuf>>> =
            Arc::new(ArcSwap::from_pointee(vec![tmp.path().to_path_buf()]));

        let result =
            resolve_thread_path(&empty_thread_managers(), &dirs, "emf", "nonexistent").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn scan_uses_first_workspace_with_match() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp1.path().join("shared")).unwrap();
        std::fs::create_dir(tmp2.path().join("shared")).unwrap();

        let dirs: Arc<ArcSwap<Vec<PathBuf>>> = Arc::new(ArcSwap::from_pointee(vec![
            tmp1.path().to_path_buf(),
            tmp2.path().to_path_buf(),
        ]));

        let result = resolve_thread_path(&empty_thread_managers(), &dirs, "any", "shared").await;
        assert_eq!(result, Some(tmp1.path().join("shared")));
    }

    // Suppress dead_code warning for the unused HashMap import; it's a
    // useful reminder of the type that ThreadManager internally uses.
    #[allow(dead_code)]
    fn _suppress_unused() -> HashMap<String, PathBuf> {
        HashMap::new()
    }

    // Suppress unused import warning for Path (kept for future tests)
    #[allow(dead_code)]
    fn _path_marker(_p: &Path) {}
}
