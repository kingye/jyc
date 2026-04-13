//! Central thread path resolution.
//!
//! The thread directory follows the convention:
//!   `<workdir>/<channel>/workspace/<thread_name>/`
//!
//! This module provides a single source of truth for resolving thread paths,
//! preventing the double-nesting bugs that occur when path segments are
//! added multiple times in different modules.

use std::path::{Path, PathBuf};

/// Resolve the full thread directory path.
///
/// Convention: `<workdir>/<channel>/workspace/<thread_name>/`
///
/// - `workdir`: the jyc data root (e.g., `/home/user/jyc-data`)
/// - `channel`: the channel config name (e.g., `jiny283a`, `feishu_bot`)
/// - `thread_name`: the thread name (e.g., `invoice-processing`, `self-hosting-jyc`)
pub fn resolve_thread_path(workdir: &Path, channel: &str, thread_name: &str) -> PathBuf {
    workdir.join(channel).join("workspace").join(thread_name)
}

/// Resolve the workspace directory for a channel.
///
/// Convention: `<workdir>/<channel>/workspace/`
pub fn resolve_workspace(workdir: &Path, channel: &str) -> PathBuf {
    workdir.join(channel).join("workspace")
}

/// Resolve the attachments directory for a thread.
///
/// Convention: `<thread_path>/attachments/`
pub fn resolve_attachments_dir(thread_path: &Path) -> PathBuf {
    thread_path.join("attachments")
}

/// Resolve the messages directory for a thread.
///
/// Convention: `<thread_path>/messages/`
pub fn resolve_messages_dir(thread_path: &Path) -> PathBuf {
    thread_path.join("messages")
}

/// Resolve the .jyc state directory for a thread.
///
/// Convention: `<thread_path>/.jyc/`
pub fn resolve_jyc_dir(thread_path: &Path) -> PathBuf {
    thread_path.join(".jyc")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_thread_path() {
        let path = resolve_thread_path(
            Path::new("/home/user/jyc-data"),
            "jiny283a",
            "invoice-processing",
        );
        assert_eq!(
            path,
            PathBuf::from("/home/user/jyc-data/jiny283a/workspace/invoice-processing")
        );
    }

    #[test]
    fn test_resolve_workspace() {
        let path = resolve_workspace(Path::new("/home/user/jyc-data"), "feishu_bot");
        assert_eq!(
            path,
            PathBuf::from("/home/user/jyc-data/feishu_bot/workspace")
        );
    }

    #[test]
    fn test_resolve_attachments_dir() {
        let thread = PathBuf::from("/data/jiny283/workspace/invoices");
        assert_eq!(
            resolve_attachments_dir(&thread),
            PathBuf::from("/data/jiny283/workspace/invoices/attachments")
        );
    }

    #[test]
    fn test_resolve_jyc_dir() {
        let thread = PathBuf::from("/data/channel/workspace/thread");
        assert_eq!(
            resolve_jyc_dir(&thread),
            PathBuf::from("/data/channel/workspace/thread/.jyc")
        );
    }
}
